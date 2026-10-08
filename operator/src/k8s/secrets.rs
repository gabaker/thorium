use k8s_openapi::{ByteString, api::core::v1::Secret};
use kube::Api;
use kube::api::{DeleteParams, ObjectMeta, Patch, PatchParams, PostParams};
use rand::Rng;
use rand::distr::Alphanumeric;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use thorium::{Conf, Error};

use super::clusters::{ClusterMeta, get_secret_bytes};

/// Create or update a Secret
///
/// This creates or optionally updates a kubernetes secret if it exists. Returns whether the
/// Secret was written, which is false when it already exists and `update` is false.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `secret` - Secret object to update in the kubernetes API
/// * `update` - Should the operator update this secret if it exists
pub async fn create_or_update(
    meta: &ClusterMeta,
    secret: &Secret,
    update: bool,
) -> Result<bool, Error> {
    // get the name of this secret for logging and patching
    let Some(name) = secret.metadata.name.as_deref() else {
        return Err(Error::new("Cannot create a Secret without a name"));
    };
    // first attempt to create the Secret
    let params = PostParams::default();
    match meta.secret_api.create(&params, secret).await {
        Ok(_) => {
            println!("Created {name} secret in namespace {}", meta.namespace);
            Ok(true)
        }
        // an existing secret is patched when updates are allowed and left alone otherwise
        Err(kube::Error::Api(error)) if error.reason == "AlreadyExists" => {
            if !update {
                println!(
                    "Warning: secret {name} in namespace {} already exists",
                    meta.namespace
                );
                return Ok(false);
            }
            // overwrite the keys and labels we manage, leaving any others in place
            let mut patch = serde_json::json!({
                "data": secret.data
            });
            if let Some(labels) = &secret.metadata.labels {
                patch["metadata"] = serde_json::json!({"labels": labels});
            }
            let patch = Patch::Merge(&patch);
            let params: PatchParams = PatchParams::default();
            match meta.secret_api.patch(name, &params, &patch).await {
                Ok(_) => {
                    println!("Patched {name} secret in namespace {}", meta.namespace);
                    Ok(true)
                }
                Err(error) => Err(Error::new(format!(
                    "Failed to patch {name} secret: {error}"
                ))),
            }
        }
        Err(error) => Err(Error::new(format!(
            "Failed to create {name} secret: {error}"
        ))),
    }
}

/// Build a Secret object
///
/// # Arguments
///
/// * `secret` - JSON string secret data
/// * `name` - Name of secret being created
/// * `key` - Key within secret to place secret data
/// * `namespace` - Namespace to create secret within
pub fn build_secret(secret: &str, name: &str, key: &str, namespace: &str) -> Secret {
    // store our data under its key
    let data = BTreeMap::from([(key.to_owned(), ByteString(secret.as_bytes().to_vec()))]);
    Secret {
        metadata: ObjectMeta {
            name: Some(name.to_owned()),
            namespace: Some(namespace.to_owned()),
            ..Default::default()
        },
        data: Some(data),
        ..Default::default()
    }
}

/// The Secrets the operator writes or deletes, which config and bootstrap Secrets can't use
///
/// Any name ending in `-pass` is reserved for user passwords as well. The chart's own pull
/// secret is listed so the operator never reads a config from it.
pub const RESERVED_SECRET_NAMES: &[&str] = &[
    "thorium",
    "keys",
    "keys-kaboom",
    "docker-skopeo",
    crate::k8s::crds::REGISTRY_TOKEN_SECRET,
    "thorium-image-pull",
];

/// Check whether a Secret name is reserved for a Secret the operator or chart owns
///
/// # Arguments
///
/// * `name` - The name of the Secret to check
pub fn is_reserved_secret_name(name: &str) -> bool {
    // user password secrets are named <username>-pass
    RESERVED_SECRET_NAMES.contains(&name) || name.ends_with("-pass")
}

/// A rendered thorium.yml and its hash
pub struct RenderedConfig {
    /// The thorium.yml document the components mount
    pub yaml: String,
    /// The hex encoded sha256 of `yaml`
    pub hash: String,
}

