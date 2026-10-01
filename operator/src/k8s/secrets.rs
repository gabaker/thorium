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
/// This creates or optionally updates a kubernetes secret if it exists.
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
    // get name and namespace for logging/error handling
    let name = secret
        .metadata
        .name
        .as_ref()
        .expect("could not get secret name");
    // first attempt to create the ConfigMap
    let params = PostParams::default();
    match meta.secret_api.create(&params, &secret).await {
        Ok(_) => {
            println!("Created {} secret in namespace {}", name, &meta.namespace);
            Ok(true)
        }
        Err(kube::Error::Api(error)) => {
            // do not panic if ConfigMap exists, patch it
            if error.reason == "AlreadyExists" {
                if !update {
                    println!(
                        "Warning: secret {} in namespace {} already exists",
                        name, &meta.namespace
                    );
                    return Ok(false);
                }
                let patch = serde_json::json!({
                    "data": secret.data
                });
                let patch = Patch::Merge(&patch);
                let params: PatchParams = PatchParams::default();
                match meta.secret_api.patch(&name, &params, &patch).await {
                    Ok(_) => {
                        println!("Patched {} secret in namespace {}", name, &meta.namespace);
                        Ok(true)
                    }
                    Err(error) => Err(Error::new(format!(
                        "Failed to patch {} secret: {}",
                        name, error
                    ))),
                }
            } else {
                Err(Error::new(format!(
                    "Failed to create {} secret: {}",
                    name, error
                )))
            }
        }
        Err(error) => Err(Error::new(format!(
            "Failed to create {} secret: {}",
            name, error
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
    let secret = ByteString(secret.to_owned().into_bytes());
    let mut data = BTreeMap::new();
    data.insert(key.to_owned(), secret);
    // create tracing ConfigMap object
    let secret = Secret {
        // metadata for the ConfigMap
        metadata: ObjectMeta {
            name: Some(name.to_owned()),
            namespace: Some(namespace.to_owned()),
            ..Default::default()
        },
        data: Some(data),
        ..Default::default()
    };
    secret
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
    // render the keys.yml for this user
    let template = serde_json::json!({
        "api": format!("http://thorium-api.{}.svc.cluster.local", &meta.namespace),
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
/// the operator track the authentication information for it's admin user account as
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
    secret_name: &String,
) -> Result<Option<Secret>, Error> {
    return match secret_api.get(secret_name).await {
        Ok(secret) => Ok(Some(secret)),
        Err(kube::Error::Api(error)) => {
            // do not panic when secret does not exists
            if error.reason == "NotFound" {
                Ok(None)
            } else {
                Err(Error::new(format!(
                    "Failed to get {} secret: {}",
                    secret_name, error
                )))
            }
        }
        Err(error) => Err(Error::new(format!(
            "Failed to get {} secret: {}",
            secret_name, error
        ))),
    };
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
/// # Arguments
///
/// * `username` - Name of the Thorium user
/// * `meta` - Thorium cluster client and metadata
pub async fn get_user_password(
    username: &str,
    meta: &ClusterMeta,
) -> Result<Option<String>, Error> {
    // get secret from k8s
    let secret_name = format!("{username}-pass");
    let secret = get_secret(&meta.secret_api, &secret_name).await?;
    if secret.is_none() {
        // secret was not found
        Ok(None)
    } else {
        // secret found, attempt to decode it
        let secret_data = secret
            .expect("expected secret to be some")
            .data
            .expect("expected secret data to be some");
        let byte_string = secret_data.get(username);
        if let Some(ByteString(raw_bytes)) = byte_string {
            let decoded =
                std::str::from_utf8(&raw_bytes).expect("decoding of secret was not valid utf8");
            return Ok(Some(decoded.to_owned()));
        }
        Ok(None)
    }
}

/// Create registry tokens secret from ThoriumCluster CRD
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
pub async fn create_or_update_registry_auth(meta: &ClusterMeta) -> Result<(), Error> {
    // check if registry auth tokens have been provided
    let registries = meta.cluster.spec.registry_auth.clone();
    if registries.is_none() {
        println!("No registry auth provided, skipping registry secret creation");
        return Ok(());
    }
    // build the url/token structure for the registry auth data
    // it should look like {}
    let mut auth_map: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for (url, token) in registries.unwrap().iter() {
        let registry_auth = BTreeMap::from([("auth".to_string(), token.to_owned())]);
        auth_map.insert(url.to_owned(), registry_auth);
    }
    // build the registry auth secret data
    let template = serde_json::json!({"auths": auth_map}).to_string();
    // build the docker skopeo secret for scaler
    let skopeo_secret = build_secret(
        template.as_ref(),
        "docker-skopeo",
        "config.json",
        &meta.namespace,
    );
    // create the secret but do not force update
    println!("Creating skopeo registry secret");
    create_or_update(meta, &skopeo_secret, true).await?;
    // build a container pull secret
    let mut pull_secret = build_secret(
        template.as_ref(),
        crate::k8s::crds::REGISTRY_TOKEN_SECRET,
        ".dockerconfigjson",
        &meta.namespace,
    );
    // we need to change the default type since default is Opaque
    println!("Creating registry pull secret");
    pull_secret.type_ = Some("kubernetes.io/dockerconfigjson".to_string());
    // create the secret and force update
    create_or_update(meta, &pull_secret, true).await?;
    Ok(())
}

/// Cleanup Thorium secrets
///
/// # Arguments
///
/// * `secret_api` - The Secret API for the `ThoriumCluster`'s namespace
pub async fn delete(secret_api: &Api<Secret>) -> Result<(), Error> {
    let params: DeleteParams = DeleteParams::default();
    // delete secrets from vector
    // Do not delete the "thorium-operator-pass" unless deleting the thorium-operator user from the Thorium API
    let secrets_names = vec![
        "thorium".to_string(),
        "keys".to_string(),
        "keys-kaboom".to_string(),
        "thorium-pass".to_string(),
        "thorium-kaboom-pass".to_string(),
        "docker-skopeo".to_string(),
    ];
    for secret_name in secrets_names.iter() {
        match secret_api.delete(secret_name, &params).await {
            Ok(_) => {
                println!("Deleted {} secret", secret_name);
            }
            Err(kube::Error::Api(error)) => {
                // secret was not found, continue on
                if error.code == 404 {
                    println!("Secret {} does not exist, skipping deletion", secret_name);
                    continue;
                }
                return Err(Error::new(format!(
                    "Failed to delete {} secret: {}",
                    secret_name, error
                )));
            }
            Err(error) => {
                return Err(Error::new(format!(
                    "Failed to delete {} secret: {}",
                    secret_name, error
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::k8s::clusters::tests::sample_conf;

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
}
