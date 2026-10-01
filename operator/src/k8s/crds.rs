use k8s_openapi::{
    apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition,
    apimachinery::pkg::api::resource::Quantity,
};
use kube::{
    Client,
    api::{Api, Patch, PatchParams},
    core::CustomResourceExt,
    runtime::{conditions, wait::await_condition},
};
use kube_derive::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;
use thorium::Error;

/// A struct representing an environment variable
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Hash, Eq, PartialEq)]
pub struct EnvVar {
    pub name: String,
    pub value: Option<String>,
}

/// A struct representing the cpu an memory resources of a container
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Hash, Eq, PartialEq)]
pub struct Resources {
    pub cpu: u64,
    pub memory: u64,
}

/// Serde helper for default container resources
fn default_resources() -> Resources {
    Resources {
        // Amount of CPU cores in millicpus
        cpu: 2000,
        // Amount of RAM in mebibytes
        memory: 4096,
    }
}

/// COPIED FROM API without ephemeral/gpus
// used when casting to a quantity
macro_rules! quantity {
    ($($raw:tt)+) => {serde_json::from_value(json!($($raw)+))}
}
impl Resources {
    /// converts a resource request to a BTreeMap
    ///
    /// This will ignore any value that is None
    ///
    /// # Arguments
    ///
    /// * `raw` - The resource request to convert
    pub fn request_conv(raw: &Resources) -> Result<BTreeMap<String, Quantity>, Error> {
        // creat btreemap of requests
        let mut btree = BTreeMap::default();
        // build the resource request map
        btree.insert("cpu".to_owned(), quantity!(format!("{}m", raw.cpu))?);
        btree.insert("memory".to_owned(), quantity!(format!("{}Mi", raw.memory))?);
        Ok(btree)
    }
}

/// Serde helper for default environment variables
fn default_envs() -> Vec<EnvVar> {
    vec![
        EnvVar {
            name: "http_proxy".to_owned(),
            value: Some("".to_owned()),
        },
        EnvVar {
            name: "https_proxy".to_owned(),
            value: Some("".to_owned()),
        },
        EnvVar {
            name: "no_proxy".to_owned(),
            value: Some("localhost,cluster.local".to_owned()),
        },
        EnvVar {
            name: "HTTP_PROXY".to_owned(),
            value: Some("".to_owned()),
        },
        EnvVar {
            name: "HTTPS_PROXY".to_owned(),
            value: Some("".to_owned()),
        },
        EnvVar {
            name: "NO_PROXY".to_owned(),
            value: Some("localhost,cluster.local".to_owned()),
        },
    ]
}

/// Serde helper for default number of api replicas
fn default_api_replicas() -> u16 {
    3
}

/// Serde helper for default api container args (cmd in a Dockerfile)
fn default_api_cmd() -> Vec<String> {
    vec!["/app/thorium-api".to_owned()]
}

/// Serde helper for default api container cmd (entrypoint in a Dockerfile)
fn default_api_args() -> Vec<String> {
    vec!["--config".to_owned(), "/conf/thorium.yml".to_owned()]
}

/// Serde helper for default API memory and cpu resources
fn default_api_resources() -> Resources {
    Resources {
        cpu: 2000,
        memory: 8192,
    }
}

/// Thorium API spec
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Hash, Eq, PartialEq)]
pub struct ThoriumApi {
    /// Number of API pods to scale the deployment
    #[serde(default = "default_api_replicas")]
    pub replicas: u16,
    /// Environment variables to apply to API container
    #[serde(default = "default_envs")]
    pub env: Vec<EnvVar>,
    /// Commands to run in API container
    #[serde(default = "default_api_cmd")]
    pub cmd: Vec<String>,
    // Args to pass to command in API container
    #[serde(default = "default_api_args")]
    pub args: Vec<String>,
    /// The CPU and Memory needed by the API
    #[serde(default = "default_api_resources")]
    pub resources: Resources,
}

impl Default for ThoriumApi {
    /// Build a default api config
    fn default() -> Self {
        ThoriumApi {
            replicas: default_api_replicas(),
            env: default_envs(),
            cmd: default_api_cmd(),
            args: default_api_args(),
            resources: default_api_resources(),
        }
    }
}

/// Serde helper for default kube config path
fn default_scaler_envs() -> Vec<EnvVar> {
    let mut envs = default_envs();
    envs.push(EnvVar {
        name: "KUBECONFIG".to_owned(),
        value: Some("/root/.kube/config".to_owned()),
    });
    envs
}

/// Serde helper for default scaler container cmd (entrypoint in a Dockerfile)
fn default_scaler_cmd() -> Vec<String> {
    vec!["/app/thorium-scaler".to_owned()]
}

/// Serde helper for default scaler container args (cmd in a Dockerfile)
fn default_scaler_args() -> Vec<String> {
    vec![
        "--config".to_owned(),
        "/conf/thorium.yml".to_owned(),
        "--auth".to_owned(),
        "/keys/keys.yml".to_owned(),
    ]
}

/// Serde helper for default for whether the scaler uses a service account
fn default_service_account() -> bool {
    false
}