/// Render the thorium.yml config for a cluster without writing it anywhere
///
/// # Arguments
///
/// * `conf` - The merged Thorium config to render
pub fn render_thorium_config(conf: &Conf) -> Result<RenderedConfig, Error> {
    // convert the merged config to JSON first so enums serialize as maps; serializing the
    // typed config directly emits YAML tags (e.g. `!Grpc`) that the config loader rejects
    let conf_json = serde_json::to_value(conf)?;
    // render the config as YAML
    let yaml = serde_norway::to_string(&conf_json)?;
    // hash the rendered config so components can roll out when it changes
    let hash = format!("{:x}", Sha256::digest(yaml.as_bytes()));
    Ok(RenderedConfig { yaml, hash })
}

/// Write a rendered thorium.yml to the `thorium` Secret the components mount
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `rendered` - The rendered config to write
pub async fn write_thorium_config(
    meta: &ClusterMeta,
    rendered: &RenderedConfig,
) -> Result<(), Error> {
    // build thorium config secret template
    let thorium_secret = build_secret(&rendered.yaml, "thorium", "thorium.yml", &meta.namespace);
    // create or update the thorium config secret in k8s
    create_or_update(meta, &thorium_secret, true).await?;
    Ok(())
}

/// Read one key of a user Secret a component mounts so its content can be hashed
///
/// A missing Secret or key gives a fixed marker instead of failing, since the component's
/// pods report the missing mount themselves and the marker still changes the hash once the
/// Secret appears.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `name` - The name of the Secret
/// * `key` - The key in the Secret the component mounts
pub async fn mounted_content(meta: &ClusterMeta, name: &str, key: &str) -> Result<Vec<u8>, Error> {
    // get this Secret if it exists
    let secret = meta.secret_api.get_opt(name).await.map_err(|error| {
        Error::new(format!(
            "Failed to get {name} secret in {}: {error}",
            meta.namespace
        ))
    })?;
    // take the mounted key out of it or mark it as absent
    Ok(secret
        .and_then(|secret| secret.data)
        .and_then(|mut data| data.remove(key))
        .map_or_else(
            || format!("absent:{name}/{key}").into_bytes(),
            |bytes| bytes.0,
        ))
}

/// Create a keys.yml secret for a user
///
/// Returns the rendered keys.yml so callers can hash what the components mount.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `username` - Name of user
/// * `password` - Password for user
/// * `secret_name` - Optional name of secret
pub async fn create_keys(
    meta: &ClusterMeta,
    username: &str,
    password: &str,
    secret_name: Option<&str>,
) -> Result<String, Error> {
    // render the keys.yml for this user, leaving the cluster domain to the pod's DNS search
    // path so clusters with a domain other than cluster.local work
    let template = serde_json::json!({
        "api": format!("http://thorium-api.{}.svc", meta.namespace),
        "username": username,
        "password": password
    })
    .to_string();
    // use the default keys secret name unless another was given
    let name = secret_name.unwrap_or("keys");
    // build a password secret
    let secret = build_secret(template.as_ref(), name, "keys.yml", &meta.namespace);
    // actually create the secret in k8s
    create_or_update(meta, &secret, true).await?;
    Ok(template)
}

/// Create a user password secret
///
/// User password secrets are created when creating a new Thorium user. This helps
/// the operator track the authentication information for its admin user account as
/// well as any additional users needed for Thorium operation. If the thorium-operator
/// account password secret is lost, external action will need to be taken to delete
/// that user account.
///
/// # Arguments
///
/// * `username` - Name of the Thorium user
/// * `meta` - Thorium cluster client and metadata
pub async fn create_user_secret(
    username: &str,
    meta: &ClusterMeta,
) -> Result<Option<String>, Error> {
    // generate alphanumeric password for operator
    let password: String = rand::rng()
        .sample_iter(&Alphanumeric)
        .take(32)
        .map(char::from)
        .collect();
    // user password secrets are names based on the username
    let secret_name = format!("{username}-pass");
    // build a password secret
    let secret = build_secret(
        password.as_ref(),
        secret_name.as_ref(),
        username,
        &meta.namespace,
    );
    // create the secret but do not force update
    if !create_or_update(meta, &secret, false).await? {
        return Ok(None);
    }
    Ok(Some(password))
}

/// Get kubernetes secret by name
///
/// # Arguments
///
/// * `secret_api` - API for interacting with kubernetes secrets
/// * `secret_name` - Name of secret to retrieve
pub async fn get_secret(
    secret_api: &Api<Secret>,
    secret_name: &str,
) -> Result<Option<Secret>, Error> {
    // a missing secret is returned as None rather than an error
    secret_api
        .get_opt(secret_name)
        .await
        .map_err(|error| Error::new(format!("Failed to get {secret_name} secret: {error}")))
}

