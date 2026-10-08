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
use std::sync::{Arc, Mutex, PoisonError};
use thorium::Error;
use thorium::models::upgrades::{UpgradeApproval, UpgradeStatus};

/// A struct representing an environment variable
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Hash, Eq, PartialEq)]
pub struct EnvVar {
    /// The name of the environment variable
    pub name: String,
    /// The value of the environment variable (empty when unset)
    pub value: Option<String>,
}

/// A struct representing the cpu and memory resources of a container
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Hash, Eq, PartialEq)]
pub struct Resources {
    /// The CPU to request and limit the container to in millicpus (1000 is one core)
    pub cpu: u64,
    /// The memory to request and limit the container to in mebibytes
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

impl Resources {
    /// Convert a resource request to the quantities k8s expects
    ///
    /// CPU is in millicpus and memory in mebibytes.
    ///
    /// # Arguments
    ///
    /// * `raw` - The resource request to convert
    pub fn request_conv(raw: &Resources) -> BTreeMap<String, Quantity> {
        // express the cpu in millicpus and the memory in mebibytes
        BTreeMap::from([
            ("cpu".to_owned(), Quantity(format!("{}m", raw.cpu))),
            ("memory".to_owned(), Quantity(format!("{}Mi", raw.memory))),
        ])
    }
}

/// Serde helper for default environment variables
fn default_envs() -> Vec<EnvVar> {
    vec![
        EnvVar {
            name: "http_proxy".to_owned(),
            value: Some(String::new()),
        },
        EnvVar {
            name: "https_proxy".to_owned(),
            value: Some(String::new()),
        },
        EnvVar {
            name: "no_proxy".to_owned(),
            value: Some("localhost,cluster.local".to_owned()),
        },
        EnvVar {
            name: "HTTP_PROXY".to_owned(),
            value: Some(String::new()),
        },
        EnvVar {
            name: "HTTPS_PROXY".to_owned(),
            value: Some(String::new()),
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
    /// Args to pass to command in API container
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
    #[serde(default = "default_envs")]
    pub env: Vec<EnvVar>,
    /// Commands to run in scaler container
    #[serde(default = "default_scaler_cmd")]
    pub cmd: Vec<String>,
    /// Args to pass to command in scaler container
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
    #[serde(default = "default_envs")]
    pub env: Vec<EnvVar>,
    /// Commands to run in scaler container
    #[serde(default = "default_scaler_cmd")]
    pub cmd: Vec<String>,
    /// Args to pass to command in scaler container
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
    /// Args to pass to command in search-streamer container
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
    /// Args to pass to command in event-handler container
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

/// How the operator moves a `ThoriumCluster` between revisions
///
/// A cluster behind the operator's latest revision is only upgraded once a target is set,
/// either explicitly or by `auto_target_dev`; until then it stays in `UpgradeRequired` and
/// nothing in it is changed.
#[derive(Serialize, Deserialize, Clone, Debug, Default, JsonSchema)]
pub struct UpgradeSpec {
    /// The revision (`YYYY-MM-vNN`) to upgrade this cluster to
    #[schemars(pattern(thorium::models::upgrades::REVISION_PATTERN))]
    pub target_revision: Option<String>,
    /// Target the operator's latest revision when no target is set (for dev clusters)
    #[serde(default)]
    pub auto_target_dev: bool,
    /// Approvals, with backup confirmations, for steps that change data or need manual work
    #[serde(default)]
    pub approvals: Vec<UpgradeApproval>,
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
    /// This cluster is behind the operator's revision and waits for a target revision
    UpgradeRequired,
    /// The operator is running the steps that bring this cluster to its target revision
    Upgrading,
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
    /// The revision this cluster is at and any upgrade steps left to run
    pub upgrade: Option<UpgradeStatus>,
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
// this doc is also the spec description in the published CRD, so it stays plain text
#[allow(clippy::doc_markdown)]
#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[kube(
    group = "sandia.gov",
    version = "v1",
    kind = "ThoriumCluster",
    namespaced,
    status = "ThoriumClusterStatus",
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Revision","type":"string","jsonPath":".status.upgrade.current"}"#,
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
    /// How the operator upgrades this cluster between revisions
    #[serde(default)]
    pub upgrade: UpgradeSpec,
}

/// Methods operating on a `ThoriumCluster` resource
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
        self.spec.components.scaler.as_ref()
    }

    /// Get the baremetal scaler component spec
    pub fn get_baremetal_scaler_spec(&self) -> Option<&ThoriumBaremetalScaler> {
        self.spec.components.baremetal_scaler.as_ref()
    }

    /// Get the event handler component spec
    pub fn get_event_handler_spec(&self) -> Option<&ThoriumEventHandler> {
        self.spec.components.event_handler.as_ref()
    }

    /// Get the search streamer component spec
    pub fn get_search_streamer_spec(&self) -> Option<&ThoriumSearchStreamer> {
        self.spec.components.search_streamer.as_ref()
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

    /// List the deployment names of the components in the `ThoriumCluster`
    pub fn list_component_names(&self) -> Vec<String> {
        // the api is always deployed
        let mut names = vec!["api".to_owned()];
        // add each optional component the spec has
        let components = &self.spec.components;
        let optional = [
            (components.scaler.is_some(), "scaler"),
            (components.baremetal_scaler.is_some(), "baremetal-scaler"),
            (components.search_streamer.is_some(), "search-streamer"),
            (components.event_handler.is_some(), "event-handler"),
        ];
        names.extend(
            optional
                .into_iter()
                .filter(|(present, _)| *present)
                .map(|(_, name)| name.to_owned()),
        );
        names
    }
}

/// Build `ThoriumCluster` stub for testing
/// Print the `ThoriumCluster` CRD as YAML
pub fn print_crd() {
    // serialize the CRD for this operator version
    let crd = serde_norway::to_string(&ThoriumCluster::crd())
        .expect("could not turn ThoriumCluster CRD to YAML string");
    // print the CRD to stdout
    print!("{crd}");
}

/// A `ThoriumCluster` along with the status this operator last saw or wrote for it
///
/// A reconcile works from a snapshot of the cluster taken when it started, so comparing a new
/// status against that snapshot would skip a write that restores the starting status after this
/// reconcile changed it (a `Ready` cluster set to `Provisioning` while a rollout finishes would
/// never get back to `Ready`). Every status update made during a reconcile goes through one
/// tracker so updates are compared against the status last written instead.
#[derive(Debug)]
pub struct StatusTracker {
    /// The cluster as it was when the reconcile started
    cluster: Arc<ThoriumCluster>,
    /// The status as of the last successful patch, or the snapshot's status before any
    current: Mutex<ThoriumClusterStatus>,
}

impl StatusTracker {
    /// Start tracking the status of a `ThoriumCluster` from its current snapshot
    ///
    /// # Arguments
    ///
    /// * `cluster` - The cluster being reconciled
    pub fn new(cluster: Arc<ThoriumCluster>) -> Self {
        // start from the status the snapshot reports
        let current = Mutex::new(cluster.status.clone().unwrap_or_default());
        StatusTracker { cluster, current }
    }

    /// Get the cluster this tracker reports the status of
    pub fn cluster(&self) -> &Arc<ThoriumCluster> {
        &self.cluster
    }

    /// Get the status as of the last successful patch
    pub fn current(&self) -> ThoriumClusterStatus {
        // a panic while holding the lock can't leave the status half written, so a poisoned
        // lock is still safe to read
        self.current
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Record a status merge patch the API server accepted
    ///
    /// # Arguments
    ///
    /// * `patch` - The status fields that were merged
    fn record(&self, patch: &serde_json::Value) {
        // lock the status so concurrent updates apply one at a time
        let mut current = self.current.lock().unwrap_or_else(PoisonError::into_inner);
        // apply the patch the same way the API server merged it
        let mut merged = serde_json::to_value(&*current).unwrap_or_default();
        json_patch::merge(&mut merged, patch);
        // keep the merged status, which always fits since it came from our own fields
        if let Ok(status) = serde_json::from_value(merged) {
            *current = status;
        }
    }
}

/// Describe an error for a `ThoriumCluster`'s status message
///
/// The status already puts a failed cluster in the error phase, so a plain message is used
/// without the "Error: " prefix an error displays with; errors carrying a status code keep it.
///
/// # Arguments
///
/// * `error` - The error to describe
pub fn error_message(error: &Error) -> String {
    match (error.status(), error.msg()) {
        // a plain message reads on its own
        (None, Some(message)) => message,
        // anything else keeps its code or kind
        _ => error.to_string(),
    }
}

/// Set a `ThoriumCluster`'s status if it has changed
///
/// Failures are logged rather than returned so a status update never fails a reconcile.
///
/// # Arguments
///
/// * `client` - The kube client to patch the status with
/// * `tracker` - The cluster to update and the status last written for it
/// * `phase` - The new phase for this `ThoriumCluster`
/// * `message` - Details about the new phase
pub async fn set_status(
    client: &Client,
    tracker: &StatusTracker,
    phase: ClusterPhase,
    message: Option<String>,
) {
    // update the status without touching the bootstrap hash
    patch_status(client, tracker, phase, message, None).await;
}

/// Set a `ThoriumCluster`'s status along with the hash of its completed bootstrap
///
/// # Arguments
///
/// * `client` - The kube client to patch the status with
/// * `tracker` - The cluster to update and the status last written for it
/// * `phase` - The new phase for this `ThoriumCluster`
/// * `message` - Details about the new phase
/// * `bootstrap_hash` - The hash of the bootstrap inputs that were just applied
pub async fn set_status_with_bootstrap(
    client: &Client,
    tracker: &StatusTracker,
    phase: ClusterPhase,
    message: Option<String>,
    bootstrap_hash: String,
) {
    // update the status and record the bootstrap we completed
    patch_status(client, tracker, phase, message, Some(bootstrap_hash)).await;
}

/// Set a `ThoriumCluster`'s phase together with its upgrade progress if either changed
///
/// Failures are logged rather than returned so a status update never fails a reconcile.
///
/// # Arguments
///
/// * `client` - The kube client to patch the status with
/// * `tracker` - The cluster to update and the status last written for it
/// * `phase` - The new phase for this `ThoriumCluster`
/// * `message` - Details about the new phase
/// * `upgrade` - The upgrade progress to report
pub async fn set_upgrade_status(
    client: &Client,
    tracker: &StatusTracker,
    phase: ClusterPhase,
    message: Option<String>,
    upgrade: UpgradeStatus,
) {
    // skip the update if nothing changed so we don't trigger needless watch events
    let current = tracker.current();
    let generation = tracker.cluster.metadata.generation;
    if status_unchanged(&current, generation, phase, message.as_ref(), None)
        && current.upgrade.as_ref() == Some(&upgrade)
    {
        return;
    }
    // the transition time only moves when the phase or message does
    let transitioned = current.phase != Some(phase) || current.message != message;
    // patch the phase, message, and upgrade progress together
    let mut status = json!({
        "phase": phase,
        "message": message,
        "observed_generation": generation,
        "upgrade": upgrade,
    });
    if transitioned {
        status["last_transition"] = json!(chrono::Utc::now().to_rfc3339());
    }
    send_status_patch(client, tracker, status).await;
}

/// Set only a `ThoriumCluster`'s upgrade progress if it changed, leaving its phase alone
///
/// # Arguments
///
/// * `client` - The kube client to patch the status with
/// * `tracker` - The cluster to update and the status last written for it
/// * `upgrade` - The upgrade progress to report
pub async fn set_upgrade_progress(
    client: &Client,
    tracker: &StatusTracker,
    upgrade: UpgradeStatus,
) {
    // skip the update if the progress didn't change
    if tracker.current().upgrade.as_ref() == Some(&upgrade) {
        return;
    }
    // patch just the upgrade progress
    send_status_patch(client, tracker, json!({ "upgrade": upgrade })).await;
}

/// Send a merge patch to a `ThoriumCluster`'s status subresource, logging any failure
///
/// A patch the API server accepts is recorded in the tracker so later updates in the same
/// reconcile compare against it.
///
/// # Arguments
///
/// * `client` - The kube client to patch the status with
/// * `tracker` - The cluster to update and the status last written for it
/// * `status` - The status fields to merge
async fn send_status_patch(client: &Client, tracker: &StatusTracker, status: serde_json::Value) {
    // get this cluster's name and namespace
    let cluster = &tracker.cluster;
    let (Some(name), Some(namespace)) = (&cluster.metadata.name, &cluster.metadata.namespace)
    else {
        println!("Cannot set the status of a ThoriumCluster without a name and namespace");
        return;
    };
    // a merge patch replaces lists, so the planned steps are always replaced as a whole
    let patch = json!({ "status": status });
    let api: Api<ThoriumCluster> = Api::namespaced(client.clone(), namespace);
    match api
        .patch_status(name, &PatchParams::default(), &Patch::Merge(&patch))
        .await
    {
        // remember what we wrote so the next update compares against it
        Ok(_) => tracker.record(&status),
        // leave the tracked status alone so the next update retries this one
        Err(error) => {
            println!("Failed to set status of ThoriumCluster {namespace}/{name}: {error}");
        }
    }
}

/// Check whether a `ThoriumCluster`'s status already matches an update
///
/// # Arguments
///
/// * `current` - The status to compare against
/// * `generation` - The `metadata.generation` of the cluster's spec
/// * `phase` - The new phase for this `ThoriumCluster`
/// * `message` - Details about the new phase
/// * `bootstrap_hash` - The new bootstrap hash, or None to keep the current one
fn status_unchanged(
    current: &ThoriumClusterStatus,
    generation: Option<i64>,
    phase: ClusterPhase,
    message: Option<&String>,
    bootstrap_hash: Option<&String>,
) -> bool {
    // the phase, message, and described generation must all match, and a new bootstrap hash
    // must match the recorded one
    current.phase == Some(phase)
        && current.message.as_ref() == message
        && current.observed_generation == generation
        && (bootstrap_hash.is_none() || current.bootstrap_hash.as_ref() == bootstrap_hash)
}

/// Patch a `ThoriumCluster`'s status subresource if anything in it has changed
///
/// # Arguments
///
/// * `client` - The kube client to patch the status with
/// * `tracker` - The cluster to update and the status last written for it
/// * `phase` - The new phase for this `ThoriumCluster`
/// * `message` - Details about the new phase
/// * `bootstrap_hash` - The new bootstrap hash, or None to keep the current one
async fn patch_status(
    client: &Client,
    tracker: &StatusTracker,
    phase: ClusterPhase,
    message: Option<String>,
    bootstrap_hash: Option<String>,
) {
    // get the spec generation this status describes
    let generation = tracker.cluster.metadata.generation;
    // skip the update if nothing changed since our last write so we don't trigger needless
    // watch events
    let current = tracker.current();
    if status_unchanged(
        &current,
        generation,
        phase,
        message.as_ref(),
        bootstrap_hash.as_ref(),
    ) {
        return;
    }
    // the transition time only moves when the phase or message does
    let transitioned = current.phase != Some(phase) || current.message != message;
    // build the new status
    let mut status = json!({
        "phase": phase,
        "message": message,
        "observed_generation": generation,
    });
    if transitioned {
        status["last_transition"] = json!(chrono::Utc::now().to_rfc3339());
    }
    // only set a new bootstrap hash so the merge patch keeps the current one otherwise
    if let Some(bootstrap_hash) = bootstrap_hash {
        status["bootstrap_hash"] = json!(bootstrap_hash);
    }
    // patch the status subresource
    send_status_patch(client, tracker, status).await;
}

/// Create or update the `ThoriumCluster` CRD
///
/// The CRD is applied server-side with the same field manager the chart's conversion script
/// uses, forcing ownership so this operator's schema always wins.
///
/// # Arguments
///
/// * `client` - The kube client for the k8s cluster to apply the CRD in
pub async fn create_or_update(client: &Client) -> Result<(), Error> {
    // apply the CRD as our field manager
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
        Ok(Ok(_)) => {
            println!("ThoriumCluster CRD applied");
            Ok(())
        }
        Ok(Err(error)) => Err(Error::new(format!(
            "Failed waiting for ThoriumCluster CRD to be established: {error}"
        ))),
        Err(_) => Err(Error::new(
            "Timed out waiting for ThoriumCluster CRD to be established",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Status messages drop the "Error: " prefix of plain errors but keep status codes
    #[test]
    fn error_messages_without_prefix() {
        // a plain error reads as its message
        let plain = Error::new("ConfigMap thorium/thorium-upgrade-state is not valid");
        assert_eq!(
            error_message(&plain),
            "ConfigMap thorium/thorium-upgrade-state is not valid"
        );
        // an error with a status code keeps it
        let coded = Error::Thorium {
            code: reqwest::StatusCode::NOT_FOUND,
            msg: Some("config secret missing not found".to_owned()),
        };
        assert_eq!(error_message(&coded), coded.to_string());
        assert!(error_message(&coded).contains("404"));
    }

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

    /// Check whether a cluster's snapshot status already matches an update
    ///
    /// # Arguments
    ///
    /// * `cluster` - The cluster whose snapshot status to compare against
    /// * `phase` - The new phase
    /// * `message` - The new message
    /// * `bootstrap_hash` - The new bootstrap hash, or None to keep the current one
    fn snapshot_unchanged(
        cluster: &ThoriumCluster,
        phase: ClusterPhase,
        message: Option<&String>,
        bootstrap_hash: Option<&String>,
    ) -> bool {
        // compare against the status the snapshot reports
        let current = cluster.status.clone().unwrap_or_default();
        status_unchanged(
            &current,
            cluster.metadata.generation,
            phase,
            message,
            bootstrap_hash,
        )
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
        assert!(snapshot_unchanged(
            &cluster,
            ClusterPhase::Ready,
            Some(&ready),
            None
        ));
        assert!(snapshot_unchanged(
            &cluster,
            ClusterPhase::Ready,
            Some(&ready),
            Some(&hash)
        ));
        // a new phase, message, or bootstrap hash is written
        assert!(!snapshot_unchanged(
            &cluster,
            ClusterPhase::Provisioning,
            Some(&ready),
            None
        ));
        assert!(!snapshot_unchanged(
            &cluster,
            ClusterPhase::Ready,
            None,
            None
        ));
        let other = "other".to_owned();
        assert!(!snapshot_unchanged(
            &cluster,
            ClusterPhase::Ready,
            Some(&ready),
            Some(&other)
        ));
        // a new spec generation is written even when nothing else changed
        cluster.metadata.generation = Some(3);
        assert!(!snapshot_unchanged(
            &cluster,
            ClusterPhase::Ready,
            Some(&ready),
            None
        ));
        // a cluster without a status is always written
        cluster.status = None;
        assert!(!snapshot_unchanged(
            &cluster,
            ClusterPhase::Ready,
            Some(&ready),
            None
        ));
    }

    /// Build a ready cluster in the test namespace with a recorded bootstrap
    fn ready_cluster() -> ThoriumCluster {
        // a cluster that was ready at generation 1 when the reconcile started
        let mut cluster = crate::k8s::clusters::tests::namespaced_cluster(
            crate::k8s::clusters::tests::full_spec(),
        );
        cluster.metadata.generation = Some(1);
        cluster.status = Some(ThoriumClusterStatus {
            phase: Some(ClusterPhase::Ready),
            observed_generation: Some(1),
            bootstrap_hash: Some("hash".to_owned()),
            ..ThoriumClusterStatus::default()
        });
        cluster
    }

    /// Build a fake kube API that accepts status patches for the test cluster
    ///
    /// # Arguments
    ///
    /// * `cluster` - The cluster the patches answer with
    fn accepting_status(cluster: &ThoriumCluster) -> crate::k8s::clusters::tests::FakeKube {
        crate::k8s::clusters::tests::FakeKube::default().route(
            "PATCH",
            "/apis/sandia.gov/v1/namespaces/thorium/thoriumclusters/thorium/status",
            200,
            serde_json::to_value(cluster).expect("cluster serializes"),
        )
    }

    /// Get the status fields of every patch a fake kube API received
    ///
    /// # Arguments
    ///
    /// * `fake` - The fake kube API that received the patches
    fn patched(fake: &crate::k8s::clusters::tests::FakeKube) -> Vec<serde_json::Value> {
        fake.writes()
            .into_iter()
            .map(|request| request.body["status"].clone())
            .collect()
    }

    /// A ready cluster set to provisioning while a rollout finishes gets back to ready in the
    /// same reconcile even though ready is what the reconcile started from
    #[tokio::test]
    async fn ready_restored_after_provisioning() {
        // track a ready cluster like a reconcile does
        let cluster = ready_cluster();
        let fake = accepting_status(&cluster);
        let client = fake.client();
        let tracker = StatusTracker::new(Arc::new(cluster));
        // the rollout wait reports provisioning
        set_status(
            &client,
            &tracker,
            ClusterPhase::Provisioning,
            Some("Waiting for api to roll out".to_owned()),
        )
        .await;
        // the finished reconcile reports ready with the same bootstrap it started with
        set_status_with_bootstrap(
            &client,
            &tracker,
            ClusterPhase::Ready,
            None,
            "hash".to_owned(),
        )
        .await;
        // both were written and the cluster ends ready without a message
        let patches = patched(&fake);
        assert_eq!(patches.len(), 2, "{patches:?}");
        assert_eq!(patches[0]["phase"], "Provisioning");
        assert_eq!(patches[1]["phase"], "Ready");
        assert_eq!(patches[1]["message"], serde_json::Value::Null);
        // the tracker reflects the last write and a repeat of it is skipped
        assert_eq!(tracker.current().phase, Some(ClusterPhase::Ready));
        assert_eq!(tracker.current().message, None);
        set_status(&client, &tracker, ClusterPhase::Ready, None).await;
        assert_eq!(patched(&fake).len(), 2);
    }

    /// Rewriting the same phase and message with a new bootstrap hash or generation keeps the
    /// transition time, while a new phase or message moves it
    #[tokio::test]
    async fn transition_time_moves_only_with_phase_or_message() {
        // track a ready cluster like a reconcile does
        let cluster = ready_cluster();
        let fake = accepting_status(&cluster);
        let client = fake.client();
        let tracker = StatusTracker::new(Arc::new(cluster));
        // a new bootstrap hash alone is written without moving the transition time
        set_status_with_bootstrap(
            &client,
            &tracker,
            ClusterPhase::Ready,
            None,
            "new".to_owned(),
        )
        .await;
        // a new message moves it
        set_status(
            &client,
            &tracker,
            ClusterPhase::Ready,
            Some("note".to_owned()),
        )
        .await;
        // and so does a new phase
        set_status(
            &client,
            &tracker,
            ClusterPhase::Error,
            Some("note".to_owned()),
        )
        .await;
        let patches = patched(&fake);
        assert_eq!(patches.len(), 3, "{patches:?}");
        assert!(patches[0].get("last_transition").is_none(), "{patches:?}");
        assert_eq!(patches[0]["bootstrap_hash"], "new");
        assert!(patches[1].get("last_transition").is_some(), "{patches:?}");
        assert!(patches[2].get("last_transition").is_some(), "{patches:?}");
    }

    /// A status patch the API server rejects isn't recorded, so the server's status is still
    /// what later updates compare against
    #[tokio::test]
    async fn failed_status_patch_not_recorded() {
        // a fake kube API that rejects every status patch
        let fake = crate::k8s::clusters::tests::FakeKube::default();
        let client = fake.client();
        let tracker = StatusTracker::new(Arc::new(ready_cluster()));
        // the provisioning patch fails
        set_status(&client, &tracker, ClusterPhase::Provisioning, None).await;
        assert_eq!(tracker.current().phase, Some(ClusterPhase::Ready));
        // so the cluster is still ready and ready isn't written again
        set_status(&client, &tracker, ClusterPhase::Ready, None).await;
        assert_eq!(fake.writes().len(), 1);
    }

    /// Get the schema of `spec.<field>` from the generated CRD
    ///
    /// # Arguments
    ///
    /// * `field` - The spec field to get
    fn spec_schema(field: &str) -> serde_json::Value {
        // serialize the CRD so we can walk its schema
        let crd = serde_json::to_value(ThoriumCluster::crd()).expect("CRD should serialize");
        crd["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]["properties"]
            [field]
            .clone()
    }

    /// The CRD validates target revisions with the catalog's pattern
    #[test]
    fn crd_target_revision_has_pattern() {
        // get the upgrade schema
        let upgrade = spec_schema("upgrade");
        // the target revision is validated by the shared revision pattern
        assert_eq!(
            upgrade["properties"]["target_revision"]["pattern"],
            thorium::models::upgrades::REVISION_PATTERN
        );
        // auto targeting is a plain boolean
        assert_eq!(upgrade["properties"]["auto_target_dev"]["type"], "boolean");
        // approvals keep their required fields since only spec.config is made optional
        let mut required = upgrade["properties"]["approvals"]["items"]["required"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        required.sort_by_key(ToString::to_string);
        assert_eq!(
            required,
            vec![serde_json::json!("backup"), serde_json::json!("step")]
        );
    }

    /// The status schema lists the upgrade phases and step states and the CRD prints the
    /// revision
    #[test]
    fn crd_status_lists_upgrade_phases() {
        // serialize the CRD so we can walk its schema
        let crd = serde_json::to_value(ThoriumCluster::crd()).expect("CRD should serialize");
        let version = &crd["spec"]["versions"][0];
        let status_schema =
            &version["schema"]["openAPIV3Schema"]["properties"]["status"]["properties"];
        // every phase is allowed, including the upgrade phases
        let phases = status_schema["phase"]["enum"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        for phase in [
            "Provisioning",
            "Ready",
            "Error",
            "UpgradeRequired",
            "Upgrading",
        ] {
            assert!(
                phases.contains(&serde_json::json!(phase)),
                "{phase} missing"
            );
        }
        // every step state is allowed
        let states =
            status_schema["upgrade"]["properties"]["steps"]["items"]["properties"]["state"]["enum"]
                .as_array()
                .cloned()
                .unwrap_or_default();
        assert_eq!(states.len(), 6);
        // the revision is printed from the upgrade status
        let columns = version["additionalPrinterColumns"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert!(columns.iter().any(|column| column["name"] == "Revision"
            && column["jsonPath"] == ".status.upgrade.current"));
    }

    /// A minimal spec waits for an upgrade target with no approvals and takes the defaults
    /// the components expect
    #[test]
    fn minimal_spec_defaults() {
        // a spec with only the required fields and a config secret without a key
        let spec: ThoriumClusterSpec = serde_json::from_value(serde_json::json!({
            "components": {"api": {}},
            "registry": "registry/thorium",
            "config": {},
            "config_secrets": [{"name": "thorium-config-secrets"}]
        }))
        .expect("spec should deserialize");
        // the config secret key falls back to the chart's key
        assert_eq!(spec.config_secrets[0].key, "thorium.yml");
        // components left out are unset
        assert!(spec.components.scaler.is_none());
        assert!(spec.components.event_handler.is_none());
        // no target, no auto targeting, and no approvals
        assert_eq!(spec.upgrade.target_revision, None);
        assert!(!spec.upgrade.auto_target_dev);
        assert_eq!(spec.upgrade.approvals, Vec::new());
        // the remaining defaults match what the components expect
        assert_eq!(spec.version, "latest");
        assert_eq!(spec.image_pull_policy, "Always");
        assert_eq!(spec.image_pull_secrets, Vec::<String>::new());
        assert!(spec.elastic_ca_secret.is_none());
    }

    /// A cluster as the Helm chart's template renders it deserializes with every field kept
    #[test]
    fn chart_rendered_cr_deserializes() {
        // the fields deploy/charts/thorium/charts/operator/templates/thoriumcluster.yaml writes
        let spec: ThoriumClusterSpec = serde_json::from_value(serde_json::json!({
            "components": {"api": {"replicas": 2}, "scaler": {"service_account": true}},
            "registry": "registry/thorium",
            "version": "1.8.1",
            "image_pull_policy": "IfNotPresent",
            "image_pull_secrets": ["thorium-image-pull"],
            "config": {"thorium": {"cors": {"insecure": false}}},
            "config_secrets": [{"name": "thorium-config-secrets", "key": "thorium.yml"}],
            "bootstrap": {
                "admin": {"secret": {"name": "thorium-admin", "username_key": "username", "password_key": "password"}},
                "scylla": {
                    "drop_default_role": true,
                    "admin_secret": {"name": "scylla-admin", "username": "cassandra", "password_key": "password"}
                },
                "elastic": {"admin_secret": {"name": "elastic-es-elastic-user", "username": "elastic", "password_key": "elastic"}}
            },
            "elastic_ca_secret": {"name": "elastic-ca", "key": "ca.crt"},
            "upgrade": {
                "target_revision": "2026-10-v02",
                "auto_target_dev": false,
                "approvals": [{"step": "elastic-reindex-keyword-mappings", "backup": "snap-1"}]
            }
        }))
        .expect("chart spec should deserialize");
        // the upgrade settings come through
        assert_eq!(spec.upgrade.target_revision.as_deref(), Some("2026-10-v02"));
        assert_eq!(spec.upgrade.approvals[0].backup, "snap-1");
        // and so do the other chart fields
        assert_eq!(spec.components.api.replicas, 2);
        assert_eq!(spec.config_secrets[0].name, "thorium-config-secrets");
        assert_eq!(
            spec.bootstrap
                .as_ref()
                .and_then(|bootstrap| bootstrap.admin.as_ref())
                .map(|admin| admin.secret.name.as_str()),
            Some("thorium-admin")
        );
        // the Scylla and Elastic bootstrap settings the chart renders come through
        let bootstrap = spec.bootstrap.as_ref().expect("bootstrap");
        let scylla = bootstrap.scylla.as_ref().expect("scylla bootstrap");
        assert!(scylla.drop_default_role);
        assert_eq!(
            scylla
                .admin_secret
                .as_ref()
                .and_then(|secret| secret.username.as_deref()),
            Some("cassandra")
        );
        let elastic = bootstrap.elastic.as_ref().expect("elastic bootstrap");
        assert_eq!(elastic.admin_secret.password_key, "elastic");
    }

    /// The image and component list follow the spec
    #[test]
    fn component_names_and_image() {
        // a cluster with every component
        let cluster = crate::k8s::clusters::tests::cluster_from_spec(
            crate::k8s::clusters::tests::full_spec(),
        );
        assert_eq!(cluster.get_image(), "registry/thorium:1.8.1");
        assert_eq!(
            cluster.list_component_names(),
            [
                "api",
                "scaler",
                "baremetal-scaler",
                "search-streamer",
                "event-handler"
            ]
        );
        // only the api is always deployed
        let api_only = crate::k8s::clusters::tests::cluster_from_spec(serde_json::json!({
            "components": {"api": {}},
            "registry": "registry/thorium",
            "config": {}
        }));
        assert_eq!(api_only.list_component_names(), ["api"]);
        assert!(api_only.get_scaler_spec().is_none());
    }

    /// Build an empty upgrade progress at a revision
    ///
    /// # Arguments
    ///
    /// * `current` - The revision to report
    fn progress_at(current: &str) -> UpgradeStatus {
        UpgradeStatus {
            current: Some(current.to_owned()),
            ..UpgradeStatus::default()
        }
    }

    /// Upgrade statuses are only written when they change and move the transition time only
    /// when the phase or message does
    #[tokio::test]
    async fn upgrade_status_patches() {
        // a cluster already reporting a held upgrade at generation 1
        let mut cluster = crate::k8s::clusters::tests::namespaced_cluster(
            crate::k8s::clusters::tests::full_spec(),
        );
        cluster.metadata.generation = Some(1);
        cluster.status = Some(ThoriumClusterStatus {
            phase: Some(ClusterPhase::UpgradeRequired),
            message: Some("held".to_owned()),
            observed_generation: Some(1),
            upgrade: Some(progress_at("2026-10-v01")),
            ..ThoriumClusterStatus::default()
        });
        let fake = crate::k8s::clusters::tests::FakeKube::default();
        let client = fake.client();
        let tracker = StatusTracker::new(Arc::new(cluster));
        // the same status and progress aren't written again
        set_upgrade_status(
            &client,
            &tracker,
            ClusterPhase::UpgradeRequired,
            Some("held".to_owned()),
            progress_at("2026-10-v01"),
        )
        .await;
        set_upgrade_progress(&client, &tracker, progress_at("2026-10-v01")).await;
        assert!(fake.requests().is_empty());
        // new progress alone is written without moving the transition time
        set_upgrade_status(
            &client,
            &tracker,
            ClusterPhase::UpgradeRequired,
            Some("held".to_owned()),
            progress_at("2026-10-v02"),
        )
        .await;
        // a new phase moves the transition time
        set_upgrade_status(
            &client,
            &tracker,
            ClusterPhase::Upgrading,
            Some("held".to_owned()),
            progress_at("2026-10-v01"),
        )
        .await;
        // new progress through the progress update only patches the progress
        set_upgrade_progress(&client, &tracker, progress_at("2026-10-v02")).await;
        let patches = fake
            .writes()
            .into_iter()
            .map(|request| request.body["status"].clone())
            .collect::<Vec<_>>();
        assert_eq!(patches.len(), 3);
        assert!(patches[0].get("last_transition").is_none());
        assert_eq!(patches[0]["upgrade"]["current"], "2026-10-v02");
        assert_eq!(patches[0]["observed_generation"], 1);
        assert_eq!(patches[1]["phase"], "Upgrading");
        assert!(patches[1].get("last_transition").is_some());
        assert_eq!(
            patches[2],
            serde_json::json!({"upgrade": progress_at("2026-10-v02")})
        );
    }

    /// A cluster without a name or namespace can't have its status patched
    #[tokio::test]
    async fn status_needs_name_and_namespace() {
        // a cluster that was never placed in a namespace
        let cluster = crate::k8s::clusters::tests::cluster_from_spec(
            crate::k8s::clusters::tests::full_spec(),
        );
        let fake = crate::k8s::clusters::tests::FakeKube::default();
        // the update is dropped without a request
        let tracker = StatusTracker::new(Arc::new(cluster));
        set_status(&fake.client(), &tracker, ClusterPhase::Ready, None).await;
        assert!(fake.requests().is_empty());
    }
}