/// K8s scaler spec
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Hash, Eq, PartialEq)]
pub struct ThoriumScaler {
    /// Environment variables to apply in container
    #[serde(default = "default_scaler_envs")]
    pub env: Vec<EnvVar>,
    /// Commands to run in scaler container
    #[serde(default = "default_scaler_cmd")]
    pub cmd: Vec<String>,
    // Args to pass to command in scaler container
    #[serde(default = "default_scaler_args")]
    pub args: Vec<String>,
    /// The CPU and Memory for a scaler container
    #[serde(default = "default_resources")]
    pub resources: Resources,
    /// whether to use a service account
    #[serde(default = "default_service_account")]
    pub service_account: bool,
}

/// Serde helper for default baremetal scaler container args (cmd in a Dockerfile)
fn default_baremetal_scaler_args() -> Vec<String> {
    let mut args = default_scaler_args();
    args.append(&mut vec!["--scaler".to_owned(), "bare-metal".to_owned()]);
    args
}

/// Baremetal scaler spec
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Hash, Eq, PartialEq)]
pub struct ThoriumBaremetalScaler {
    /// Environment variables to apply in container
    #[serde(default = "default_scaler_envs")]
    pub env: Vec<EnvVar>,
    /// Commands to run in scaler container
    #[serde(default = "default_scaler_cmd")]
    pub cmd: Vec<String>,
    // Args to pass to command in scaler container
    #[serde(default = "default_baremetal_scaler_args")]
    pub args: Vec<String>,
    /// The CPU and Memory for a scaler container
    #[serde(default = "default_resources")]
    pub resources: Resources,
}

/// Serde helper for default search-streamer container cmd (entrypoint in a Dockerfile)
fn default_search_streamer_cmd() -> Vec<String> {
    vec!["/app/thorium-search-streamer".to_owned()]
}

/// Serde helper for default search-streamer container args (cmd in a Dockerfile)
fn default_search_streamer_args() -> Vec<String> {
    vec![
        "--config".to_owned(),
        "/conf/thorium.yml".to_owned(),
        "--keys".to_owned(),
        "/keys/keys.yml".to_owned(),
    ]
}

/// Search streamer spec
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Hash, Eq, PartialEq)]
pub struct ThoriumSearchStreamer {
    /// Environment variables to apply in container
    #[serde(default = "default_envs")]
    pub env: Vec<EnvVar>,
    /// Commands to run in search-streamer container
    #[serde(default = "default_search_streamer_cmd")]
    pub cmd: Vec<String>,
    // Args to pass to command in search-streamer container
    #[serde(default = "default_search_streamer_args")]
    pub args: Vec<String>,
    /// The CPU and Memory needed by the search-streamer
    #[serde(default = "default_resources")]
    pub resources: Resources,
}

/// Serde helper for default event-handler container cmd (entrypoint in a Dockerfile)
fn default_event_handler_cmd() -> Vec<String> {
    vec!["/app/thorium-event-handler".to_owned()]
}

/// Serde helper for default event-handler container args (cmd in a Dockerfile)
fn default_event_handler_args() -> Vec<String> {
    vec![
        "--config".to_owned(),
        "/conf/thorium.yml".to_owned(),
        "--auth".to_owned(),
        "/keys/keys.yml".to_owned(),
    ]
}

/// Event handler spec
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Hash, Eq, PartialEq)]
pub struct ThoriumEventHandler {
    /// Environment variables to apply in container
    #[serde(default = "default_envs")]
    pub env: Vec<EnvVar>,
    /// Commands to run in event-handler container
    #[serde(default = "default_event_handler_cmd")]
    pub cmd: Vec<String>,
    // Args to pass to command in event-handler container
    #[serde(default = "default_event_handler_args")]
    pub args: Vec<String>,
    /// The CPU and Memory needed by the event-handler
    #[serde(default = "default_resources")]
    pub resources: Resources,
}

/// Thorium components to deploy
#[derive(Serialize, Deserialize, Clone, Debug, Default, JsonSchema, Hash, Eq, PartialEq)]
pub struct ThoriumComponents {
    /// Thorium API
    pub api: ThoriumApi,
    /// The kubernetes scaler
    pub scaler: Option<ThoriumScaler>,
    /// The baremetal/kaboom scaler
    pub baremetal_scaler: Option<ThoriumBaremetalScaler>,
    /// Elastic search streamer
    pub search_streamer: Option<ThoriumSearchStreamer>,
    /// Event trigger/handler component of Thorium
    pub event_handler: Option<ThoriumEventHandler>,
}

/// Serde helper for default image pull policy for containers
fn default_pull_policy() -> String {
    "Always".to_string()
}

/// Serde helper for default image version to deploy
fn default_version() -> String {
    //String::from(env!("CARGO_PKG_VERSION"))
    "latest".to_owned()
}

/// The name of the `ThoriumCluster` CRD and of this operator's finalizer
pub const CRD_NAME: &str = "thoriumclusters.sandia.gov";

/// The pull secret the operator renders from `registry_auth`
pub const REGISTRY_TOKEN_SECRET: &str = "registry-token";

/// Serde helper for the default key of a config secret
fn default_config_secret_key() -> String {
    "thorium.yml".to_owned()
}