/// Read a single key from a Secret as a utf8 string
///
/// A missing Secret or key reports a `NOT_FOUND` status.
///
/// # Arguments
///
/// * `api` - The Secret API for the Secret's namespace
/// * `kind` - What the Secret holds (e.g. "bootstrap admin") for errors
/// * `name` - The name of the Secret
/// * `key` - The key to read from the Secret
pub async fn get_secret_key(
    api: &Api<Secret>,
    kind: &str,
    name: &str,
    key: &str,
) -> Result<String, Error> {
    // get the raw bytes of this key
    let raw = get_secret_bytes(api, kind, name, key).await?;
    // decode this key as utf8
    String::from_utf8(raw)
        .map_err(|_| Error::new(format!("Secret {name} key {key} is not valid utf8")))
}

/// Retrieve password from a user k8s secret
///
/// Returns None when the Secret doesn't exist or has no key for this user.
///
/// # Arguments
///
/// * `username` - Name of the Thorium user
/// * `meta` - Thorium cluster client and metadata
pub async fn get_user_password(
    username: &str,
    meta: &ClusterMeta,
) -> Result<Option<String>, Error> {
    // user password secrets are named after their user
    let secret_name = format!("{username}-pass");
    let Some(secret) = get_secret(&meta.secret_api, &secret_name).await? else {
        return Ok(None);
    };
    // the password is stored under the username
    let Some(ByteString(raw)) = secret.data.and_then(|mut data| data.remove(username)) else {
        return Ok(None);
    };
    // decode the password as utf8
    String::from_utf8(raw).map(Some).map_err(|_| {
        Error::new(format!(
            "Secret {secret_name} key {username} is not valid utf8"
        ))
    })
}

/// The label marking a Secret the operator rendered from `registry_auth`
///
/// Only Secrets carrying it are deleted once `registry_auth` is unset, so a Secret with the
/// same name that an admin or the pre-Helm scripts created is never removed.
pub const REGISTRY_AUTH_LABEL: &str = "thorium.sandia.gov/registry-auth";

/// The Secrets the operator renders from `registry_auth`
const REGISTRY_AUTH_SECRETS: [&str; 2] = ["docker-skopeo", crate::k8s::crds::REGISTRY_TOKEN_SECRET];

/// Build a Secret rendered from `registry_auth`, labelled as the operator's own
///
/// # Arguments
///
/// * `template` - The docker config.json to store
/// * `name` - The name of the Secret
/// * `key` - The key to store the config under
/// * `namespace` - The namespace of the Secret
fn build_registry_secret(template: &str, name: &str, key: &str, namespace: &str) -> Secret {
    // build the secret and mark it as rendered from registry_auth
    let mut secret = build_secret(template, name, key, namespace);
    secret.metadata.labels = Some(BTreeMap::from([(
        REGISTRY_AUTH_LABEL.to_owned(),
        "true".to_owned(),
    )]));
    secret
}

/// Check whether a Secret was rendered by the operator from `registry_auth`
///
/// A Secret Helm manages is never the operator's, even if it carries the label.
///
/// # Arguments
///
/// * `secret` - The Secret to check
fn is_registry_auth_secret(secret: &Secret) -> bool {
    // get the Secret's labels
    let Some(labels) = &secret.metadata.labels else {
        return false;
    };
    // the operator's label must be set and Helm must not own the Secret
    labels.get(REGISTRY_AUTH_LABEL).map(String::as_str) == Some("true")
        && labels
            .get("app.kubernetes.io/managed-by")
            .map(String::as_str)
            != Some("Helm")
}

/// Delete the Secrets the operator rendered from `registry_auth` once it is unset
///
/// Secrets with these names that the operator didn't render (no [`REGISTRY_AUTH_LABEL`]) are
/// left alone, and a Secret that is already gone or replaced in between is skipped.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
async fn delete_registry_auth(meta: &ClusterMeta) -> Result<(), Error> {
    for name in REGISTRY_AUTH_SECRETS {
        // get the Secret to check who owns it
        let existing = meta
            .secret_api
            .get_opt(name)
            .await
            .map_err(|error| Error::new(format!("Failed to read {name} secret: {error}")))?;
        // leave a missing Secret or one the operator didn't render alone
        let Some(existing) = existing.filter(is_registry_auth_secret) else {
            continue;
        };
        // only delete the Secret that was checked, not one that replaced it in between
        let params = DeleteParams {
            preconditions: Some(kube::api::Preconditions {
                uid: existing.metadata.uid.clone(),
                resource_version: existing.metadata.resource_version.clone(),
            }),
            ..DeleteParams::default()
        };
        match meta.secret_api.delete(name, &params).await {
            Ok(_) => println!("Deleted {name} secret as registry_auth is unset"),
            // a Secret already gone or changed since it was checked is left for the next
            // reconcile
            Err(kube::Error::Api(error)) if error.code == 404 || error.code == 409 => {
                println!("Secret {name} changed or is gone, skipping deletion");
            }
            Err(error) => {
                return Err(Error::new(format!(
                    "Failed to delete {name} secret: {error}"
                )));
            }
        }
    }
    Ok(())
}

