use k8s_openapi::api::{
    apps::v1::Deployment,
    core::v1::{ConfigMap, Node, Pod, Secret, Service},
};
use kube::{Api, Client};
use reqwest::StatusCode;
use std::sync::Arc;
use thorium::conf::ElasticCertValidation;
use thorium::{Conf, Error};

use super::{crds, secrets};

/// Wrapper for `ThoriumCluster` metadata
#[derive(Clone)]
pub struct ClusterMeta {
    /// namespace in k8s
    pub namespace: String,
    /// name of the `ThoriumCluster` instance
    pub name: String,
    /// The name of the kube context the `ThoriumCluster`'s k8s cluster is reached through
    pub context: String,
    /// kube api client
    pub client: Client,
    /// thorium cluster custom resource spec
    pub cluster: Arc<crds::ThoriumCluster>,
    /// The cluster's status as last written by the current reconcile
    pub status: Arc<crds::StatusTracker>,
    /// The Thorium config built from the spec's config merged with its config secrets
    pub conf: Conf,
    /// k8s api instance for `ConfigMaps`
    pub cm_api: Api<ConfigMap>,
    /// k8s api instance for Deployments
    pub deploy_api: Api<Deployment>,
    /// k8s api instance for Nodes
    pub node_api: Api<Node>,
    /// k8s api instance for Pods
    pub pod_api: Api<Pod>,
    /// k8s api instance for Secrets
    pub secret_api: Api<Secret>,
    /// k8s api instance for Services
    pub service_api: Api<Service>,
}

impl ClusterMeta {
    /// Build a new wrapper for k8s cluster metadata
    ///
    /// # Arguments
    ///
    /// * `status` - The cluster being reconciled and the status last written for it
    /// * `client` - The kube client to build APIs with
    /// * `context` - The name of the kube context `client` reaches its k8s cluster through
    pub async fn new(
        status: &Arc<crds::StatusTracker>,
        client: &Client,
        context: &str,
    ) -> Result<Self, Error> {
        // get the cluster snapshot this reconcile works from
        let cluster = status.cluster();
        // grab the cluster name and namespace from ThoriumCluster metadata
        let (name, namespace) = cluster_name_and_namespace(cluster)?;
        // build kube api client
        let cm_api: Api<ConfigMap> = Api::namespaced(client.clone(), &namespace);
        let deploy_api: Api<Deployment> = Api::namespaced(client.clone(), &namespace);
        let node_api: Api<Node> = Api::all(client.clone());
        let pod_api: Api<Pod> = Api::namespaced(client.clone(), &namespace);
        let secret_api: Api<Secret> = Api::namespaced(client.clone(), &namespace);
        let service_api: Api<Service> = Api::namespaced(client.clone(), &namespace);
        // build our Thorium config from the spec and its config secrets
        let conf = resolve_config(cluster, &secret_api).await?;
        // return the built cluster
        Ok(ClusterMeta {
            name,
            context: context.to_owned(),
            client: client.clone(),
            namespace,
            cluster: cluster.clone(),
            status: status.clone(),
            conf,
            cm_api,
            deploy_api,
            node_api,
            pod_api,
            secret_api,
            service_api,
        })
    }
}

/// Build an error for a config problem that retrying won't fix
///
/// # Arguments
///
/// * `code` - The status describing the problem (`NOT_FOUND` or `BAD_REQUEST`)
/// * `msg` - A description of the problem
fn config_error(code: StatusCode, msg: String) -> Error {
    Error::Thorium {
        code,
        msg: Some(msg),
    }
}

/// Get the name and namespace of a `ThoriumCluster`
///
/// # Arguments
///
/// * `cluster` - The `ThoriumCluster` to get the name and namespace of
pub fn cluster_name_and_namespace(
    cluster: &crds::ThoriumCluster,
) -> Result<(String, String), Error> {
    // get the name and namespace from this cluster's metadata
    match (&cluster.metadata.name, &cluster.metadata.namespace) {
        (Some(name), Some(namespace)) => Ok((name.clone(), namespace.clone())),
        _ => Err(config_error(
            StatusCode::BAD_REQUEST,
            "ThoriumCluster is missing a name or namespace".to_owned(),
        )),
    }
}