/// A reference to a Secret holding a partial thorium.yml config
///
/// The document is parsed as plain YAML and merged over `config` as a JSON merge patch
/// (RFC 7386): objects merge key by key, any other value (lists included) replaces the
/// existing one, and a null deletes the key. YAML tags (e.g. `!Grpc`), non-string map keys,
/// and merge keys (`<<`) are not supported. Values are not coerced the way thorium.yml is, so
/// each one must already have the type Thorium expects (a quoted "5" stays a string).
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
pub struct ConfigSecretRef {
    /// The name of the Secret in the `ThoriumCluster`'s namespace
    pub name: String,
    /// The key in the Secret containing the partial thorium.yml YAML document
    #[serde(default = "default_config_secret_key")]
    pub key: String,
}

/// Serde helper for the default key of a CA Secret
fn default_ca_secret_key() -> String {
    ELASTIC_CA_FILE.to_owned()
}

/// The directory every Thorium component and the operator mount the Elastic CA Secret at
pub const ELASTIC_CA_DIR: &str = "/etc/thorium/elastic-ca";

/// The file name the Elastic CA is mounted as inside [`ELASTIC_CA_DIR`]
pub const ELASTIC_CA_FILE: &str = "ca.crt";

/// The Secret holding the kube config a k8s scaler without a service account mounts
pub const KUBE_CONFIG_SECRET: &str = "kube-config";

/// The key in [`KUBE_CONFIG_SECRET`] holding the kube config
pub const KUBE_CONFIG_KEY: &str = "config";

/// A reference to a Secret holding a PEM encoded CA certificate
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
pub struct CaSecretRef {
    /// The name of the Secret in the `ThoriumCluster`'s namespace
    pub name: String,
    /// The key in the Secret containing the PEM encoded CA certificate
    #[serde(default = "default_ca_secret_key")]
    pub key: String,
}

/// A username/password pair stored in a Secret in the `ThoriumCluster`'s namespace
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
pub struct SecretCredentials {
    /// The name of the Secret containing the credentials
    pub name: String,
    /// A literal username to use
    pub username: Option<String>,
    /// The key in the Secret containing the username (takes precedence over `username`)
    pub username_key: Option<String>,
    /// The key in the Secret containing the password
    pub password_key: String,
}

/// The initial Thorium admin user to create
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
pub struct AdminBootstrap {
    /// The Secret containing the admin's username and password
    pub secret: SecretCredentials,
}

/// Settings for creating Thorium's Scylla role
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
pub struct ScyllaBootstrap {
    /// The Scylla superuser credentials to create the role with (defaults to cassandra/cassandra)
    pub admin_secret: Option<SecretCredentials>,
    /// Drop the default cassandra role once Thorium's role exists
    #[serde(default)]
    pub drop_default_role: bool,
}

/// Settings for creating Thorium's role and user in an externally managed Elasticsearch
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
pub struct ElasticBootstrap {
    /// The Elastic superuser credentials to create the role and user with
    pub admin_secret: SecretCredentials,
}

/// Privileged backend setup the operator performs when it or a referenced Secret changes
#[derive(Serialize, Deserialize, Clone, Debug, Default, JsonSchema)]
pub struct ThoriumBootstrap {
    /// Create an initial Thorium admin user
    pub admin: Option<AdminBootstrap>,
    /// Create Thorium's Scylla role
    pub scylla: Option<ScyllaBootstrap>,
    /// Create Thorium's Elastic role and user (only for an externally managed Elasticsearch)
    pub elastic: Option<ElasticBootstrap>,
}

/// The phase a `ThoriumCluster` is in
#[derive(Serialize, Deserialize, Clone, Copy, Debug, JsonSchema, PartialEq, Eq)]
pub enum ClusterPhase {
    /// The operator is deploying or updating this cluster
    Provisioning,
    /// Every component of this cluster has been deployed
    Ready,
    /// The last reconcile of this cluster failed
    Error,
}

/// The observed state of a `ThoriumCluster`
#[derive(Serialize, Deserialize, Clone, Debug, Default, JsonSchema, PartialEq, Eq)]
pub struct ThoriumClusterStatus {
    /// The current phase of this `ThoriumCluster`
    pub phase: Option<ClusterPhase>,
    /// Details about the current phase
    pub message: Option<String>,
    /// When the phase or message last changed (RFC3339)
    pub last_transition: Option<String>,
    /// The `metadata.generation` of the spec this status describes
    pub observed_generation: Option<i64>,
    /// A hash of the bootstrap spec and referenced Secrets the last completed bootstrap used
    pub bootstrap_hash: Option<String>,
}