/// Create registry tokens secret from `ThoriumCluster` CRD
///
/// When `registry_auth` is unset, the Secrets the operator rendered from it earlier are
/// deleted instead.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
pub async fn create_or_update_registry_auth(meta: &ClusterMeta) -> Result<(), Error> {
    // check if registry auth tokens have been provided
    let Some(registries) = &meta.cluster.spec.registry_auth else {
        // remove the Secrets an earlier registry_auth rendered
        return delete_registry_auth(meta).await;
    };
    // build a docker config.json mapping each registry to its auth token
    let auth_map: BTreeMap<&String, BTreeMap<&str, &String>> = registries
        .iter()
        .map(|(url, token)| (url, BTreeMap::from([("auth", token)])))
        .collect();
    // build the registry auth secret data
    let template = serde_json::json!({"auths": auth_map}).to_string();
    // build the docker skopeo secret for scaler
    let skopeo_secret = build_registry_secret(
        template.as_ref(),
        "docker-skopeo",
        "config.json",
        &meta.namespace,
    );
    // create or update the skopeo secret
    println!("Creating skopeo registry secret");
    create_or_update(meta, &skopeo_secret, true).await?;
    // build a container pull secret
    let mut pull_secret = build_registry_secret(
        template.as_ref(),
        crate::k8s::crds::REGISTRY_TOKEN_SECRET,
        ".dockerconfigjson",
        &meta.namespace,
    );
    // pull secrets need the dockerconfigjson type instead of the default Opaque
    println!("Creating registry pull secret");
    pull_secret.type_ = Some("kubernetes.io/dockerconfigjson".to_string());
    // create the secret and force update
    create_or_update(meta, &pull_secret, true).await?;
    Ok(())
}

