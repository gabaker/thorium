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

pub const CRD_NAME: &str = "thoriumclusters.sandia.gov";

/// The label that links a Secret to the `ThoriumCluster` it configures
///
/// Changes to Secrets carrying this label trigger a reconcile of the `ThoriumCluster`
/// named by the label's value in the Secret's namespace.
pub const CLUSTER_SECRET_LABEL: &str = "thorium.sandia.gov/cluster";

/// Serde helper for the default key of a config secret
fn default_config_secret_key() -> String {
    "thorium.yml".to_owned()
}

/// A reference to a Secret holding a partial thorium.yml config
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
pub struct ConfigSecretRef {
    /// The name of the Secret in the `ThoriumCluster`'s namespace
    pub name: String,
    /// The key in the Secret containing the partial thorium.yml YAML document
    #[serde(default = "default_config_secret_key")]
    pub key: String,
}

/// A username/password pair stored in a Secret
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
pub struct SecretCredentials {
    /// The name of the Secret containing the credentials
    pub name: String,
    /// The namespace of the Secret (defaults to the `ThoriumCluster`'s namespace)
    pub namespace: Option<String>,
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

/// Settings for creating Thorium's Elastic role and user
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
pub struct ElasticBootstrap {
    /// The Elastic superuser credentials to create the role and user with
    pub admin_secret: SecretCredentials,
}

/// Backend setup the operator performs before and after deploying Thorium
#[derive(Serialize, Deserialize, Clone, Debug, Default, JsonSchema)]
pub struct ThoriumBootstrap {
    /// Create an initial Thorium admin user
    pub admin: Option<AdminBootstrap>,
    /// Create Thorium's Scylla role
    pub scylla: Option<ScyllaBootstrap>,
    /// Create Thorium's Elastic role, user, and indexes
    pub elastic: Option<ElasticBootstrap>,
}

/// The observed state of a `ThoriumCluster`
#[derive(Serialize, Deserialize, Clone, Debug, Default, JsonSchema, PartialEq, Eq)]
pub struct ThoriumClusterStatus {
    /// The current phase of this `ThoriumCluster` (Provisioning, Ready, or Error)
    pub phase: Option<String>,
    /// Details about the current phase
    pub message: Option<String>,
    /// When the phase or message last changed (RFC3339)
    pub last_transition: Option<String>,
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
    pub registry_auth: Option<BTreeMap<String, String>>,
    /// K8s image pull policies for Thorium components
    #[serde(default = "default_pull_policy")]
    pub image_pull_policy: String,
    /// Configuration options for Thorium components
    ///
    /// Any field may be omitted here and supplied by `config_secrets` instead.
    #[schemars(schema_with = "partial_conf_schema")]
    pub config: serde_json::Value,
    /// Secrets holding partial thorium.yml documents merged over `config` in order
    #[serde(default)]
    pub config_secrets: Vec<ConfigSecretRef>,
    /// Backend setup the operator performs for this cluster
    #[serde(default)]
    pub bootstrap: Option<ThoriumBootstrap>,
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
    phase: &str,
    message: Option<String>,
) {
    // get the current status of this cluster
    let current = cluster.status.clone().unwrap_or_default();
    // skip the update if nothing changed so we don't trigger needless watch events
    if current.phase.as_deref() == Some(phase) && current.message == message {
        return;
    }
    // get this cluster's name and namespace
    let (Some(name), Some(namespace)) = (&cluster.metadata.name, &cluster.metadata.namespace)
    else {
        println!("Cannot set the status of a ThoriumCluster without a name and namespace");
        return;
    };
    // build the new status
    let status = ThoriumClusterStatus {
        phase: Some(phase.to_owned()),
        message,
        last_transition: Some(chrono::Utc::now().to_rfc3339()),
    };
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