/// Remove `required` constraints from a JSON schema so any field may be omitted
///
/// Constraints on the members of `oneOf`/`anyOf`/`allOf` are kept because they are what
/// distinguish enum variants from one another.
///
/// # Arguments
///
/// * `value` - The JSON schema to strip
/// * `union_member` - Whether this schema is a member of a `oneOf`/`anyOf`/`allOf`
fn strip_required(value: &mut serde_json::Value, union_member: bool) {
    // only schema objects can contain constraints
    let Some(object) = value.as_object_mut() else {
        return;
    };
    // drop this schema's required list unless it identifies a union member
    if !union_member
        && object
            .get("required")
            .is_some_and(serde_json::Value::is_array)
    {
        object.remove("required");
    }
    // walk the child schemas of this schema
    for (key, child) in object.iter_mut() {
        match key.as_str() {
            // each property is its own schema
            "properties" => {
                // strip every property's schema
                if let Some(properties) = child.as_object_mut() {
                    for property in properties.values_mut() {
                        strip_required(property, false);
                    }
                }
            }
            // array items and map values are schemas
            "items" | "additionalProperties" => strip_required(child, false),
            // union members keep their own required lists
            "oneOf" | "anyOf" | "allOf" => {
                // strip below each member without touching the member itself
                if let Some(members) = child.as_array_mut() {
                    for member in members {
                        strip_required(member, true);
                    }
                }
            }
            // any other keyword doesn't contain a schema
            _ => (),
        }
    }
}

/// Build the schema for a partial Thorium config
///
/// This is the [`thorium::Conf`] schema with every field optional so secrets can be
/// supplied separately through `config_secrets`.
///
/// # Arguments
///
/// * `generator` - The schema generator building the CRD
fn partial_conf_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    // generate the full config schema
    let schema = generator.subschema_for::<thorium::Conf>();
    // convert the schema to raw JSON so we can edit it
    let mut raw = schema.to_value();
    // make every field optional
    strip_required(&mut raw, false);
    // convert our edited schema back into a schema
    schemars::Schema::try_from(raw).expect("partial config schema is not a valid schema")
}

/// ThoriumCluster CRD definition
#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[kube(
    group = "sandia.gov",
    version = "v1",
    kind = "ThoriumCluster",
    namespaced,
    status = "ThoriumClusterStatus",
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Message","type":"string","jsonPath":".status.message","priority":1}"#,
    doc = "Custom resource representing a ThoriumCluster"
)]
pub struct ThoriumClusterSpec {
    /// List of Thorium components to deploy
    pub components: ThoriumComponents,
    /// URL to base container image for Thorium
    pub registry: String,
    /// Version or tag of image, overrides version
    #[serde(default = "default_version")]
    pub version: String,
    /// Auth tokens for Thorium's container registries
    ///
    /// These are written to the operator-owned `registry-token` pull secret (and the scaler's
    /// `docker-skopeo` secret) and every Thorium pod references `registry-token` when set.
    pub registry_auth: Option<BTreeMap<String, String>>,
    /// Existing image pull secrets in this namespace that every Thorium pod references
    ///
    /// The Helm chart lists its own pull secret (`thorium-image-pull`) here when it creates one.
    #[serde(default)]
    pub image_pull_secrets: Vec<String>,
    /// K8s image pull policies for Thorium components
    #[serde(default = "default_pull_policy")]
    pub image_pull_policy: String,
    /// Configuration options for Thorium components
    ///
    /// Any field may be omitted here and supplied by `config_secrets` instead.
    #[schemars(schema_with = "partial_conf_schema")]
    pub config: serde_json::Value,
    /// Secrets holding partial thorium.yml documents merged over `config` in order
    ///
    /// Each document is merged as a JSON merge patch (RFC 7386), so a null deletes a key and
    /// lists replace rather than append. YAML tags, non-string map keys, and merge keys are
    /// not supported, and values are not coerced the way thorium.yml is.
    #[serde(default)]
    pub config_secrets: Vec<ConfigSecretRef>,
    /// Privileged backend setup the operator performs for this cluster
    #[serde(default)]
    pub bootstrap: Option<ThoriumBootstrap>,
    /// A Secret holding the CA that signed an external Elasticsearch's certificate
    ///
    /// When set, the operator mounts it read-only at `/etc/thorium/elastic-ca/ca.crt` in every
    /// Thorium component and validates Elastic's certificate (chain and hostname) against it
    /// instead of the config's `cert_validation` and `insecure_certificates`. The operator's
    /// own pod must mount the same Secret at the same path (the Helm chart does this).
    #[serde(default)]
    pub elastic_ca_secret: Option<CaSecretRef>,
}

/// Methods operating on a ThoriumCluster resource
impl ThoriumCluster {
    /// Get the full image path within the registry
    pub fn get_image(&self) -> String {
        format!("{}:{}", self.spec.registry, self.spec.version)
    }

    /// Get target Thorium image version
    pub fn get_version(&self) -> String {
        self.spec.version.clone()
    }

    /// Get the scaler component spec
    pub fn get_scaler_spec(&self) -> Option<&ThoriumScaler> {
        if let Some(spec) = &self.spec.components.scaler {
            Some(&spec)
        } else {
            None
        }
    }

    /// Get the baremetal scaler component spec
    pub fn get_baremetal_scaler_spec(&self) -> Option<&ThoriumBaremetalScaler> {
        if let Some(spec) = &self.spec.components.baremetal_scaler {
            Some(&spec)
        } else {
            None
        }
    }

    /// Get the event handler component spec
    pub fn get_event_handler_spec(&self) -> Option<&ThoriumEventHandler> {
        if let Some(spec) = &self.spec.components.event_handler {
            Some(&spec)
        } else {
            None
        }
    }