/// Cleanup Thorium secrets
///
/// The `thorium-operator-pass` Secret is kept since the operator's Thorium user outlives the
/// cluster, and a new `ThoriumCluster` on the same databases needs its password.
///
/// # Arguments
///
/// * `secret_api` - The Secret API for the `ThoriumCluster`'s namespace
pub async fn delete(secret_api: &Api<Secret>) -> Result<(), Error> {
    let params: DeleteParams = DeleteParams::default();
    // delete each secret the operator renders for the components
    let secrets_names = [
        "thorium",
        "keys",
        "keys-kaboom",
        "thorium-pass",
        "thorium-kaboom-pass",
        "docker-skopeo",
        crate::k8s::crds::REGISTRY_TOKEN_SECRET,
    ];
    for secret_name in secrets_names {
        match secret_api.delete(secret_name, &params).await {
            Ok(_) => println!("Deleted {secret_name} secret"),
            // a missing secret is already in the state we want
            Err(kube::Error::Api(error)) if error.code == 404 => {
                println!("Secret {secret_name} does not exist, skipping deletion");
            }
            Err(error) => {
                return Err(Error::new(format!(
                    "Failed to delete {secret_name} secret: {error}"
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::k8s::clusters::tests::{
        FakeKube, full_spec, kube_status, meta_for, namespaced_cluster, sample_conf,
    };

    /// The path Secrets are created at in the test namespace
    const SECRETS_PATH: &str = "/api/v1/namespaces/thorium/secrets";

    /// Rendering the same config twice gives the same YAML and hash
    #[test]
    fn config_hash_is_deterministic() {
        // render the same config twice from separately parsed copies
        let first = render_thorium_config(&sample_conf()).expect("config should render");
        let second = render_thorium_config(&sample_conf()).expect("config should render");
        // both renders match exactly
        assert_eq!(first.yaml, second.yaml);
        assert_eq!(first.hash, second.hash);
        // the hash is a hex sha256
        assert_eq!(first.hash.len(), 64);
        // the rendered config has no YAML tags the config loader would reject
        assert!(!first.yaml.contains('!'));
    }

    /// Changing the config changes its hash
    #[test]
    fn config_hash_tracks_changes() {
        // render a config and a copy with a different redis password
        let base = sample_conf();
        let mut changed = sample_conf();
        changed.redis.password = Some("rotated".to_owned());
        let base = render_thorium_config(&base).expect("config should render");
        let changed = render_thorium_config(&changed).expect("config should render");
        // the hashes differ
        assert_ne!(base.hash, changed.hash);
    }

    /// Operator-owned and password secret names are reserved
    #[test]
    fn reserved_names() {
        // every listed name is reserved
        for name in RESERVED_SECRET_NAMES {
            assert!(is_reserved_secret_name(name), "{name}");
        }
        // user password secrets are reserved
        assert!(is_reserved_secret_name("thorium-operator-pass"));
        // the chart's config and admin secrets are not
        assert!(!is_reserved_secret_name("thorium-config-secrets"));
        assert!(!is_reserved_secret_name("thorium-admin"));
    }

    /// The rendered thorium.yml loads through the same loader the components use
    #[test]
    fn rendered_config_loads_with_conf_loader() {
        // validate Elastic against a mounted CA so the enum config is rendered too
        let cluster = crate::k8s::clusters::tests::cluster_from_spec(serde_json::json!({
            "components": {"api": {}},
            "registry": "registry/thorium",
            "config": {},
            "elastic_ca_secret": {"name": "elastic-ca"}
        }));
        let mut conf = sample_conf();
        crate::k8s::clusters::apply_elastic_ca(&cluster, &mut conf);
        let rendered = render_thorium_config(&conf).expect("config should render");
        // write it where the loader can read it
        let path = std::env::temp_dir().join(format!(
            "thorium-operator-rendered-{}-{}.yml",
            std::process::id(),
            rendered.hash
        ));
        std::fs::write(&path, &rendered.yaml).expect("write rendered config");
        let loaded = Conf::new(&path);
        std::fs::remove_file(&path).expect("remove rendered config");
        // the loaded config matches the one that was rendered
        assert_eq!(loaded.expect("rendered config should load"), conf);
    }

    /// A built Secret holds its data under its key in its namespace
    #[test]
    fn build_secret_shape() {
        // build a secret
        let secret = build_secret("data", "thorium", "thorium.yml", "ns");
        assert_eq!(secret.metadata.name.as_deref(), Some("thorium"));
        assert_eq!(secret.metadata.namespace.as_deref(), Some("ns"));
        assert_eq!(
            secret.data.expect("data")["thorium.yml"],
            ByteString(b"data".to_vec())
        );
    }

    /// The thorium.yml Secret is created, or has its data patched when it already exists
    #[tokio::test]
    async fn thorium_config_is_created_or_patched() {
        // render a config for a cluster whose Secret doesn't exist yet
        let rendered = render_thorium_config(&sample_conf()).expect("config should render");
        let fake = FakeKube::default().route("POST", SECRETS_PATH, 201, serde_json::json!({}));
        let meta = meta_for(namespaced_cluster(full_spec()), &fake.client());
        write_thorium_config(&meta, &rendered)
            .await
            .expect("create");
        // the Secret is created with the rendered config
        let created = &fake.writes()[0];
        assert_eq!(created.body["metadata"]["name"], "thorium");
        assert!(created.body["data"]["thorium.yml"].is_string());
        // an existing Secret has only its data patched
        let fake = FakeKube::default()
            .route("POST", SECRETS_PATH, 409, kube_status(409, "AlreadyExists"))
            .route(
                "PATCH",
                &format!("{SECRETS_PATH}/thorium"),
                200,
                serde_json::json!({}),
            );
        let meta = meta_for(namespaced_cluster(full_spec()), &fake.client());
        write_thorium_config(&meta, &rendered).await.expect("patch");
        let patch = &fake.writes()[1];
        assert_eq!(patch.method, "PATCH");
        assert!(patch.body.get("metadata").is_none());
        assert!(patch.body["data"]["thorium.yml"].is_string());
    }

    /// An existing user password is never overwritten
    #[tokio::test]
    async fn user_secret_is_not_overwritten() {
        // the password Secret already exists
        let fake =
            FakeKube::default().route("POST", SECRETS_PATH, 409, kube_status(409, "AlreadyExists"));
        let meta = meta_for(namespaced_cluster(full_spec()), &fake.client());
        // no new password is returned and nothing is patched
        assert_eq!(
            create_user_secret("thorium-operator", &meta)
                .await
                .expect("create"),
            None
        );
        assert_eq!(fake.writes().len(), 1);
        // a new Secret returns its generated password
        let fake = FakeKube::default().route("POST", SECRETS_PATH, 201, serde_json::json!({}));
        let meta = meta_for(namespaced_cluster(full_spec()), &fake.client());
        let password = create_user_secret("thorium-operator", &meta)
            .await
            .expect("create")
            .expect("password");
        assert_eq!(password.len(), 32);
        assert_eq!(
            fake.writes()[0].body["metadata"]["name"],
            "thorium-operator-pass"
        );
    }

    /// A user's password is read from its Secret and missing ones are None
    #[tokio::test]
    async fn user_password_lookup() {
        // a Secret holding the password under the username and one with invalid utf8
        let fake = FakeKube::default()
            .route(
                "GET",
                &format!("{SECRETS_PATH}/alice-pass"),
                200,
                serde_json::json!({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "alice-pass"}, "data": {"alice": "aHVudGVyMg=="}}),
            )
            .route(
                "GET",
                &format!("{SECRETS_PATH}/bob-pass"),
                200,
                serde_json::json!({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "bob-pass"}, "data": {"bob": "/w=="}}),
            );
        let meta = meta_for(namespaced_cluster(full_spec()), &fake.client());
        // the stored password is returned
        assert_eq!(
            get_user_password("alice", &meta).await.expect("alice"),
            Some("hunter2".to_owned())
        );
        // a missing Secret is no password
        assert_eq!(
            get_user_password("carol", &meta).await.expect("carol"),
            None
        );
        // a password that isn't utf8 is an error naming the Secret
        let error = get_user_password("bob", &meta).await.expect_err("bob");
        assert!(error.to_string().contains("bob-pass"));
    }

    /// A missing mounted Secret hashes as a marker naming it
    #[tokio::test]
    async fn mounted_content_marks_absent_secrets() {
        // a namespace without the CA Secret
        let fake = FakeKube::default();
        let meta = meta_for(namespaced_cluster(full_spec()), &fake.client());
        // the content is a marker naming the Secret and key
        assert_eq!(
            mounted_content(&meta, "elastic-ca", "ca.crt")
                .await
                .expect("content"),
            b"absent:elastic-ca/ca.crt".to_vec()
        );
    }

    /// The keys.yml points the agent at the API in the cluster's namespace
    #[tokio::test]
    async fn keys_point_at_namespace_api() {
        // create the keys for a user
        let fake = FakeKube::default().route("POST", SECRETS_PATH, 201, serde_json::json!({}));
        let meta = meta_for(namespaced_cluster(full_spec()), &fake.client());
        let keys = create_keys(&meta, "thorium-operator", "pw", None)
            .await
            .expect("keys");
        // the keys name the in-cluster API and the user
        let keys: serde_json::Value = serde_json::from_str(&keys).expect("keys are JSON");
        assert_eq!(keys["api"], "http://thorium-api.thorium.svc");
        assert_eq!(keys["username"], "thorium-operator");
        // and are written to the default keys Secret
        assert_eq!(fake.writes()[0].body["metadata"]["name"], "keys");
    }

    /// Cleanup deletes every Secret the operator renders, including the registry pull secret,
    /// and keeps the operator's password
    #[tokio::test]
    async fn delete_removes_rendered_secrets() {
        // a namespace without any of the secrets, which cleanup treats as already deleted
        let fake = FakeKube::default();
        delete(&Api::namespaced(fake.client(), "thorium"))
            .await
            .expect("delete");
        // every rendered secret was deleted by name
        let deleted = fake
            .writes()
            .into_iter()
            .filter(|request| request.method == "DELETE")
            .map(|request| request.path)
            .collect::<Vec<String>>();
        assert!(deleted.contains(&format!(
            "{SECRETS_PATH}/{}",
            crate::k8s::crds::REGISTRY_TOKEN_SECRET
        )));
        assert!(deleted.contains(&format!("{SECRETS_PATH}/docker-skopeo")));
        // the operator's password outlives the cluster
        assert!(!deleted.contains(&format!("{SECRETS_PATH}/thorium-operator-pass")));
    }

    /// Build a Secret answer with labels
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the Secret
    /// * `labels` - The labels the Secret carries
    fn labelled_secret(name: &str, labels: &serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "metadata": {
                "name": name,
                "namespace": "thorium",
                "uid": format!("uid-{name}"),
                "resourceVersion": "7",
                "labels": labels
            }
        })
    }

    /// Registry Secrets are created with the operator's label, and an existing one gets the
    /// label patched on along with its data
    #[tokio::test]
    async fn registry_secrets_are_labelled() {
        // a cluster with registry_auth whose Secrets already exist
        let mut cluster = namespaced_cluster(full_spec());
        cluster.spec.registry_auth = Some(BTreeMap::from([(
            "registry.example".to_owned(),
            "token".to_owned(),
        )]));
        let fake = FakeKube::default()
            .route("POST", SECRETS_PATH, 409, kube_status(409, "AlreadyExists"))
            .route(
                "PATCH",
                &format!("{SECRETS_PATH}/docker-skopeo"),
                200,
                serde_json::json!({}),
            )
            .route(
                "PATCH",
                &format!("{SECRETS_PATH}/registry-token"),
                200,
                serde_json::json!({}),
            );
        let meta = meta_for(cluster, &fake.client());
        create_or_update_registry_auth(&meta)
            .await
            .expect("rendered");
        // both creates and both patches carry the label
        let writes = fake.writes();
        assert_eq!(writes.len(), 4, "{writes:?}");
        for write in &writes {
            assert_eq!(
                write.body["metadata"]["labels"][REGISTRY_AUTH_LABEL],
                "true"
            );
        }
    }

    /// Unsetting `registry_auth` deletes only the registry Secrets the operator rendered,
    /// tolerating ones already gone
    #[tokio::test]
    async fn unset_registry_auth_deletes_owned_secrets() {
        // the skopeo Secret was rendered by the operator, registry-token by someone else
        let fake = FakeKube::default()
            .route(
                "GET",
                &format!("{SECRETS_PATH}/docker-skopeo"),
                200,
                labelled_secret(
                    "docker-skopeo",
                    &serde_json::json!({REGISTRY_AUTH_LABEL: "true"}),
                ),
            )
            .route(
                "GET",
                &format!("{SECRETS_PATH}/registry-token"),
                200,
                labelled_secret("registry-token", &serde_json::json!({})),
            )
            .route(
                "DELETE",
                &format!("{SECRETS_PATH}/docker-skopeo"),
                200,
                labelled_secret("docker-skopeo", &serde_json::json!({})),
            );
        let meta = meta_for(namespaced_cluster(full_spec()), &fake.client());
        create_or_update_registry_auth(&meta)
            .await
            .expect("cleaned");
        // only the operator's Secret was deleted, guarded by its uid and resource version
        let writes = fake.writes();
        assert_eq!(writes.len(), 1, "{writes:?}");
        assert_eq!(writes[0].method, "DELETE");
        assert_eq!(writes[0].path, format!("{SECRETS_PATH}/docker-skopeo"));
        assert_eq!(writes[0].body["preconditions"]["uid"], "uid-docker-skopeo");
        assert_eq!(writes[0].body["preconditions"]["resourceVersion"], "7");
        // a labelled Secret Helm manages is kept too
        let fake = FakeKube::default().route(
            "GET",
            &format!("{SECRETS_PATH}/registry-token"),
            200,
            labelled_secret(
                "registry-token",
                &serde_json::json!({REGISTRY_AUTH_LABEL: "true", "app.kubernetes.io/managed-by": "Helm"}),
            ),
        );
        let meta = meta_for(namespaced_cluster(full_spec()), &fake.client());
        create_or_update_registry_auth(&meta)
            .await
            .expect("cleaned");
        assert_eq!(fake.writes().len(), 0, "{:?}", fake.writes());
        // a Secret deleted between the read and the delete is skipped
        let fake = FakeKube::default().route(
            "GET",
            &format!("{SECRETS_PATH}/registry-token"),
            200,
            labelled_secret(
                "registry-token",
                &serde_json::json!({REGISTRY_AUTH_LABEL: "true"}),
            ),
        );
        let meta = meta_for(namespaced_cluster(full_spec()), &fake.client());
        create_or_update_registry_auth(&meta)
            .await
            .expect("cleaned");
        assert_eq!(fake.writes().len(), 1);
    }
}