/// Check whether an error building a cluster's config is permanent
///
/// Missing Secrets or keys and invalid YAML or config won't resolve by retrying, while any
/// other error (an unreachable API server, a timeout, a forbidden request) might.
///
/// # Arguments
///
/// * `error` - The error to check
pub fn is_permanent_config_error(error: &Error) -> bool {
    // only not found and invalid input errors are permanent
    matches!(
        error.status(),
        Some(StatusCode::NOT_FOUND | StatusCode::BAD_REQUEST)
    )
}

/// Describe a failed read of a Secret without the raw kube error
///
/// The status code of the kube error is kept so missing Secrets are still classified as
/// permanent config errors.
///
/// # Arguments
///
/// * `error` - The error from reading the Secret
/// * `kind` - What the Secret holds (e.g. "config" or "bootstrap admin")
/// * `name` - The name of the Secret
/// * `namespace` - The namespace the Secret was read from
fn secret_read_error(error: kube::Error, kind: &str, name: &str, namespace: &str) -> Error {
    match error {
        // a missing secret is a config problem that retrying won't fix
        kube::Error::Api(response) if response.code == 404 => config_error(
            StatusCode::NOT_FOUND,
            format!("{kind} secret {name} not found in {namespace}"),
        ),
        // keep the status of any other API error so it is classified correctly
        kube::Error::Api(response) => Error::Thorium {
            code: StatusCode::from_u16(response.code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            msg: Some(format!(
                "Failed to read {kind} secret {name} in {namespace}: {}",
                response.message
            )),
        },
        // anything else (a timeout or connection error) may resolve on its own
        other => Error::new(format!(
            "Failed to read {kind} secret {name} in {namespace}: {other}"
        )),
    }
}

/// Get the raw bytes of one key in a Secret
///
/// A missing Secret or key is reported with a `NOT_FOUND` status.
///
/// # Arguments
///
/// * `api` - The Secret API for the Secret's namespace
/// * `kind` - What the Secret holds (e.g. "config" or "bootstrap admin") for errors
/// * `name` - The name of the Secret
/// * `key` - The key to read from the Secret
pub async fn get_secret_bytes(
    api: &Api<Secret>,
    kind: &str,
    name: &str,
    key: &str,
) -> Result<Vec<u8>, Error> {
    // get the namespace this secret is read from for errors
    let namespace = api.namespace().unwrap_or_default().to_owned();
    // get the secret
    let secret = api
        .get(name)
        .await
        .map_err(|error| secret_read_error(error, kind, name, &namespace))?;
    // take the requested key out of this secret
    secret
        .data
        .and_then(|mut data| data.remove(key))
        .map(|bytes| bytes.0)
        .ok_or_else(|| {
            config_error(
                StatusCode::NOT_FOUND,
                format!("{kind} secret {name} in {namespace} has no key {key}"),
            )
        })
}

/// Reject Secret names that collide with a Secret the operator or chart owns
///
/// The operator overwrites or deletes its own Secrets, so a config or bootstrap Secret
/// sharing one of their names would be clobbered.
///
/// # Arguments
///
/// * `cluster` - The `ThoriumCluster` whose referenced Secrets to check
pub fn validate_secret_names(cluster: &crds::ThoriumCluster) -> Result<(), Error> {
    // gather every secret this cluster reads
    let config = cluster
        .spec
        .config_secrets
        .iter()
        .map(|secret_ref| ("config_secrets", secret_ref.name.as_str()));
    let bootstrap = cluster
        .bootstrap_secret_names()
        .into_iter()
        .map(|name| ("bootstrap", name));
    let elastic_ca = cluster
        .spec
        .elastic_ca_secret
        .iter()
        .map(|secret_ref| ("elastic_ca_secret", secret_ref.name.as_str()));
    // reject the first one that collides with a reserved name
    for (field, name) in config.chain(bootstrap).chain(elastic_ca) {
        if secrets::is_reserved_secret_name(name) {
            return Err(config_error(
                StatusCode::BAD_REQUEST,
                format!(
                    "Secret {name} in {field} collides with a Secret the operator manages; \
                     rename it (reserved: {}, and any name ending in -pass)",
                    secrets::RESERVED_SECRET_NAMES.join(", ")
                ),
            ));
        }
    }
    Ok(())
}

/// Read a partial Thorium config from a Secret
///
/// # Arguments
///
/// * `secret_api` - The Secret API for the `ThoriumCluster`'s namespace
/// * `secret_ref` - The Secret and key holding the partial config
async fn read_config_secret(
    secret_api: &Api<Secret>,
    secret_ref: &crds::ConfigSecretRef,
) -> Result<serde_json::Value, Error> {
    // get the partial config from the referenced secret
    let raw = get_secret_bytes(secret_api, "config", &secret_ref.name, &secret_ref.key).await?;
    // parse the partial config as YAML
    serde_norway::from_slice(&raw).map_err(|error| {
        config_error(
            StatusCode::BAD_REQUEST,
            format!(
                "Config secret {}/{} is not valid YAML: {error}",
                secret_ref.name, secret_ref.key
            ),
        )
    })
}

/// Parse a merged config into a full Thorium config
///
/// Values are not coerced, so each one must already have the type Thorium expects.
///
/// # Arguments
///
/// * `merged` - The spec's config with every config secret merged over it
/// * `sources` - The config secrets that were merged, for errors
fn parse_config(merged: serde_json::Value, sources: &[String]) -> Result<Conf, Error> {
    // cast our merged config into a full Thorium config
    serde_json::from_value(merged).map_err(|error| {
        config_error(
            StatusCode::BAD_REQUEST,
            format!(
                "Invalid Thorium config from spec.config merged with config secrets {sources:?}: {error}"
            ),
        )
    })
}

/// Build the Thorium config for a `ThoriumCluster`
///
/// This starts with the spec's config and merges each config secret over it in order.
///
/// # Arguments
///
/// * `cluster` - The `ThoriumCluster` to build a config for
/// * `secret_api` - The Secret API for the `ThoriumCluster`'s namespace
pub async fn resolve_config(
    cluster: &crds::ThoriumCluster,
    secret_api: &Api<Secret>,
) -> Result<Conf, Error> {
    // make sure no referenced secret collides with one we manage
    validate_secret_names(cluster)?;
    // start with the config in the spec
    let mut merged = cluster.spec.config.clone();
    // merge each config secret over our config in order
    for secret_ref in &cluster.spec.config_secrets {
        // read this secret's partial config
        let overlay = read_config_secret(secret_api, secret_ref).await?;
        // merge this partial config into our config as a JSON merge patch (RFC 7386)
        json_patch::merge(&mut merged, &overlay);
    }
    // list the secrets we merged so a missing field is easy to track down
    let sources = cluster
        .spec
        .config_secrets
        .iter()
        .map(|secret_ref| format!("{}/{}", secret_ref.name, secret_ref.key))
        .collect::<Vec<String>>();
    // cast our merged config into a full Thorium config
    let mut conf = parse_config(merged, &sources)?;
    // validate Elastic against the mounted CA if one is set
    apply_elastic_ca(cluster, &mut conf);
    Ok(conf)
}

/// Validate Elastic's certificate against the mounted CA when an Elastic CA Secret is set
///
/// Full validation (the chain and the hostname) is used and certificate validation can't be
/// disabled, since setting a CA is an explicit request to verify Elastic.
///
/// # Arguments
///
/// * `cluster` - The `ThoriumCluster` the config is for
/// * `conf` - The config to update
pub fn apply_elastic_ca(cluster: &crds::ThoriumCluster, conf: &mut Conf) {
    // leave the config alone without a CA Secret
    if let Some(path) = cluster.elastic_ca_path() {
        conf.elastic.cert_validation = Some(ElasticCertValidation::Full(path));
        conf.elastic.insecure_certificates = false;
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    /// Build a full Thorium config like the one the Helm chart renders with its secrets
    pub(crate) fn sample_config() -> serde_json::Value {
        json!({
            "elastic": {
                "insecure_certificates": true,
                "node": "https://elastic-es-http.elastic.svc.cluster.local:9200",
                "username": "thorium",
                "password": "elastic-pass",
                "results": {"repos": "thorium_repo_results", "samples": "thorium_sample_results"},
                "tags": {"repos": "thorium_repo_tags", "samples": "thorium_sample_tags"}
            },
            "redis": {"host": "redis.redis.svc.cluster.local", "port": 6379, "password": "redis-pass"},
            "scylla": {
                "nodes": ["scylla-client.scylla.svc.cluster.local"],
                "replication": 1,
                "auth": {"username": "thorium", "password": "scylla-pass"}
            },
            "thorium": {
                "secret_key": "secret-key",
                "attachments": {"bucket": "thorium-attachments"},
                "cors": {"insecure": true},
                "ephemeral": {"bucket": "thorium-ephemeral"},
                "files": {"bucket": "thorium-files", "earliest": 1_610_596_807, "partition_size": 3600},
                "repos": {"bucket": "thorium-repos", "partition_size": 3600},
                "results": {"bucket": "thorium-results", "earliest": 1_610_596_807, "partition_size": 3600},
                "s3": {
                    "endpoint": "http://seaweedfs.seaweedfs.svc.cluster.local:8333",
                    "region": "us-east-1",
                    "use_path_style": true,
                    "access_key": "access",
                    "secret_token": "secret"
                },
                "scaler": {
                    "crane": {"insecure": true},
                    "k8s": {"clusters": {"kubernetes-admin@cluster.local": {"alias": "thorium", "nodes": []}}}
                },
                "tracing": {
                    "external": {"Grpc": {"endpoint": "http://quickwit-indexer:7281", "level": "Info"}},
                    "local": {"level": "Info"}
                }
            }
        })
    }

    /// Build a full Thorium config from [`sample_config`]
    pub(crate) fn sample_conf() -> Conf {
        parse_config(sample_config(), &[]).expect("the sample config should parse")
    }

    /// Build a `ThoriumCluster` from a spec written as JSON
    ///
    /// # Arguments
    ///
    /// * `spec` - The spec to deserialize
    pub(crate) fn cluster_from_spec(spec: serde_json::Value) -> crds::ThoriumCluster {
        // deserialize our spec
        let spec: crds::ThoriumClusterSpec =
            serde_json::from_value(spec).expect("spec should deserialize");
        crds::ThoriumCluster::new("thorium", spec)
    }

    /// Build a `ThoriumCluster` named `thorium` in the `thorium` namespace from a JSON spec
    ///
    /// # Arguments
    ///
    /// * `spec` - The spec to deserialize
    pub(crate) fn namespaced_cluster(spec: serde_json::Value) -> crds::ThoriumCluster {
        // build the cluster and place it in its namespace like the API server would
        let mut cluster = cluster_from_spec(spec);
        cluster.metadata.namespace = Some("thorium".to_owned());
        cluster
    }

    /// Build a chart shaped spec with every optional component enabled
    pub(crate) fn full_spec() -> serde_json::Value {
        json!({
            "components": {
                "api": {},
                "scaler": {},
                "baremetal_scaler": {},
                "search_streamer": {},
                "event_handler": {}
            },
            "registry": "registry/thorium",
            "version": "1.8.1",
            "image_pull_secrets": ["thorium-image-pull"],
            "config": {},
            "config_secrets": [{"name": "thorium-config-secrets"}]
        })
    }

    /// Build a spec shaped like the ones the pre-Helm minithor and megathor scripts deployed,
    /// with Thorium's secret key inline and no config secrets
    pub(crate) fn pre_helm_spec() -> serde_json::Value {
        json!({
            "components": {"api": {}},
            "registry": "registry/thorium",
            "config": {"thorium": {"secret_key": "inline"}}
        })
    }

    /// The kube context test cluster metadata is placed in
    pub(crate) const TEST_CONTEXT: &str = "kubernetes-admin@cluster.local";

    /// Build cluster metadata for a cluster without contacting the kube API
    ///
    /// The config is [`sample_conf`] rather than one resolved from Secrets.
    ///
    /// # Arguments
    ///
    /// * `cluster` - The cluster to wrap, which must have a name and namespace
    /// * `client` - The kube client the metadata's APIs use
    pub(crate) fn meta_for(cluster: crds::ThoriumCluster, client: &Client) -> ClusterMeta {
        // get the cluster's name and namespace
        let (name, namespace) = cluster_name_and_namespace(&cluster).expect("name and namespace");
        // track the cluster's status from its snapshot like a reconcile does
        let cluster = Arc::new(cluster);
        let status = Arc::new(crds::StatusTracker::new(cluster.clone()));
        // build every API against the given client like ClusterMeta::new does
        ClusterMeta {
            name,
            context: TEST_CONTEXT.to_owned(),
            client: client.clone(),
            cm_api: Api::namespaced(client.clone(), &namespace),
            deploy_api: Api::namespaced(client.clone(), &namespace),
            node_api: Api::all(client.clone()),
            pod_api: Api::namespaced(client.clone(), &namespace),
            secret_api: Api::namespaced(client.clone(), &namespace),
            service_api: Api::namespaced(client.clone(), &namespace),
            namespace,
            cluster,
            status,
            conf: sample_conf(),
        }
    }

    /// A request the fake kube API received
    #[derive(Debug, Clone)]
    pub(crate) struct FakeRequest {
        /// The HTTP method of the request
        pub method: String,
        /// The path of the request without its query
        pub path: String,
        /// The JSON body of the request, or null when it had none
        pub body: serde_json::Value,
    }

    /// A canned answer the fake kube API gives every request with a method and path
    #[derive(Debug, Clone)]
    struct FakeRoute {
        /// The HTTP method to answer
        method: &'static str,
        /// The exact path to answer
        path: String,
        /// The status code to answer with
        status: u16,
        /// The JSON body to answer with
        body: serde_json::Value,
    }

    /// An in-process fake of the kube API that gives canned answers and records every request
    ///
    /// A request without a matching route gets a 404 `Status`, which kube reports as a
    /// missing object.
    #[derive(Debug, Clone, Default)]
    pub(crate) struct FakeKube {
        /// The canned answers, the first matching one winning
        routes: Arc<std::sync::Mutex<Vec<FakeRoute>>>,
        /// Every request received, oldest first
        requests: Arc<std::sync::Mutex<Vec<FakeRequest>>>,
    }

    /// Build the `Status` body the kube API answers a failed request with
    ///
    /// # Arguments
    ///
    /// * `code` - The HTTP status code
    /// * `reason` - The machine readable reason (e.g. `AlreadyExists`)
    pub(crate) fn kube_status(code: u16, reason: &str) -> serde_json::Value {
        json!({
            "kind": "Status",
            "apiVersion": "v1",
            "metadata": {},
            "status": "Failure",
            "message": format!("fake {reason}"),
            "reason": reason,
            "code": code
        })
    }

    impl FakeKube {
        /// Add a canned answer for every request with a method and path
        ///
        /// # Arguments
        ///
        /// * `method` - The HTTP method to answer
        /// * `path` - The exact path to answer
        /// * `status` - The status code to answer with
        /// * `body` - The JSON body to answer with
        pub(crate) fn route(
            self,
            method: &'static str,
            path: &str,
            status: u16,
            body: serde_json::Value,
        ) -> Self {
            // add the answer after any earlier ones so those still win
            self.routes.lock().expect("routes lock").push(FakeRoute {
                method,
                path: path.to_owned(),
                status,
                body,
            });
            self
        }

        /// Get every request received so far
        pub(crate) fn requests(&self) -> Vec<FakeRequest> {
            self.requests.lock().expect("requests lock").clone()
        }

        /// Get every request received so far that could change something
        pub(crate) fn writes(&self) -> Vec<FakeRequest> {
            // anything but a read may change the cluster
            self.requests()
                .into_iter()
                .filter(|request| request.method != "GET")
                .collect()
        }

        /// Build a kube client whose requests this fake answers
        ///
        /// The client spawns a buffer task, so it must be built inside a tokio runtime.
        pub(crate) fn client(&self) -> Client {
            // answer each request from the routes and record it
            let fake = self.clone();
            let service = tower::service_fn(move |request: http::Request<kube::client::Body>| {
                // give each request its own handle on the fake
                let fake = fake.clone();
                async move {
                    // record the request with its JSON body
                    let method = request.method().to_string();
                    let path = request.uri().path().to_owned();
                    let raw = request.into_body().collect_bytes().await?;
                    let body = serde_json::from_slice(&raw).unwrap_or(serde_json::Value::Null);
                    fake.requests
                        .lock()
                        .expect("requests lock")
                        .push(FakeRequest {
                            method: method.clone(),
                            path: path.clone(),
                            body,
                        });
                    // find the canned answer, treating anything unknown as missing
                    let (status, body) = fake
                        .routes
                        .lock()
                        .expect("routes lock")
                        .iter()
                        .find(|route| route.method == method && route.path == path)
                        .map_or_else(
                            || (404, kube_status(404, "NotFound")),
                            |route| (route.status, route.body.clone()),
                        );
                    // answer with the JSON body
                    let response = http::Response::builder()
                        .status(status)
                        .header("content-type", "application/json")
                        .body(kube::client::Body::from(serde_json::to_vec(&body)?))?;
                    Ok::<_, Box<dyn std::error::Error + Send + Sync>>(response)
                }
            });
            // hand the fake to a kube client defaulting to the test namespace
            Client::new(service, "thorium")
        }
    }

    /// A cluster without a name or namespace is a permanent config error
    #[test]
    fn name_and_namespace_required() {
        // a cluster with both is fine
        let cluster = namespaced_cluster(full_spec());
        assert_eq!(
            cluster_name_and_namespace(&cluster).expect("both set"),
            ("thorium".to_owned(), "thorium".to_owned())
        );
        // a cluster without a namespace can't be placed and retrying won't help
        let error =
            cluster_name_and_namespace(&cluster_from_spec(full_spec())).expect_err("no namespace");
        assert!(is_permanent_config_error(&error));
    }

    /// The fake kube API answers routes, treats anything else as missing, and records requests
    #[tokio::test]
    async fn fake_kube_answers_and_records() {
        // answer one ConfigMap read
        let fake = FakeKube::default().route(
            "GET",
            "/api/v1/namespaces/thorium/configmaps/present",
            200,
            json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "present"}}),
        );
        let api: Api<ConfigMap> = Api::namespaced(fake.client(), "thorium");
        // the routed object is found and anything else is missing
        assert!(api.get_opt("present").await.expect("get").is_some());
        assert!(api.get_opt("absent").await.expect("get").is_none());
        // both reads were recorded and none of them wrote anything
        assert_eq!(fake.requests().len(), 2);
        assert!(fake.writes().is_empty());
    }

    /// A config with every value already the right type parses
    #[test]
    fn parse_config_accepts_typed_values() {
        // parse the sample config
        let conf = sample_conf();
        // values come through untouched
        assert_eq!(conf.redis.port, 6379);
        assert_eq!(conf.scylla.replication, 1);
    }

    /// Values are not coerced, so a quoted number is rejected with a config error
    #[test]
    fn parse_config_does_not_coerce() {
        // set the redis port to a quoted number
        let mut config = sample_config();
        config["redis"]["port"] = json!("6379");
        // the config is rejected instead of coerced
        let error = parse_config(config, &["thorium-config-secrets/thorium.yml".to_owned()])
            .expect_err("a quoted port should not parse");
        // the error is a permanent config error that names the merged secrets
        assert!(is_permanent_config_error(&error));
        assert!(
            error
                .to_string()
                .contains("thorium-config-secrets/thorium.yml")
        );
        // a quoted bool is rejected as well
        let mut config = sample_config();
        config["thorium"]["s3"]["use_path_style"] = json!("true");
        assert!(parse_config(config, &[]).is_err());
    }

    /// Secrets named like one the operator owns are rejected as bad config
    #[test]
    fn reserved_secret_names_rejected() {
        // build a spec around some config and bootstrap secrets
        let spec = |config: &str, admin: &str| {
            json!({
                "components": {"api": {}},
                "registry": "registry/thorium",
                "config": {},
                "config_secrets": [{"name": config}],
                "bootstrap": {"admin": {"secret": {"name": admin, "password_key": "password"}}}
            })
        };
        // non-colliding names are accepted
        let cluster = cluster_from_spec(spec("thorium-config-secrets", "thorium-admin"));
        assert!(validate_secret_names(&cluster).is_ok());
        // every reserved name is rejected in config_secrets
        for name in secrets::RESERVED_SECRET_NAMES {
            let cluster = cluster_from_spec(spec(name, "thorium-admin"));
            let error = validate_secret_names(&cluster).expect_err("reserved name accepted");
            assert_eq!(error.status(), Some(StatusCode::BAD_REQUEST), "{name}");
        }
        // user password secrets are rejected in the bootstrap
        let cluster = cluster_from_spec(spec("thorium-config-secrets", "thorium-operator-pass"));
        let error = validate_secret_names(&cluster).expect_err("a -pass secret was accepted");
        assert!(is_permanent_config_error(&error));
        assert!(error.to_string().contains("thorium-operator-pass"));
    }

    /// An Elastic CA Secret named like one the operator owns is rejected
    #[test]
    fn reserved_elastic_ca_name_rejected() {
        // build a cluster whose CA secret collides with the rendered config
        let cluster = cluster_from_spec(json!({
            "components": {"api": {}},
            "registry": "registry/thorium",
            "config": {},
            "elastic_ca_secret": {"name": "thorium"}
        }));
        let error = validate_secret_names(&cluster).expect_err("reserved name accepted");
        assert!(error.to_string().contains("elastic_ca_secret"));
    }

    /// An Elastic CA Secret switches Elastic to full validation against the mounted CA
    #[test]
    fn elastic_ca_sets_cert_validation() {
        // a cluster without a CA secret keeps the config's validation
        let mut conf = sample_conf();
        let plain = cluster_from_spec(json!({
            "components": {"api": {}},
            "registry": "registry/thorium",
            "config": {}
        }));
        apply_elastic_ca(&plain, &mut conf);
        assert!(conf.elastic.insecure_certificates);
        assert_eq!(conf.elastic.cert_validation, None);
        // a CA secret defaults its key and turns on full validation
        let with_ca = cluster_from_spec(json!({
            "components": {"api": {}},
            "registry": "registry/thorium",
            "config": {},
            "elastic_ca_secret": {"name": "elastic-ca"}
        }));
        let secret_ref = with_ca.spec.elastic_ca_secret.as_ref().expect("CA secret");
        assert_eq!(secret_ref.key, "ca.crt");
        apply_elastic_ca(&with_ca, &mut conf);
        assert!(!conf.elastic.insecure_certificates);
        assert_eq!(
            conf.elastic.cert_validation,
            Some(ElasticCertValidation::Full(
                "/etc/thorium/elastic-ca/ca.crt".into()
            ))
        );
    }

    /// A missing Secret keeps its not found status with a short message
    #[test]
    fn secret_read_error_is_concise() {
        // build the error kube returns for a missing secret
        let missing = kube::Error::Api(kube::core::ErrorResponse {
            status: "Failure".to_owned(),
            message: "secrets \"admin\" not found".to_owned(),
            reason: "NotFound".to_owned(),
            code: 404,
        });
        // the message names the kind, secret, and namespace
        let error = secret_read_error(missing, "bootstrap admin", "admin", "thorium");
        assert_eq!(error.status(), Some(StatusCode::NOT_FOUND));
        assert!(
            error
                .to_string()
                .contains("bootstrap admin secret admin not found in thorium")
        );
        // a forbidden read keeps its status so it isn't treated as permanent
        let forbidden = kube::Error::Api(kube::core::ErrorResponse {
            status: "Failure".to_owned(),
            message: "forbidden".to_owned(),
            reason: "Forbidden".to_owned(),
            code: 403,
        });
        let error = secret_read_error(forbidden, "config", "conf", "thorium");
        assert!(!is_permanent_config_error(&error));
        assert_eq!(error.status(), Some(StatusCode::FORBIDDEN));
    }

    /// Merge an overlay over a base and return the result
    ///
    /// # Arguments
    ///
    /// * `base` - The value to merge into
    /// * `overlay` - The value to merge over `base`
    fn merged(mut base: serde_json::Value, overlay: &serde_json::Value) -> serde_json::Value {
        // merge our overlay into our base
        json_patch::merge(&mut base, overlay);
        base
    }

    /// Objects merge key by key, keeping keys the overlay doesn't set
    #[test]
    fn merge_objects() {
        // merge a partial object over a full one
        let result = merged(
            json!({"redis": {"host": "redis", "port": 6379}}),
            &json!({"redis": {"password": "secret"}}),
        );
        // every key from both sides is kept
        assert_eq!(
            result,
            json!({"redis": {"host": "redis", "port": 6379, "password": "secret"}})
        );
    }

    /// Arrays are replaced rather than appended to or merged
    #[test]
    fn merge_replaces_arrays() {
        // merge a shorter list over a longer one
        let result = merged(
            json!({"scylla": {"nodes": ["a", "b", "c"]}}),
            &json!({"scylla": {"nodes": ["d"]}}),
        );
        // the overlay's list replaces the base's
        assert_eq!(result, json!({"scylla": {"nodes": ["d"]}}));
    }

    /// A scalar replaces an object and an object replaces a scalar
    #[test]
    fn merge_type_conflicts() {
        // a scalar replaces an object
        let result = merged(json!({"a": {"b": 1}}), &json!({"a": 5}));
        assert_eq!(result, json!({"a": 5}));
        // an object replaces a scalar
        let result = merged(json!({"a": 5}), &json!({"a": {"b": 1}}));
        assert_eq!(result, json!({"a": {"b": 1}}));
        // an array replaces an object
        let result = merged(json!({"a": {"b": 1}}), &json!({"a": [1]}));
        assert_eq!(result, json!({"a": [1]}));
    }

    /// Nested keys missing from the base are added
    #[test]
    fn merge_adds_nested_keys() {
        // merge a deeply nested key into an empty config
        let result = merged(
            json!({}),
            &json!({"thorium": {"s3": {"access_key": "key"}}}),
        );
        // the whole nested path is created
        assert_eq!(result, json!({"thorium": {"s3": {"access_key": "key"}}}));
    }

    /// Overlays apply in order so later ones win
    #[test]
    fn merge_ordered_overlays() {
        // merge two overlays that set the same key
        let mut config = json!({"elastic": {"username": "spec", "node": "http://es"}});
        json_patch::merge(&mut config, &json!({"elastic": {"username": "first"}}));
        json_patch::merge(&mut config, &json!({"elastic": {"username": "second"}}));
        // the last overlay wins and untouched keys are kept
        assert_eq!(
            config,
            json!({"elastic": {"username": "second", "node": "http://es"}})
        );
    }

    /// A null deletes a key instead of setting it to null
    #[test]
    fn merge_null_deletes() {
        // delete one nested key and leave its sibling
        let result = merged(
            json!({"scylla": {"auth": {"username": "u", "password": "p"}, "nodes": ["a"]}}),
            &json!({"scylla": {"auth": null}}),
        );
        // the key is gone rather than null
        assert_eq!(result, json!({"scylla": {"nodes": ["a"]}}));
        // deleting a missing key is a no-op
        let result = merged(json!({"a": 1}), &json!({"b": null}));
        assert_eq!(result, json!({"a": 1}));
    }

    /// Only not found and invalid input errors are permanent
    #[test]
    fn permanent_config_errors() {
        // missing secrets or keys are permanent
        assert!(is_permanent_config_error(&config_error(
            StatusCode::NOT_FOUND,
            "missing".to_owned()
        )));
        // invalid YAML or config is permanent
        assert!(is_permanent_config_error(&config_error(
            StatusCode::BAD_REQUEST,
            "invalid".to_owned()
        )));
        // a forbidden request may be fixed by an RBAC change
        assert!(!is_permanent_config_error(&config_error(
            StatusCode::FORBIDDEN,
            "forbidden".to_owned()
        )));
        // generic errors such as timeouts are transient
        assert!(!is_permanent_config_error(&Error::new("timed out")));
    }
}