    /// Get the search streamer component spec
    pub fn get_search_streamer_spec(&self) -> Option<&ThoriumSearchStreamer> {
        if let Some(spec) = &self.spec.components.search_streamer {
            Some(&spec)
        } else {
            None
        }
    }

    /// List the image pull secrets every Thorium pod should reference
    ///
    /// This is every secret in `image_pull_secrets` followed by the operator-owned
    /// `registry-token` secret when `registry_auth` is set.
    pub fn image_pull_secret_names(&self) -> Vec<String> {
        // start with the pull secrets the spec names
        let mut names = self.spec.image_pull_secrets.clone();
        // add the pull secret we render from registry_auth
        if self.spec.registry_auth.is_some() {
            names.push(REGISTRY_TOKEN_SECRET.to_owned());
        }
        // a secret named twice only needs to be referenced once
        let mut seen = std::collections::BTreeSet::new();
        names.retain(|name| seen.insert(name.clone()));
        names
    }

    /// Build the `imagePullSecrets` list for a Thorium pod spec
    pub fn image_pull_secrets(&self) -> serde_json::Value {
        // reference each pull secret by name
        self.image_pull_secret_names()
            .into_iter()
            .map(|name| json!({ "name": name }))
            .collect()
    }

    /// List the bootstrap Secrets this cluster references
    pub fn bootstrap_secret_names(&self) -> Vec<&str> {
        // get the bootstrap settings if any are set
        let Some(bootstrap) = &self.spec.bootstrap else {
            return Vec::new();
        };
        // collect the Secret each bootstrap step reads credentials from
        let admin = bootstrap.admin.as_ref().map(|admin| &admin.secret);
        let scylla = bootstrap
            .scylla
            .as_ref()
            .and_then(|scylla| scylla.admin_secret.as_ref());
        let elastic = bootstrap
            .elastic
            .as_ref()
            .map(|elastic| &elastic.admin_secret);
        [admin, scylla, elastic]
            .into_iter()
            .flatten()
            .map(|creds| creds.name.as_str())
            .collect()
    }

    /// Get the path the Elastic CA is mounted at, if this cluster has an Elastic CA Secret
    pub fn elastic_ca_path(&self) -> Option<std::path::PathBuf> {
        // the CA is only mounted when a CA Secret is set
        self.spec
            .elastic_ca_secret
            .as_ref()
            .map(|_| std::path::Path::new(ELASTIC_CA_DIR).join(ELASTIC_CA_FILE))
    }

    /// List the user Secrets the components mount whose content rolls them out
    ///
    /// This is the Elastic CA Secret when one is set, plus the `kube-config` Secret the k8s
    /// scaler mounts when it doesn't use a service account.
    pub fn mounted_secret_names(&self) -> Vec<&str> {
        // the Elastic CA is mounted by every component
        let elastic_ca = self
            .spec
            .elastic_ca_secret
            .as_ref()
            .map(|secret_ref| secret_ref.name.as_str());
        // the kube config is only mounted by a scaler without a service account
        let kube_config = self
            .spec
            .components
            .scaler
            .as_ref()
            .filter(|scaler| !scaler.service_account)
            .map(|_| KUBE_CONFIG_SECRET);
        [elastic_ca, kube_config].into_iter().flatten().collect()
    }

    /// Check whether this cluster's spec references a Secret by name
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the Secret in this cluster's namespace
    pub fn references_secret(&self, name: &str) -> bool {
        // check our config secrets, then our bootstrap secrets, then our mounted secrets
        self.spec
            .config_secrets
            .iter()
            .any(|secret_ref| secret_ref.name == name)
            || self.bootstrap_secret_names().contains(&name)
            || self.mounted_secret_names().contains(&name)
    }

    /// List the components in the ThoriumCluster
    pub fn list_component_names(&self) -> Vec<String> {
        // a list of component names
        let mut names: Vec<String> = Vec::new();
        // always add the api name
        names.push("api".to_owned());
        if let Some(_) = self.spec.components.scaler {
            names.push("scaler".to_owned());
        }
        if let Some(_) = self.spec.components.baremetal_scaler {
            names.push("baremetal-scaler".to_owned());
        }
        if let Some(_) = self.spec.components.search_streamer {
            names.push("search-streamer".to_owned());
        }
        if let Some(_) = self.spec.components.event_handler {
            names.push("event-handler".to_owned());
        }
        names
    }
}

/// Build ThoriumCluster stub for testing
#[allow(dead_code)]
pub async fn get_stub_resource() -> Result<ThoriumCluster, Error> {
    let raw_thorium_cluster_spec = json!({
        "name": "ThoriumExample",
        "nodes": ["server1", "server2", "server3"],
        "components": {
            "api": {"replicas": 1, "urls": ["some_url"], "ports": [80, 443]},
            "scaler": {},
            "baremetal_scaler": {},
            "search_streamer": {},
        },
        "registry": "url:port/path/to/image",
        "tag": "tag",
        "config": {},
        "config_secrets": [{"name": "thorium-config-secrets"}]
    });
    // build ThoriumCluster spec from json
    let thorium_cluster_spec: ThoriumClusterSpec =
        serde_json::from_value(raw_thorium_cluster_spec)?;
    // create the ThoriumCluster cr using the ThoriumCluster spec
    let thorium_cluster = ThoriumCluster::new("ThoriumProduction", thorium_cluster_spec);
    // print the ThoriumCluster as yaml
    println!(
        "{}",
        serde_norway::to_string(&thorium_cluster)
            .expect("could not turn ThoriumCluster to YAML string")
    );
    Ok(thorium_cluster)
}

/// Print the `ThoriumCluster` CRD as YAML
pub fn print_crd() {
    // serialize the CRD for this operator version
    let crd = serde_norway::to_string(&ThoriumCluster::crd())
        .expect("could not turn ThoriumCluster CRD to YAML string");
    // print the CRD to stdout
    print!("{crd}");
}

/// Set a `ThoriumCluster`'s status if it has changed
///
/// Failures are logged rather than returned so a status update never fails a reconcile.
///
/// # Arguments
///
/// * `client` - The kube client to patch the status with
/// * `cluster` - The `ThoriumCluster` to update
/// * `phase` - The new phase for this `ThoriumCluster`
/// * `message` - Details about the new phase
pub async fn set_status(
    client: &Client,
    cluster: &ThoriumCluster,
    phase: ClusterPhase,
    message: Option<String>,
) {
    // update the status without touching the bootstrap hash
    patch_status(client, cluster, phase, message, None).await;
}

/// Set a `ThoriumCluster`'s status along with the hash of its completed bootstrap
///
/// # Arguments
///
/// * `client` - The kube client to patch the status with
/// * `cluster` - The `ThoriumCluster` to update
/// * `phase` - The new phase for this `ThoriumCluster`
/// * `message` - Details about the new phase
/// * `bootstrap_hash` - The hash of the bootstrap inputs that were just applied
pub async fn set_status_with_bootstrap(
    client: &Client,
    cluster: &ThoriumCluster,
    phase: ClusterPhase,
    message: Option<String>,
    bootstrap_hash: String,
) {
    // update the status and record the bootstrap we completed
    patch_status(client, cluster, phase, message, Some(bootstrap_hash)).await;
}

/// Check whether a `ThoriumCluster`'s status already matches an update
///
/// # Arguments
///
/// * `cluster` - The `ThoriumCluster` whose current status to compare against
/// * `phase` - The new phase for this `ThoriumCluster`
/// * `message` - Details about the new phase
/// * `bootstrap_hash` - The new bootstrap hash, or None to keep the current one
fn status_unchanged(
    cluster: &ThoriumCluster,
    phase: ClusterPhase,
    message: Option<&String>,
    bootstrap_hash: Option<&String>,
) -> bool {
    // get the current status of this cluster
    let current = cluster.status.clone().unwrap_or_default();
    // the phase, message, and described generation must all match, and a new bootstrap hash
    // must match the recorded one
    current.phase == Some(phase)
        && current.message.as_ref() == message
        && current.observed_generation == cluster.metadata.generation
        && (bootstrap_hash.is_none() || current.bootstrap_hash.as_ref() == bootstrap_hash)
}

/// Patch a `ThoriumCluster`'s status subresource if anything in it has changed
///
/// # Arguments
///
/// * `client` - The kube client to patch the status with
/// * `cluster` - The `ThoriumCluster` to update
/// * `phase` - The new phase for this `ThoriumCluster`
/// * `message` - Details about the new phase
/// * `bootstrap_hash` - The new bootstrap hash, or None to keep the current one
async fn patch_status(
    client: &Client,
    cluster: &ThoriumCluster,
    phase: ClusterPhase,
    message: Option<String>,
    bootstrap_hash: Option<String>,
) {
    // get the spec generation this status describes
    let generation = cluster.metadata.generation;
    // skip the update if nothing changed so we don't trigger needless watch events
    if status_unchanged(cluster, phase, message.as_ref(), bootstrap_hash.as_ref()) {
        return;
    }
    // get this cluster's name and namespace
    let (Some(name), Some(namespace)) = (&cluster.metadata.name, &cluster.metadata.namespace)
    else {
        println!("Cannot set the status of a ThoriumCluster without a name and namespace");
        return;
    };
    // build the new status
    let mut status = json!({
        "phase": phase,
        "message": message,
        "last_transition": chrono::Utc::now().to_rfc3339(),
        "observed_generation": generation,
    });
    // only set a new bootstrap hash so the merge patch keeps the current one otherwise
    if let Some(bootstrap_hash) = bootstrap_hash {
        status["bootstrap_hash"] = json!(bootstrap_hash);
    }
    // build the status patch
    let patch = json!({ "status": status });
    // patch the status subresource
    let api: Api<ThoriumCluster> = Api::namespaced(client.clone(), namespace);
    if let Err(error) = api
        .patch_status(name, &PatchParams::default(), &Patch::Merge(&patch))
        .await
    {
        println!("Failed to set status of ThoriumCluster {namespace}/{name}: {error}");
    }
}

/// Create or update the ThoriumCluster CRD
pub async fn create_or_update(client: &Client) -> Result<(), Error> {
    let params = PatchParams::apply("thorium_cluster_apply").force();
    let crd_api: Api<CustomResourceDefinition> = Api::all(client.clone());
    // create the CRD for this operator version or patch it if it already exists
    crd_api
        .patch(CRD_NAME, &params, &Patch::Apply(ThoriumCluster::crd()))
        .await?;
    // wait for crd to be setup
    let established = await_condition(crd_api, CRD_NAME, conditions::is_crd_established());
    // timeout if CRD isn't setup in N seconds
    let result = tokio::time::timeout(tokio::time::Duration::from_secs(30), established).await;
    // ensure CRD is established before continuing on
    match result {
        Ok(_) => println!("ThoriumCluster CRD applied"),
        Err(_) => {
            return Err(Error::new(format!(
                "Timed out waiting for ThoriumCluster CRD to be established"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Find every `required` list outside of a `oneOf`/`anyOf`/`allOf` member
    ///
    /// # Arguments
    ///
    /// * `value` - The schema to search
    /// * `path` - The path to this schema
    /// * `found` - The paths of any `required` lists found
    fn find_required(value: &serde_json::Value, path: &str, found: &mut Vec<String>) {
        // only schema objects can contain constraints
        let Some(object) = value.as_object() else {
            return;
        };
        // record this schema's required list
        if object.contains_key("required") {
            found.push(path.to_owned());
        }
        // walk the child schemas of this schema
        for (key, child) in object {
            match key.as_str() {
                // each property is its own schema
                "properties" => {
                    // search every property's schema
                    if let Some(properties) = child.as_object() {
                        for (name, property) in properties {
                            find_required(property, &format!("{path}.{name}"), found);
                        }
                    }
                }
                // array items and map values are schemas
                "items" | "additionalProperties" => {
                    find_required(child, &format!("{path}.{key}"), found);
                }
                // union members may require fields to tell variants apart
                _ => (),
            }
        }
    }

    /// Get the schema of `spec.config` from the generated CRD
    fn config_schema() -> serde_json::Value {
        // serialize the CRD so we can walk its schema
        let crd = serde_json::to_value(ThoriumCluster::crd()).expect("CRD should serialize");
        // get the config schema from the first version
        crd["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]["properties"]
            ["config"]
            .clone()
    }

    /// The partial config schema has no required fields outside of enum variants
    #[test]
    fn crd_config_has_no_required() {
        // get the config schema
        let schema = config_schema();
        // make sure the schema was actually generated
        assert!(schema.get("properties").is_some(), "config schema is empty");
        // find any required lists that aren't part of a union
        let mut found = Vec::new();
        find_required(&schema, "spec.config", &mut found);
        assert!(
            found.is_empty(),
            "required fields under spec.config: {found:?}"
        );
    }

    /// Enum variants keep their required lists so they stay distinguishable
    #[test]
    fn strip_required_keeps_union_members() {
        // build a schema with a required field on a property and on a union member
        let mut schema = serde_json::json!({
            "required": ["a"],
            "properties": {
                "a": {"oneOf": [{"required": ["Grpc"], "properties": {"Grpc": {"required": ["x"]}}}]}
            }
        });
        // strip the required lists
        strip_required(&mut schema, false);
        // the top level list is gone
        assert!(schema.get("required").is_none());
        // the union member keeps its list but the schema below it doesn't
        let member = &schema["properties"]["a"]["oneOf"][0];
        assert_eq!(member["required"], serde_json::json!(["Grpc"]));
        assert!(member["properties"]["Grpc"].get("required").is_none());
    }

    /// The stub resource deserializes into a `ThoriumCluster`
    #[tokio::test]
    async fn stub_resource_deserializes() {
        // build the stub resource
        let cluster = get_stub_resource().await.expect("stub should deserialize");
        // the config secret key falls back to its default
        assert_eq!(cluster.spec.config_secrets[0].key, "thorium.yml");
        // components left out of the stub are unset
        assert!(cluster.spec.components.event_handler.is_none());
    }

    /// A cluster references its config secrets and every bootstrap secret
    #[test]
    fn references_secrets() {
        // build a cluster with config and bootstrap secrets
        let spec: ThoriumClusterSpec = serde_json::from_value(serde_json::json!({
            "components": {"api": {}},
            "registry": "registry/thorium",
            "config": {},
            "config_secrets": [{"name": "config"}],
            "bootstrap": {
                "admin": {"secret": {"name": "admin", "password_key": "password"}},
                "scylla": {"admin_secret": {"name": "scylla", "password_key": "password"}},
                "elastic": {"admin_secret": {"name": "elastic", "password_key": "elastic"}}
            }
        }))
        .expect("spec should deserialize");
        let cluster = ThoriumCluster::new("thorium", spec);
        // every referenced secret is found
        for name in ["config", "admin", "scylla", "elastic"] {
            assert!(
                cluster.references_secret(name),
                "{name} should be referenced"
            );
        }
        // other secrets are not
        assert!(!cluster.references_secret("thorium"));
        // the bootstrap secrets are listed in order
        assert_eq!(
            cluster.bootstrap_secret_names(),
            vec!["admin", "scylla", "elastic"]
        );
    }

    /// The Elastic CA and a scaler's kube config are referenced only when they are mounted
    #[test]
    fn references_mounted_secrets() {
        // build a cluster whose scaler uses a service account and has no Elastic CA
        let mut spec: ThoriumClusterSpec = serde_json::from_value(serde_json::json!({
            "components": {"api": {}, "scaler": {"service_account": true}},
            "registry": "registry/thorium",
            "config": {}
        }))
        .expect("spec should deserialize");
        let cluster = ThoriumCluster::new("thorium", spec.clone());
        // nothing is mounted from a user secret
        assert_eq!(cluster.mounted_secret_names(), Vec::<&str>::new());
        assert!(!cluster.references_secret(KUBE_CONFIG_SECRET));
        // a scaler without a service account mounts the kube config
        if let Some(scaler) = spec.components.scaler.as_mut() {
            scaler.service_account = false;
        }
        // an Elastic CA Secret is mounted by every component
        spec.elastic_ca_secret = Some(CaSecretRef {
            name: "elastic-ca".to_owned(),
            key: "tls.crt".to_owned(),
        });
        let cluster = ThoriumCluster::new("thorium", spec);
        assert_eq!(
            cluster.mounted_secret_names(),
            vec!["elastic-ca", KUBE_CONFIG_SECRET]
        );
        assert!(cluster.references_secret("elastic-ca"));
        assert!(cluster.references_secret(KUBE_CONFIG_SECRET));
    }

    /// Pods reference the spec's pull secrets plus `registry-token` when `registry_auth` is set
    #[test]
    fn pull_secret_names() {
        // build a spec with a chart pull secret and no registry auth
        let mut spec: ThoriumClusterSpec = serde_json::from_value(serde_json::json!({
            "components": {"api": {}},
            "registry": "registry/thorium",
            "config": {},
            "image_pull_secrets": ["thorium-image-pull"]
        }))
        .expect("spec should deserialize");
        let cluster = ThoriumCluster::new("thorium", spec.clone());
        // only the chart's secret is referenced
        assert_eq!(
            cluster.image_pull_secret_names(),
            vec!["thorium-image-pull"]
        );
        assert_eq!(
            cluster.image_pull_secrets(),
            serde_json::json!([{"name": "thorium-image-pull"}])
        );
        // registry auth adds the operator's own pull secret once
        spec.registry_auth = Some(BTreeMap::new());
        spec.image_pull_secrets
            .push(REGISTRY_TOKEN_SECRET.to_owned());
        let cluster = ThoriumCluster::new("thorium", spec);
        assert_eq!(
            cluster.image_pull_secret_names(),
            vec!["thorium-image-pull", REGISTRY_TOKEN_SECRET]
        );
    }

    /// The phase serializes as its variant name
    #[test]
    fn phase_serializes_as_name() {
        // serialize a status with a phase
        let status = ThoriumClusterStatus {
            phase: Some(ClusterPhase::Ready),
            ..Default::default()
        };
        let value = serde_json::to_value(status).expect("status should serialize");
        // the phase is a plain string
        assert_eq!(value["phase"], "Ready");
    }

    /// A status update is skipped only when the phase, message, generation, and any new
    /// bootstrap hash already match
    #[test]
    fn status_update_skips_no_ops() {
        // build a ready cluster at generation 2 with a recorded bootstrap
        let spec: ThoriumClusterSpec = serde_json::from_value(serde_json::json!({
            "components": {"api": {}},
            "registry": "registry/thorium",
            "config": {}
        }))
        .expect("spec should deserialize");
        let mut cluster = ThoriumCluster::new("thorium", spec);
        cluster.metadata.generation = Some(2);
        cluster.status = Some(ThoriumClusterStatus {
            phase: Some(ClusterPhase::Ready),
            message: Some("ready".to_owned()),
            observed_generation: Some(2),
            bootstrap_hash: Some("hash".to_owned()),
            ..Default::default()
        });
        let ready = "ready".to_owned();
        let hash = "hash".to_owned();
        // the same status is a no-op, with or without the same bootstrap hash
        assert!(status_unchanged(
            &cluster,
            ClusterPhase::Ready,
            Some(&ready),
            None
        ));
        assert!(status_unchanged(
            &cluster,
            ClusterPhase::Ready,
            Some(&ready),
            Some(&hash)
        ));
        // a new phase, message, or bootstrap hash is written
        assert!(!status_unchanged(
            &cluster,
            ClusterPhase::Provisioning,
            Some(&ready),
            None
        ));
        assert!(!status_unchanged(&cluster, ClusterPhase::Ready, None, None));
        let other = "other".to_owned();
        assert!(!status_unchanged(
            &cluster,
            ClusterPhase::Ready,
            Some(&ready),
            Some(&other)
        ));
        // a new spec generation is written even when nothing else changed
        cluster.metadata.generation = Some(3);
        assert!(!status_unchanged(
            &cluster,
            ClusterPhase::Ready,
            Some(&ready),
            None
        ));
        // a cluster without a status is always written
        cluster.status = None;
        assert!(!status_unchanged(
            &cluster,
            ClusterPhase::Ready,
            Some(&ready),
            None
        ));
    }
}
