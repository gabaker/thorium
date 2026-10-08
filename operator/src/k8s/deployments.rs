use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::{Pod, PodStatus};
use kube::Api;
use kube::api::{DeleteParams, ListParams, Patch, PatchParams, PostParams};
use serde_json::Value;
use thorium::conf::K8sHostAliases;
use thorium::{Error, client::Basic};

use super::clusters::ClusterMeta;
use super::crds::{self, ThoriumCluster};

/// The pod template annotation holding the sha256 of the rendered thorium.yml
const CONFIG_HASH_ANNOTATION: &str = "thorium.sandia.gov/config-hash";

/// The pod template annotation holding the sha256 of everything a component mounts besides
/// thorium.yml (its keys.yml, the login banner, the Elastic CA, and the scaler's kube config)
const MOUNTS_HASH_ANNOTATION: &str = "thorium.sandia.gov/mounts-hash";

/// The uid and gid of the `thorium` user the Thorium image creates and runs its components as
pub const THORIUM_UID: i64 = 10001;

/// The home directory of the image's `thorium` user, where the scaler reads its kube config
/// and registry credentials
const THORIUM_HOME: &str = "/home/thorium";

/// How often to check on a deployment that is rolling out
const ROLLOUT_POLL: std::time::Duration = std::time::Duration::from_secs(3);

/// The most pod problems to list when a rollout doesn't finish
const MAX_POD_PROBLEMS: usize = 3;

/// The container waiting reasons that mean a pod is failing rather than just starting
const FAILING_REASONS: [&str; 5] = [
    "CrashLoopBackOff",
    "ErrImagePull",
    "ImagePullBackOff",
    "CreateContainerError",
    "CreateContainerConfigError",
];

/// Build the api deployment
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `host_aliases` - The host aliases to add to each pod
// the deployment is one JSON document that reads best kept whole
#[allow(clippy::too_many_lines)]
fn api(
    meta: &ClusterMeta,
    host_aliases: &Vec<K8sHostAliases>,
) -> Result<Deployment, serde_json::Error> {
    // get the api component spec
    let api_spec = &meta.cluster.spec.components.api;
    // reference the chart's pull secret and the one rendered from registry_auth
    let image_pull_secrets = meta.cluster.image_pull_secrets();
    // build the api deployment template
    let template = serde_json::json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": {
            "namespace": meta.cluster.metadata.namespace.clone(),
            "name": "api",
            "labels": {
                "app": "api",
                "version": meta.cluster.get_version(),
            }
        },
        "spec": {
            "replicas": api_spec.replicas,
            "selector": {
                "matchLabels": {
                    "app": "api",
                }
            },
            "template": {
                "metadata": {
                    "labels": {
                        "app": "api",
                        "version": meta.cluster.get_version(),
                    }
                },
                "spec": {
                    // the API never calls the k8s API, so it doesn't get a service account token
                    "automountServiceAccountToken": false,
                    // let the non-root API bind its configured port (80 by default), as Docker
                    // allows in every container; this sysctl is namespaced and safe, so restricted
                    // Pod Security admits it
                    "securityContext": {
                        "sysctls": [{"name": "net.ipv4.ip_unprivileged_port_start", "value": "0"}]
                    },
                    "containers": [
                        {
                            "name": "api",
                            "image": meta.cluster.get_image(),
                            "command": api_spec.cmd.clone(),
                            "args": api_spec.args.clone(),
                            "imagePullPolicy": meta.cluster.spec.image_pull_policy.clone(),
                            "resources": {
                                "limits": crds::Resources::request_conv(&api_spec.resources),
                                "requests": crds::Resources::request_conv(&api_spec.resources),
                            },
                            "env": api_spec.env.clone(),
                            "livenessProbe": {
                                "httpGet": {
                                    "path": "/health",
                                    "port": 80,
                                    "scheme": "HTTP"
                                },
                                "initialDelaySeconds": 10,
                                "periodSeconds": 10,
                                "successThreshold": 1,
                                "timeoutSeconds": 1,
                                "failureThreshold": 1,
                            },
                            "readinessProbe": {
                                "httpGet": {
                                    "path": "/health",
                                    "port": 80,
                                    "scheme": "HTTP"
                                },
                                "initialDelaySeconds": 5,
                                "periodSeconds": 3,
                                "successThreshold": 1,
                                "timeoutSeconds": 1,
                                "failureThreshold": 10,
                            },
                            "volumeMounts": [
                                {
                                    "name": "config",
                                    "mountPath": "/conf/thorium.yml",
                                    "subPath": "thorium.yml"
                                },
                                {
                                    "name": "banner",
                                    "mountPath": "/app/banner.txt",
                                    "subPath": "banner.txt"
                                }
                            ]
                        }
                    ],
                    "hostAliases": host_aliases,
                    "volumes": [
                        {
                            "name": "config",
                            "secret": {
                                "secretName": "thorium"
                            }
                        },
                        {
                            "name": "banner",
                            "configMap": {
                                "name": "banner",
                                "optional": true,
                            }
                        }
                    ],
                    "imagePullSecrets": image_pull_secrets
                }
            }
        }
    });
    // parse this template into a deployment
    serde_json::from_value(template)
}

/// Build JSON template for the k8s scaler deployment
///
/// Returns None when the spec has no k8s scaler.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `host_aliases` - The host aliases to add to each pod
// the deployment template is one long JSON document
#[allow(clippy::too_many_lines)]
fn scaler_template(meta: &ClusterMeta, host_aliases: &Vec<K8sHostAliases>) -> Option<Value> {
    // build nothing for a component that isn't in the spec
    let scaler_spec = meta.cluster.get_scaler_spec()?;
    // reference the chart's pull secret and the one rendered from registry_auth
    let image_pull_secrets = meta.cluster.image_pull_secrets();
    // every scaler mounts the config and its keys
    let mut volumes = vec![
        serde_json::json!({"name": "config", "secret": {"secretName": "thorium"}}),
        serde_json::json!({"name": "keys", "secret": {"secretName": "keys"}}),
    ];
    let mut volume_mounts = vec![
        serde_json::json!({"name": "config", "mountPath": "/conf/thorium.yml", "subPath": "thorium.yml"}),
        serde_json::json!({"name": "keys", "mountPath": "/keys/keys.yml", "subPath": "keys.yml"}),
    ];
    // only include skopeo secret when registry auth is configured
    if meta.cluster.spec.registry_auth.is_some() {
        volumes.push(serde_json::json!({
            "name": "docker-skopeo",
            "secret": {"secretName": "docker-skopeo"}
        }));
        volume_mounts.push(serde_json::json!({
            "name": "docker-skopeo",
            "mountPath": format!("{THORIUM_HOME}/.docker")
        }));
    }
    // without a service account the scaler reaches k8s through a user created kube-config secret
    if !scaler_spec.service_account {
        volumes.push(serde_json::json!({
            "name": "kube-config",
            "secret": {"secretName": crds::KUBE_CONFIG_SECRET}
        }));
        volume_mounts.push(serde_json::json!({
            "name": "kube-config",
            "mountPath": format!("{THORIUM_HOME}/.kube/config"),
            "subPath": crds::KUBE_CONFIG_KEY
        }));
    }
    // the scaler reads its registry credentials from $HOME and its kube config from $KUBECONFIG,
    // both mounted above, so the operator sets these over any value in the spec; a stored spec
    // can still carry the old CRD default /root/.kube/config, which the non-root scaler can't read
    let mut env: Vec<crds::EnvVar> = scaler_spec
        .env
        .iter()
        .filter(|var| var.name != "HOME" && var.name != "KUBECONFIG")
        .cloned()
        .collect();
    env.push(crds::EnvVar {
        name: "HOME".to_owned(),
        value: Some(THORIUM_HOME.to_owned()),
    });
    // without a kube config the scaler falls back to its service account
    if !scaler_spec.service_account {
        env.push(crds::EnvVar {
            name: "KUBECONFIG".to_owned(),
            value: Some(format!("{THORIUM_HOME}/.kube/config")),
        });
    }
    // build the scaler deployment
    Some(serde_json::json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": {
            "namespace": meta.cluster.metadata.namespace.clone(),
            "name": "scaler",
            "labels": {
                "app": "scaler",
                "version": meta.cluster.get_version(),
            }
        },
        "spec": {
            "replicas": 1,
            "selector": {
                "matchLabels": {
                    "app": "scaler",
                }
            },
            "template": {
                "metadata": {
                    "labels": {
                        "app": "scaler",
                        "version": meta.cluster.get_version(),
                    }
                },
                "spec": {
                    "serviceAccountName": if scaler_spec.service_account {
                        Some("thorium")
                    } else {
                        None
                    },
                    "automountServiceAccountToken": scaler_spec.service_account,
                    "containers": [
                        {
                            "name": "scaler",
                            "image": meta.cluster.get_image(),
                            "imagePullPolicy": meta.cluster.spec.image_pull_policy.clone(),
                            "command": scaler_spec.cmd.clone(),
                            "args": scaler_spec.args.clone(),
                            "resources": {
                                "limits": crds::Resources::request_conv(&scaler_spec.resources),
                                "requests": crds::Resources::request_conv(&scaler_spec.resources),
                            },
                            "env": env,
                            "volumeMounts": volume_mounts
                        }
                    ],
                    "hostAliases": host_aliases,
                    "volumes": volumes,
                    "imagePullSecrets": image_pull_secrets
                }
            }
        }
    }))
}

/// Build JSON template for baremetal-scaler deployment
///
/// Returns None when the spec has no baremetal scaler.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `host_aliases` - The host aliases to add to each pod
fn baremetal_scaler_template(
    meta: &ClusterMeta,
    host_aliases: &Vec<K8sHostAliases>,
) -> Option<Value> {
    // build nothing for a component that isn't in the spec
    let scaler_spec = meta.cluster.get_baremetal_scaler_spec()?;
    // reference the chart's pull secret and the one rendered from registry_auth
    let image_pull_secrets = meta.cluster.image_pull_secrets();
    // build this component's deployment
    Some(serde_json::json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": {
            "namespace": meta.cluster.metadata.namespace.clone(),
            "name": "baremetal-scaler",
            "labels": {
                "app": "baremetal-scaler",
                "version": meta.cluster.get_version(),
            }
        },
        "spec": {
            "replicas": 1,
            "selector": {
                "matchLabels": {
                    "app": "baremetal-scaler",
                }
            },
            "template": {
                "metadata": {
                    "labels": {
                        "app": "baremetal-scaler",
                        "version": meta.cluster.get_version(),
                    }
                },
                "spec": {
                    // the bare metal scaler never calls the k8s API, so it doesn't get a service account token
                    "automountServiceAccountToken": false,
                    "containers": [
                        {
                            "name": "baremetal-scaler",
                            "image": meta.cluster.get_image(),
                            "imagePullPolicy": meta.cluster.spec.image_pull_policy.clone(),
                            "command": scaler_spec.cmd.clone(),
                            "args": scaler_spec.args.clone(),
                            "resources": {
                                "limits": crds::Resources::request_conv(&scaler_spec.resources),
                                "requests": crds::Resources::request_conv(&scaler_spec.resources),
                            },
                            "env": scaler_spec.env.clone(),
                            "volumeMounts": [
                                {
                                    "name": "config",
                                    "mountPath": "/conf/thorium.yml",
                                    "subPath": "thorium.yml"
                                },
                                {
                                    "name": "keys",
                                    "mountPath": "/keys/keys.yml",
                                    "subPath": "keys.yml"
                                }
                            ]
                        }
                    ],
                    "hostAliases": host_aliases,
                    "volumes": [
                        {
                            "name": "config",
                            "secret": {
                                "secretName": "thorium"
                            }
                        },
                        {
                            "name": "keys",
                            "secret": {
                                "secretName": "keys"
                            }
                        }
                    ],
                    "imagePullSecrets": image_pull_secrets
                }
            }
        }
    }))
}

/// Build JSON template for event-handler deployment
///
/// Returns None when the spec has no event handler.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `host_aliases` - The host aliases to add to each pod
fn event_handler_template(meta: &ClusterMeta, host_aliases: &Vec<K8sHostAliases>) -> Option<Value> {
    // build nothing for a component that isn't in the spec
    let handler_spec = meta.cluster.get_event_handler_spec()?;
    // reference the chart's pull secret and the one rendered from registry_auth
    let image_pull_secrets = meta.cluster.image_pull_secrets();
    // build this component's deployment
    Some(serde_json::json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": {
            "namespace": meta.cluster.metadata.namespace.clone(),
            "name": "event-handler",
            "labels": {
                "app": "event-handler",
                "version": meta.cluster.get_version(),
            }
        },
        "spec": {
            "replicas": 1,
            "selector": {
                "matchLabels": {
                    "app": "event-handler",
                }
            },
            "template": {
                "metadata": {
                    "labels": {
                        "app": "event-handler",
                        "version": meta.cluster.get_version(),
                    }
                },
                "spec": {
                    // the event handler never calls the k8s API, so it doesn't get a service account token
                    "automountServiceAccountToken": false,
                    "containers": [
                        {
                            "name": "event-handler",
                            "image": meta.cluster.get_image(),
                            "imagePullPolicy": meta.cluster.spec.image_pull_policy.clone(),
                            "command": handler_spec.cmd.clone(),
                            "args": handler_spec.args.clone(),
                            "resources": {
                                "limits": crds::Resources::request_conv(&handler_spec.resources),
                                "requests": crds::Resources::request_conv(&handler_spec.resources),
                            },
                            "env": handler_spec.env.clone(),
                            "volumeMounts": [
                                {
                                    "name": "config",
                                    "mountPath": "/conf/thorium.yml",
                                    "subPath": "thorium.yml",
                                },
                                {
                                    "name": "keys",
                                    "mountPath": "/keys/keys.yml",
                                    "subPath": "keys.yml"
                                }
                            ]
                        }
                    ],
                    "hostAliases": host_aliases,
                    "volumes": [
                        {
                            "name": "config",
                            "secret": {
                                "secretName": "thorium"
                            },
                        },
                        {
                            "name": "keys",
                            "secret": {
                                "secretName": "keys"
                            }
                        }
                    ],
                    "imagePullSecrets": image_pull_secrets
                }
            }
        }
    }))
}

/// Build JSON template for search-streamer deployment
///
/// Returns None when the spec has no search streamer.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `host_aliases` - The host aliases to add to each pod
fn search_streamer_template(
    meta: &ClusterMeta,
    host_aliases: &Vec<K8sHostAliases>,
) -> Option<Value> {
    // build nothing for a component that isn't in the spec
    let streamer_spec = meta.cluster.get_search_streamer_spec()?;
    // reference the chart's pull secret and the one rendered from registry_auth
    let image_pull_secrets = meta.cluster.image_pull_secrets();
    // build this component's deployment
    Some(serde_json::json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": {
            "namespace": meta.cluster.metadata.namespace.clone(),
            "name": "search-streamer",
            "labels": {
                "app": "search-streamer",
                "version": meta.cluster.get_version(),
            }
        },
        "spec": {
            "replicas": 1,
            "selector": {
                "matchLabels": {
                    "app": "search-streamer",
                }
            },
            "template": {
                "metadata": {
                    "labels": {
                        "app": "search-streamer",
                        "version": meta.cluster.get_version(),
                    }
                },
                "spec": {
                    // the search streamer never calls the k8s API, so it doesn't get a service account token
                    "automountServiceAccountToken": false,
                    "containers": [
                        {
                            "name": "search-streamer",
                            "image": meta.cluster.get_image(),
                            "imagePullPolicy": meta.cluster.spec.image_pull_policy.clone(),
                            "command": streamer_spec.cmd.clone(),
                            "args": streamer_spec.args.clone(),
                            "resources": {
                                "limits": crds::Resources::request_conv(&streamer_spec.resources),
                                "requests": crds::Resources::request_conv(&streamer_spec.resources),
                            },
                            "env": streamer_spec.env.clone(),
                            "volumeMounts": [
                                {
                                    "name": "config",
                                    "mountPath": "/conf/thorium.yml",
                                    "subPath": "thorium.yml"
                                },
                                {
                                    "name": "keys",
                                    "mountPath": "/keys/keys.yml",
                                    "subPath": "keys.yml"
                                }
                            ]
                        }
                    ],
                    "hostAliases": host_aliases,
                    "volumes": [
                        {
                            "name": "config",
                            "secret": {
                                "secretName": "thorium"
                            }
                        },
                        {
                            "name": "keys",
                            "secret": {
                                "secretName": "keys"
                            }
                        }
                    ],
                    "imagePullSecrets": image_pull_secrets
                }
            }
        }
    }))
}

/// The hashes of the inputs a component's pods are built from
pub struct RolloutHashes {
    /// The sha256 of the rendered thorium.yml
    pub config: String,
    /// The sha256 of everything else the component mounts and only reads at startup
    pub mounts: String,
}

/// Annotate a deployment's pod template with the inputs its pods were built from
///
/// Changing any annotation changes the pod template, so k8s rolls out new pods whenever
/// the rendered config or anything else the component mounts changes. Every other spec
/// field a component depends on is part of its pod template already.
///
/// # Arguments
///
/// * `deployment` - The deployment to annotate
/// * `hashes` - The hashes of the inputs these pods are built from
fn annotate_rollout(deployment: &mut Deployment, hashes: &RolloutHashes) {
    // get the annotations on this deployment's pod template
    let annotations = deployment
        .spec
        .get_or_insert_with(Default::default)
        .template
        .metadata
        .get_or_insert_with(Default::default)
        .annotations
        .get_or_insert_with(Default::default);
    // record the config these pods were built from
    annotations.insert(CONFIG_HASH_ANNOTATION.to_owned(), hashes.config.clone());
    // record everything else these pods mount
    annotations.insert(MOUNTS_HASH_ANNOTATION.to_owned(), hashes.mounts.clone());
}

/// Build the merge patch that updates an existing deployment to match a template
///
/// A merge patch only changes the keys it names, so a field the template omits would keep its
/// live value. `serviceAccountName` and the deprecated `serviceAccount` are therefore set to null
/// when the template sets neither, so a scaler switched off its service account really stops
/// running as `thorium`.
///
/// # Arguments
///
/// * `deployment` - The deployment template to patch towards
fn update_patch(deployment: &Deployment) -> Result<Value, serde_json::Error> {
    // patch the whole spec of this deployment
    let mut patch = serde_json::json!({ "spec": serde_json::to_value(&deployment.spec)? });
    // get the pod template in our patch if it has one
    let Some(template) = patch
        .pointer_mut("/spec/template")
        .and_then(Value::as_object_mut)
    else {
        return Ok(patch);
    };
    // get the pod spec in our pod template
    let Some(pod) = template.get_mut("spec").and_then(Value::as_object_mut) else {
        return Ok(patch);
    };
    // a pod that names no service account must drop any it ran as before, which a merge patch
    // keeps unless both the current and deprecated fields are null
    if !pod.contains_key("serviceAccountName") {
        pod.insert("serviceAccountName".to_owned(), Value::Null);
        pod.insert("serviceAccount".to_owned(), Value::Null);
    }
    Ok(patch)
}

/// Mount the Elastic CA Secret read-only into every container of a deployment
///
/// This does nothing unless the `ThoriumCluster` sets `elastic_ca_secret`.
///
/// # Arguments
///
/// * `deployment` - The deployment to add the CA to
/// * `cluster` - The `ThoriumCluster` this deployment belongs to
fn mount_elastic_ca(
    deployment: &mut Deployment,
    cluster: &ThoriumCluster,
) -> Result<(), serde_json::Error> {
    // skip deployments of clusters without an Elastic CA
    let Some(secret_ref) = &cluster.spec.elastic_ca_secret else {
        return Ok(());
    };
    // get this deployment's pod spec
    let Some(pod) = deployment
        .spec
        .as_mut()
        .and_then(|spec| spec.template.spec.as_mut())
    else {
        return Ok(());
    };
    // project just the CA key of the Secret to a fixed file name
    let volume = serde_json::from_value(serde_json::json!({
        "name": "elastic-ca",
        "secret": {
            "secretName": secret_ref.name,
            "items": [{"key": secret_ref.key, "path": crds::ELASTIC_CA_FILE}],
        }
    }))?;
    pod.volumes.get_or_insert_with(Vec::new).push(volume);
    // mount the CA directory read-only in every container
    for container in &mut pod.containers {
        let mount = serde_json::from_value(serde_json::json!({
            "name": "elastic-ca",
            "mountPath": crds::ELASTIC_CA_DIR,
            "readOnly": true,
        }))?;
        container
            .volume_mounts
            .get_or_insert_with(Vec::new)
            .push(mount);
    }
    Ok(())
}

/// Run every pod of a deployment as the image's unprivileged `thorium` user
///
/// The pod runs as `THORIUM_UID` under the runtime's default seccomp profile, and each
/// container drops every Linux capability and can't gain privileges, which satisfies
/// restricted Pod Security. Settings a template already put in the pod's security context,
/// such as the API's sysctls, are kept.
///
/// # Arguments
///
/// * `deployment` - The deployment to restrict
fn run_as_thorium(deployment: &mut Deployment) -> Result<(), serde_json::Error> {
    // get this deployment's pod spec
    let Some(pod) = deployment
        .spec
        .as_mut()
        .and_then(|spec| spec.template.spec.as_mut())
    else {
        return Ok(());
    };
    // run the whole pod as the thorium user under the default seccomp profile
    let context = pod.security_context.get_or_insert_with(Default::default);
    context.run_as_non_root = Some(true);
    context.run_as_user = Some(THORIUM_UID);
    context.run_as_group = Some(THORIUM_UID);
    context.seccomp_profile = Some(serde_json::from_value(
        serde_json::json!({"type": "RuntimeDefault"}),
    )?);
    // no component needs a Linux capability or to gain privileges
    for container in &mut pod.containers {
        container.security_context = Some(serde_json::from_value(serde_json::json!({
            "allowPrivilegeEscalation": false,
            "capabilities": {"drop": ["ALL"]},
        }))?);
    }
    Ok(())
}

/// Create or update a k8s deployment within a given namespace
///
/// # Arguments
///
/// * `deployment` - Deployment spec to create or patch if already exists
/// * `meta` - Thorium cluster client and metadata
/// * `hashes` - The hashes of the inputs these pods are built from
pub async fn create_or_update(
    mut deployment: Deployment,
    meta: &ClusterMeta,
    hashes: &RolloutHashes,
) -> Result<(), Error> {
    // roll out new pods whenever our config or anything else they mount changes
    annotate_rollout(&mut deployment, hashes);
    // give every component the CA to validate an external Elastic with
    mount_elastic_ca(&mut deployment, &meta.cluster)?;
    // run every component unprivileged
    run_as_thorium(&mut deployment)?;
    // get the name of this deployment to create or patch
    let params = PostParams::default();
    let Some(name) = deployment.metadata.name.clone() else {
        return Err(Error::new("Cannot create a Deployment without a name"));
    };
    // create the deployment, or patch it towards our template if it already exists
    match meta.deploy_api.create(&params, &deployment).await {
        Ok(_) => {
            println!("Deployment created {name} in namespace {}", meta.namespace);
            Ok(())
        }
        Err(kube::Error::Api(error)) => {
            // do not panic if deployment exists
            if error.reason == "AlreadyExists" {
                // build a patch that also deletes live fields the template omits
                let patch = update_patch(&deployment)?;
                let patch = Patch::Merge(&patch);
                let params: PatchParams = PatchParams::default();
                match meta.deploy_api.patch(&name, &params, &patch).await {
                    Ok(_) => {
                        println!("Patched {name} deployment in namespace {}", meta.namespace);
                        Ok(())
                    }
                    Err(error) => Err(Error::new(format!(
                        "Failed to patch {name} deployment: {error}"
                    ))),
                }
            } else {
                Err(Error::new(format!(
                    "Failed to create {name} deployment: {error}"
                )))
            }
        }
        Err(error) => Err(Error::new(format!(
            "Failed to create {name} deployment: {error}"
        ))),
    }
}

/// Delete a k8s deployment within a given namespace
///
/// # Arguments
///
/// * `name` - Name of deployment to delete
/// * `meta` - Thorium cluster client and metadata
pub async fn delete_one(name: &str, meta: &ClusterMeta) -> Result<(), Error> {
    let params = DeleteParams::default();
    // delete the deployment by name
    match meta.deploy_api.delete(name, &params).await {
        Ok(_) => println!(
            "Deleted {name} deployment from namespace {}",
            meta.namespace
        ),
        // a missing deployment is already in the state we want
        Err(kube::Error::Api(error)) if error.code == 404 => {
            println!(
                "No {name} deployment in namespace {} to delete, skipping cleanup",
                meta.namespace
            );
        }
        Err(kube::Error::Api(error)) => {
            return Err(Error::new(format!(
                "Failed to delete {name} deployment in namespace {}: {}",
                meta.namespace, error.message
            )));
        }
        Err(error) => {
            return Err(Error::new(format!(
                "Failed to delete {name} deployment in namespace {}: {error}",
                meta.namespace
            )));
        }
    }
    Ok(())
}

/// Create or update the API deployment
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `host_aliases` - The host aliases to add to each pod
/// * `hashes` - The hashes of the inputs these pods are built from
pub async fn deploy_api(
    meta: &ClusterMeta,
    host_aliases: &Vec<K8sHostAliases>,
    hashes: &RolloutHashes,
) -> Result<(), Error> {
    // build the api deployment to deploy
    let deployment = api(meta, host_aliases)?;
    // create or update this deployment
    create_or_update(deployment, meta, hashes).await?;
    Ok(())
}

/// Create or update the scaler deployments, deleting any scaler the spec no longer has
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `host_aliases` - The host aliases to add to each pod
/// * `scaler_hashes` - The hashes of the inputs the k8s scaler's pods are built from
/// * `hashes` - The hashes of the inputs the baremetal scaler's pods are built from
pub async fn deploy_scalers(
    meta: &ClusterMeta,
    host_aliases: &Vec<K8sHostAliases>,
    scaler_hashes: &RolloutHashes,
    hashes: &RolloutHashes,
) -> Result<(), Error> {
    // deploy any scaler from template
    if let Some(deployment) = scaler_template(meta, host_aliases) {
        let deployment: Deployment = serde_json::from_value(deployment)?;
        create_or_update(deployment, meta, scaler_hashes).await?;
    // a component removed from the spec has its deployment deleted
    } else {
        delete_one("scaler", meta).await?;
    }
    // deploy any baremetal scaler from template
    if let Some(deployment) = baremetal_scaler_template(meta, host_aliases) {
        let deployment: Deployment = serde_json::from_value(deployment)?;
        create_or_update(deployment, meta, hashes).await?;
    // a component removed from the spec has its deployment deleted
    } else {
        delete_one("baremetal-scaler", meta).await?;
    }
    Ok(())
}

/// Create or update the event-handler deployment
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `host_aliases` - The host aliases to add to each pod
/// * `hashes` - The hashes of the inputs these pods are built from
pub async fn deploy_event_handler(
    meta: &ClusterMeta,
    host_aliases: &Vec<K8sHostAliases>,
    hashes: &RolloutHashes,
) -> Result<(), Error> {
    // deploy any event handler from template
    if let Some(deployment) = event_handler_template(meta, host_aliases) {
        let deployment: Deployment = serde_json::from_value(deployment)?;
        create_or_update(deployment, meta, hashes).await?;
    // a component removed from the spec has its deployment deleted
    } else {
        delete_one("event-handler", meta).await?;
    }
    Ok(())
}

/// Create or update the search-streamer deployment
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `host_aliases` - The host aliases to add to each pod
/// * `hashes` - The hashes of the inputs these pods are built from
pub async fn deploy_search_streamer(
    meta: &ClusterMeta,
    host_aliases: &Vec<K8sHostAliases>,
    hashes: &RolloutHashes,
) -> Result<(), Error> {
    // deploy any search streamer from template
    if let Some(deployment) = search_streamer_template(meta, host_aliases) {
        let deployment: Deployment = serde_json::from_value(deployment)?;
        create_or_update(deployment, meta, hashes).await?;
    // a component removed from the spec has its deployment deleted
    } else {
        delete_one("search-streamer", meta).await?;
    }
    Ok(())
}

/// Check whether a deployment has finished rolling out
///
/// A rollout is complete once the controller has seen the latest spec and every desired
/// replica is updated and available with no old replicas left. A rollout of the latest spec
/// that passed its progress deadline is returned as an error.
///
/// # Arguments
///
/// * `deployment` - The deployment to check
fn rollout_complete(deployment: &Deployment) -> Result<bool, String> {
    // get the generation of the spec and the one the controller last saw
    let generation = deployment.metadata.generation.unwrap_or_default();
    let Some(status) = &deployment.status else {
        return Ok(false);
    };
    // the controller must have seen our latest spec, since its conditions may still describe
    // an older spec that stalled
    if status.observed_generation.unwrap_or_default() < generation {
        return Ok(false);
    }
    // a deployment past its progress deadline won't finish on its own
    let stalled = status.conditions.iter().flatten().find(|condition| {
        condition.type_ == "Progressing"
            && condition.reason.as_deref() == Some("ProgressDeadlineExceeded")
    });
    if let Some(condition) = stalled {
        return Err(format!(
            "rollout exceeded its progress deadline: {}",
            condition.message.as_deref().unwrap_or("no details")
        ));
    }
    // every desired replica must be updated and available with no old replicas left
    let desired = deployment
        .spec
        .as_ref()
        .and_then(|spec| spec.replicas)
        .unwrap_or(1);
    let updated = status.updated_replicas.unwrap_or_default();
    let available = status.available_replicas.unwrap_or_default();
    let current = status.replicas.unwrap_or_default();
    Ok(updated == desired && available == desired && current == desired)
}

/// Describe why a pod's containers aren't running
///
/// # Arguments
///
/// * `pod` - The pod to describe
fn pod_problems(pod: &Pod) -> Vec<String> {
    // get this pod's name
    let name = pod.metadata.name.as_deref().unwrap_or("unknown");
    // a pod without a status has nothing to describe yet
    let Some(status) = &pod.status else {
        return Vec::new();
    };
    // collect the problems with this pod
    let mut problems = Vec::new();
    // a pod that can't be scheduled has no container statuses to describe
    for condition in status.conditions.iter().flatten() {
        if condition.type_ == "PodScheduled" && condition.status == "False" {
            problems.push(format!(
                "pod {name} is not scheduled: {}",
                condition.message.as_deref().unwrap_or("no details")
            ));
        }
    }
    // a pod stranded on a down or unreachable node keeps its last container states
    if pod_stranded(status) {
        problems.push(format!(
            "pod {name} is not ready although its containers are, so its node may be down or \
             unreachable"
        ));
    }
    // describe each container that is waiting or last died
    let containers = status
        .init_container_statuses
        .iter()
        .flatten()
        .chain(status.container_statuses.iter().flatten());
    for container in containers {
        // skip containers that are ready
        if container.ready {
            continue;
        }
        // describe why this container is waiting, ignoring normal startup states
        let waiting = container
            .state
            .as_ref()
            .and_then(|state| state.waiting.as_ref())
            .filter(|waiting| {
                !matches!(
                    waiting.reason.as_deref(),
                    None | Some("ContainerCreating" | "PodInitializing")
                )
            })
            .map(|waiting| {
                let reason = waiting.reason.as_deref().unwrap_or_default();
                match &waiting.message {
                    Some(message) => format!("{reason}: {message}"),
                    None => reason.to_owned(),
                }
            });
        // describe how this container last died
        let terminated = container
            .last_state
            .as_ref()
            .and_then(|state| state.terminated.as_ref())
            .map(|terminated| {
                let reason = terminated.reason.as_deref().unwrap_or("Terminated");
                let code = terminated.exit_code;
                match &terminated.message {
                    Some(message) => format!("last exit {reason} ({code}): {}", message.trim()),
                    None => format!("last exit {reason} ({code})"),
                }
            });
        // combine what we know about this container
        let details = [waiting, terminated]
            .into_iter()
            .flatten()
            .collect::<Vec<String>>();
        if !details.is_empty() {
            problems.push(format!(
                "pod {name} container {}: {}",
                container.name,
                details.join("; ")
            ));
        }
    }
    problems
}

/// Check whether a pod was marked unready by the node lifecycle controller
///
/// A pod whose containers all report ready but that isn't ready itself was marked unready
/// because its node stopped reporting, so its node is likely down or unreachable.
///
/// # Arguments
///
/// * `status` - The status of the pod to check
fn pod_stranded(status: &PodStatus) -> bool {
    // every container must still report the ready state the kubelet last sent
    let containers_ready = status
        .container_statuses
        .as_ref()
        .is_some_and(|containers| {
            !containers.is_empty() && containers.iter().all(|container| container.ready)
        });
    // while the pod itself isn't ready
    let pod_unready = status
        .conditions
        .iter()
        .flatten()
        .any(|condition| condition.type_ == "Ready" && condition.status != "True");
    containers_ready && pod_unready
}

/// Check whether a pod is stuck rather than just starting: it can't be scheduled or is
/// stranded on a down or unreachable node
///
/// # Arguments
///
/// * `pod` - The pod to check
fn pod_stuck(pod: &Pod) -> bool {
    // a pod without a status hasn't been looked at by the scheduler yet
    let Some(status) = &pod.status else {
        return false;
    };
    // a pod the scheduler couldn't place
    let unscheduled = status
        .conditions
        .iter()
        .flatten()
        .any(|condition| condition.type_ == "PodScheduled" && condition.status == "False");
    unscheduled || pod_stranded(status)
}

/// Check whether any of a pod's containers is failing rather than just starting
///
/// A container is failing when it waits for one of the [`FAILING_REASONS`] or last
/// terminated with a non-zero exit code.
///
/// # Arguments
///
/// * `pod` - The pod to check
fn pod_failing(pod: &Pod) -> bool {
    // a pod without a status hasn't started any containers yet
    let Some(status) = &pod.status else {
        return false;
    };
    // check every init and regular container of this pod
    status
        .init_container_statuses
        .iter()
        .flatten()
        .chain(status.container_statuses.iter().flatten())
        .any(|container| {
            // a container waiting after a failure
            let waiting = container
                .state
                .as_ref()
                .and_then(|state| state.waiting.as_ref())
                .and_then(|waiting| waiting.reason.as_deref())
                .is_some_and(|reason| FAILING_REASONS.contains(&reason));
            // a container whose last run exited with an error
            let crashed = container
                .last_state
                .as_ref()
                .and_then(|state| state.terminated.as_ref())
                .is_some_and(|terminated| terminated.exit_code != 0);
            waiting || crashed
        })
}

/// Why a deployment's pods aren't ready
struct RolloutProblems {
    /// A description of the first few pod problems
    details: String,
    /// Whether any pod is failing rather than just starting
    failing: bool,
    /// Whether any pod can't be scheduled or is stranded on a down or unreachable node
    stuck: bool,
}

/// Describe why a deployment's pods aren't ready
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `name` - The name of the deployment (and its `app` label)
async fn rollout_problems(meta: &ClusterMeta, name: &str) -> RolloutProblems {
    // list this deployment's pods
    let params = ListParams::default().labels(&format!("app={name}"));
    let pods = match meta.pod_api.list(&params).await {
        Ok(pods) => pods,
        Err(error) => {
            return RolloutProblems {
                details: format!("could not list {name} pods: {error}"),
                failing: false,
                stuck: false,
            };
        }
    };
    // summarize the problems of this deployment's pods
    summarize_pods(&pods.items)
}

/// Summarize why a deployment's pods aren't ready
///
/// # Arguments
///
/// * `pods` - The deployment's pods
fn summarize_pods(pods: &[Pod]) -> RolloutProblems {
    // a terminating pod is already being replaced and one on a down node keeps the stale
    // status it last reported, so only pods that aren't being deleted are described
    let live = pods
        .iter()
        .filter(|pod| pod.metadata.deletion_timestamp.is_none())
        .collect::<Vec<&Pod>>();
    // check whether any pod is really failing
    let failing = live.iter().any(|pod| pod_failing(pod));
    // check whether any pod is stuck unscheduled or on a down node
    let stuck = live.iter().any(|pod| pod_stuck(pod));
    // describe the first few problems we find
    let problems = live
        .iter()
        .flat_map(|pod| pod_problems(pod))
        .take(MAX_POD_PROBLEMS)
        .collect::<Vec<String>>();
    let details = if problems.is_empty() {
        "no pod reported an error".to_owned()
    } else {
        problems.join("; ")
    };
    RolloutProblems {
        details,
        failing,
        stuck,
    }
}

/// Describe the pending deployments whose pods are failing or stuck, if any are
///
/// A stuck pod is one that can't be scheduled or is stranded on a down or unreachable node,
/// which explains why an already rolled out component became unavailable.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `pending` - The deployments that haven't finished rolling out
async fn failing_rollouts(meta: &ClusterMeta, pending: &[String]) -> Option<String> {
    // describe each pending deployment that has a failing or stuck pod
    let mut details = Vec::new();
    for name in pending {
        let problems = rollout_problems(meta, name).await;
        if problems.failing || problems.stuck {
            details.push(format!("{name}: {}", problems.details));
        }
    }
    // only report when something is really failing or stuck
    (!details.is_empty()).then(|| details.join(" | "))
}

/// Find the deployments that are still rolling out from their checked rollout states
///
/// Returns the first stalled deployment and why it stalled as an error.
///
/// # Arguments
///
/// * `checked` - Each deployment's name and its result from [`rollout_complete`]
fn pending_rollouts(
    checked: Vec<(String, Result<bool, String>)>,
) -> Result<Vec<String>, (String, String)> {
    // keep only the deployments that haven't finished, stopping at a stalled one
    let mut pending = Vec::with_capacity(checked.len());
    for (name, state) in checked {
        match state {
            Ok(true) => println!("Deployment {name} has rolled out"),
            Ok(false) => pending.push(name),
            Err(stalled) => return Err((name, stalled)),
        }
    }
    Ok(pending)
}

/// Name the deployments that are still rolling out
///
/// # Arguments
///
/// * `pending` - The deployments that haven't finished rolling out
fn waiting_message(pending: &[String]) -> String {
    // list every pending deployment in one message
    format!("Waiting for {} to roll out", pending.join(", "))
}

/// Build the status message for deployments that are still rolling out
///
/// # Arguments
///
/// * `waiting` - The message naming the pending deployments
/// * `failures` - A description of the pending deployments' failing pods, if any are failing
fn pending_message(waiting: String, failures: Option<String>) -> String {
    // add the pod failures after the pending deployments when there are any
    match failures {
        Some(failures) => format!("{waiting}: {failures}"),
        None => waiting,
    }
}

/// Check whether a cluster's status already reports a wait, so writing the bare wait message
/// would only hide the pod failures it lists
///
/// Only a status describing the cluster's current spec generation counts, since the status of
/// an older generation has already been reset.
///
/// # Arguments
///
/// * `status` - The status last written for the `ThoriumCluster`
/// * `generation` - The `metadata.generation` of the cluster's current spec
/// * `waiting` - The message naming the pending deployments
fn status_reports_wait(
    status: &crds::ThoriumClusterStatus,
    generation: Option<i64>,
    waiting: &str,
) -> bool {
    // only a status describing our current spec counts
    if status.observed_generation != generation {
        return false;
    }
    // the message must be this wait, either bare or followed by its pod failures
    status.message.as_deref().is_some_and(|message| {
        message == waiting
            || message
                .strip_prefix(waiting)
                .is_some_and(|rest| rest.starts_with(": "))
    })
}

/// Wait a short, bounded time for deployments to finish rolling out
///
/// Returns None once every deployment has rolled out, or a status message naming the
/// deployments still pending (with the details of any failing pods) once `timeout` passes so
/// the caller can requeue instead of blocking the reconcile. A deployment past its progress
/// deadline is returned as an error. While waiting, the `ThoriumCluster` is set to
/// `Provisioning` naming the pending deployments, unless its status already reports this wait
/// along with pod failures from an earlier reconcile (status updates don't retrigger a
/// reconcile).
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `names` - The names of the deployments to wait on
/// * `timeout` - How long to wait for every deployment before handing back
pub async fn wait_for_rollouts(
    meta: &ClusterMeta,
    names: &[String],
    timeout: std::time::Duration,
) -> Result<Option<String>, Error> {
    // stop waiting at our deadline
    let deadline = tokio::time::Instant::now() + timeout;
    // track the deployments that haven't finished
    let mut pending = names.to_vec();
    // track the status message we last set
    let mut reported: Option<String> = None;
    loop {
        // check the rollout of each deployment that hasn't finished yet
        let mut checked = Vec::with_capacity(pending.len());
        for name in pending {
            // get this deployment's current state
            let deployment = meta.deploy_api.get(&name).await?;
            checked.push((name, rollout_complete(&deployment)));
        }
        // keep waiting on only the deployments that haven't finished
        pending = match pending_rollouts(checked) {
            Ok(pending) => pending,
            // a stalled rollout won't finish so report why its pods are failing
            Err((name, stalled)) => {
                let problems = rollout_problems(meta, &name).await;
                return Err(Error::new(format!(
                    "Deployment {name} {stalled}; {}",
                    problems.details
                )));
            }
        };
        // every deployment has rolled out
        if pending.is_empty() {
            return Ok(None);
        }
        // name the pending deployments
        let waiting = waiting_message(&pending);
        // hand back the wait and any pod failures once we run out of time
        if tokio::time::Instant::now() >= deadline {
            let failures = failing_rollouts(meta, &pending).await;
            return Ok(Some(pending_message(waiting, failures)));
        }
        // name the pending deployments in the status unless it already reports this wait
        if reported.as_ref() != Some(&waiting)
            && !status_reports_wait(
                &meta.status.current(),
                meta.cluster.metadata.generation,
                &waiting,
            )
        {
            crds::set_status(
                &meta.client,
                &meta.status,
                crds::ClusterPhase::Provisioning,
                Some(waiting.clone()),
            )
            .await;
            reported = Some(waiting);
        }
        // check again shortly
        tokio::time::sleep(ROLLOUT_POLL).await;
    }
}

/// Wait for the Thorium API to answer health checks
///
/// Returns a description of the last failure if the API didn't answer within the timeout.
///
/// # Arguments
///
/// * `host` - The url the operator reaches the API at
/// * `timeout` - How long to wait for the API to answer
pub async fn wait_for_health(
    host: &str,
    timeout: std::time::Duration,
) -> Result<Option<String>, Error> {
    // build a client with bounded request timeouts
    let client = crate::app::helpers::reqwest_client()?;
    let basic = Basic::new(host, &client);
    // stop waiting at our deadline
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        // ping the API and remember why it failed
        let failure = match basic.health().await {
            Ok(true) => return Ok(None),
            Ok(false) => "it reported it is unhealthy".to_owned(),
            Err(error) => error.to_string(),
        };
        // give up once we run out of time
        if tokio::time::Instant::now() >= deadline {
            return Ok(Some(format!(
                "The API rolled out but did not answer health checks at {host} within {}s: {failure}",
                timeout.as_secs()
            )));
        }
        // check again shortly
        tokio::time::sleep(ROLLOUT_POLL).await;
    }
}

/// Delete thorium component deployments
///
/// # Arguments
///
/// * `deploy_api` - The Deployment API for the `ThoriumCluster`'s namespace
/// * `cluster` - The `ThoriumCluster` whose component deployments to delete
pub async fn delete(deploy_api: &Api<Deployment>, cluster: &ThoriumCluster) -> Result<(), Error> {
    println!("Cleaning up deployments using CRD");
    // delete each deployment from the namespace
    let params = DeleteParams::default();
    for deployment in cluster.list_component_names() {
        match deploy_api.delete(&deployment, &params).await {
            Ok(_) => println!("Deleted {deployment} deployment"),
            // a missing deployment is already in the state we want
            Err(kube::Error::Api(error)) if error.code == 404 => {
                println!("No {deployment} deployment to delete, skipping cleanup");
            }
            Err(kube::Error::Api(error)) => {
                return Err(Error::new(format!(
                    "Failed to delete {deployment} deployment: {}",
                    error.message
                )));
            }
            Err(error) => {
                return Err(Error::new(format!(
                    "Failed to delete {deployment} deployment: {error}"
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::k8s::clusters::tests::{FakeKube, full_spec, meta_for, namespaced_cluster};
    use serde::Deserialize;
    use thorium::models::upgrades::UpgradeComponent;

    /// Build a deployment from JSON
    ///
    /// # Arguments
    ///
    /// * `raw` - The deployment as JSON
    fn deployment(raw: &serde_json::Value) -> Deployment {
        Deployment::deserialize(raw).expect("deployment should deserialize")
    }

    /// Build a deployment with a generation, desired replicas, and status
    ///
    /// # Arguments
    ///
    /// * `generation` - The spec generation
    /// * `status` - The deployment status as JSON
    fn with_status(generation: i64, status: &serde_json::Value) -> Deployment {
        deployment(&serde_json::json!({
            "metadata": {"name": "api", "generation": generation},
            "spec": {"replicas": 2, "selector": {}, "template": {}},
            "status": status
        }))
    }

    /// The Elastic CA is mounted read-only in every container only when a CA Secret is set
    #[test]
    fn elastic_ca_mount() {
        // build a deployment with two containers and an existing volume
        let raw = serde_json::json!({
            "metadata": {"name": "api"},
            "spec": {"selector": {}, "template": {"spec": {
                "containers": [{"name": "a"}, {"name": "b", "volumeMounts": [{"name": "config", "mountPath": "/conf"}]}],
                "volumes": [{"name": "config", "secret": {"secretName": "thorium"}}]
            }}}
        });
        // a cluster without a CA secret leaves the deployment alone
        let spec = |extra: serde_json::Value| {
            let mut spec = serde_json::json!({
                "components": {"api": {}},
                "registry": "registry/thorium",
                "config": {}
            });
            json_patch::merge(&mut spec, &extra);
            crate::k8s::clusters::tests::cluster_from_spec(spec)
        };
        let mut plain = deployment(&raw);
        mount_elastic_ca(&mut plain, &spec(serde_json::json!({}))).expect("mount");
        assert_eq!(plain, deployment(&raw));
        // a CA secret adds one volume holding just the CA key
        let cluster =
            spec(serde_json::json!({"elastic_ca_secret": {"name": "elastic-ca", "key": "tls.ca"}}));
        let mut mounted = deployment(&raw);
        mount_elastic_ca(&mut mounted, &cluster).expect("mount");
        let value = serde_json::to_value(&mounted).expect("serialize");
        let pod = &value["spec"]["template"]["spec"];
        assert_eq!(
            pod["volumes"][1],
            serde_json::json!({"name": "elastic-ca", "secret": {"secretName": "elastic-ca", "items": [{"key": "tls.ca", "path": "ca.crt"}]}})
        );
        // every container mounts the CA directory read-only
        let mount = serde_json::json!({"name": "elastic-ca", "mountPath": "/etc/thorium/elastic-ca", "readOnly": true});
        assert_eq!(
            pod["containers"][0]["volumeMounts"],
            serde_json::json!([mount])
        );
        assert_eq!(pod["containers"][1]["volumeMounts"][1], mount);
    }

    /// The rollout annotations record only the config and mounts hashes
    #[test]
    fn rollout_annotations() {
        // annotate a deployment
        let mut api = deployment(&serde_json::json!({"metadata": {"name": "api"}}));
        let hashes = RolloutHashes {
            config: "config".to_owned(),
            mounts: "mounts".to_owned(),
        };
        annotate_rollout(&mut api, &hashes);
        // only the config and mounts hashes are recorded
        let annotations = api
            .spec
            .and_then(|spec| spec.template.metadata)
            .and_then(|meta| meta.annotations)
            .expect("annotations should be set");
        assert_eq!(annotations[CONFIG_HASH_ANNOTATION], "config");
        assert_eq!(annotations[MOUNTS_HASH_ANNOTATION], "mounts");
        assert_eq!(annotations.len(), 2);
    }

    /// The update patch keeps the rollout annotations and deletes an omitted service account
    #[test]
    fn update_patch_nulls() {
        // build a component template without a service account
        let mut plain = deployment(&serde_json::json!({
            "metadata": {"name": "event-handler"},
            "spec": {"selector": {}, "template": {"spec": {"containers": [{"name": "a"}]}}}
        }));
        annotate_rollout(
            &mut plain,
            &RolloutHashes {
                config: "config".to_owned(),
                mounts: "mounts".to_owned(),
            },
        );
        let patch = update_patch(&plain).expect("patch");
        let template = &patch["spec"]["template"];
        // the rollout annotations are kept as they are
        let annotations = &template["metadata"]["annotations"];
        assert_eq!(
            annotations,
            &serde_json::json!({CONFIG_HASH_ANNOTATION: "config", MOUNTS_HASH_ANNOTATION: "mounts"})
        );
        // both service account fields are deleted
        let pod = template["spec"].as_object().expect("pod spec");
        assert_eq!(pod.get("serviceAccountName"), Some(&Value::Null));
        assert_eq!(pod.get("serviceAccount"), Some(&Value::Null));
        // a template naming a service account keeps it
        let scaler = deployment(&serde_json::json!({
            "metadata": {"name": "scaler"},
            "spec": {"selector": {}, "template": {"spec": {"serviceAccountName": "thorium", "containers": []}}}
        }));
        let patch = update_patch(&scaler).expect("patch");
        let pod = &patch["spec"]["template"]["spec"];
        assert_eq!(pod["serviceAccountName"], "thorium");
        assert!(pod.get("serviceAccount").is_none());
    }

    /// A rollout is complete only when every replica is updated and available
    #[test]
    fn rollout_completion() {
        // a deployment the controller hasn't seen yet isn't done
        let unseen = with_status(
            2,
            &serde_json::json!({"observedGeneration": 1, "replicas": 2, "updatedReplicas": 2, "availableReplicas": 2}),
        );
        assert_eq!(rollout_complete(&unseen), Ok(false));
        // a deployment still replacing pods isn't done
        let rolling = with_status(
            2,
            &serde_json::json!({"observedGeneration": 2, "replicas": 3, "updatedReplicas": 1, "availableReplicas": 2}),
        );
        assert_eq!(rollout_complete(&rolling), Ok(false));
        // old pods that are still terminating keep it from being done
        let terminating = with_status(
            2,
            &serde_json::json!({"observedGeneration": 2, "replicas": 3, "updatedReplicas": 2, "availableReplicas": 2}),
        );
        assert_eq!(rollout_complete(&terminating), Ok(false));
        // updated pods that aren't available yet keep it from being done
        let unavailable = with_status(
            2,
            &serde_json::json!({"observedGeneration": 2, "replicas": 2, "updatedReplicas": 2, "availableReplicas": 1}),
        );
        assert_eq!(rollout_complete(&unavailable), Ok(false));
        // every replica updated and available is done
        let done = with_status(
            2,
            &serde_json::json!({"observedGeneration": 2, "replicas": 2, "updatedReplicas": 2, "availableReplicas": 2}),
        );
        assert_eq!(rollout_complete(&done), Ok(true));
        // a deployment without a status isn't done
        let new = deployment(&serde_json::json!({"metadata": {"generation": 1}}));
        assert_eq!(rollout_complete(&new), Ok(false));
    }

    /// A rollout past its progress deadline is an error
    #[test]
    fn rollout_deadline_exceeded() {
        // build a stalled deployment
        let stalled = with_status(
            2,
            &serde_json::json!({
                "observedGeneration": 2,
                "replicas": 2,
                "updatedReplicas": 1,
                "availableReplicas": 1,
                "conditions": [{
                    "type": "Progressing",
                    "status": "False",
                    "reason": "ProgressDeadlineExceeded",
                    "message": "ReplicaSet \"api-1\" has timed out progressing."
                }]
            }),
        );
        // the error carries the condition's message
        let error = rollout_complete(&stalled).expect_err("a stalled rollout is an error");
        assert!(error.contains("has timed out progressing"));
    }

    /// A stall condition left over from an older spec doesn't fail the rollout of a newer one
    #[test]
    fn rollout_stale_stall_pending() {
        // build a deployment whose new spec the controller hasn't seen since it stalled
        let stale = with_status(
            3,
            &serde_json::json!({
                "observedGeneration": 2,
                "replicas": 2,
                "updatedReplicas": 1,
                "availableReplicas": 1,
                "conditions": [{
                    "type": "Progressing",
                    "status": "False",
                    "reason": "ProgressDeadlineExceeded",
                    "message": "ReplicaSet \"api-1\" has timed out progressing."
                }]
            }),
        );
        // the rollout of the new spec is still pending rather than failed
        assert_eq!(rollout_complete(&stale), Ok(false));
    }

    /// Only crash loops, image and container creation errors, and non-zero exits are failures
    #[test]
    fn pod_failure_detection() {
        // build a pod from one container status
        let pod = |container: serde_json::Value| -> Pod {
            let mut status = serde_json::json!({
                "name": "api",
                "ready": false,
                "restartCount": 0,
                "image": "thorium",
                "imageID": ""
            });
            json_patch::merge(&mut status, &container);
            serde_json::from_value(serde_json::json!({
                "metadata": {"name": "api-1"},
                "status": {"containerStatuses": [status]}
            }))
            .expect("pod should deserialize")
        };
        // each failing waiting reason is a failure
        for reason in FAILING_REASONS {
            let failing = pod(serde_json::json!({"state": {"waiting": {"reason": reason}}}));
            assert!(pod_failing(&failing), "{reason} should be a failure");
        }
        // a container that is still starting is not
        let starting =
            pod(serde_json::json!({"state": {"waiting": {"reason": "ContainerCreating"}}}));
        assert!(!pod_failing(&starting));
        // a last exit with an error is a failure, a clean exit is not
        let crashed = pod(serde_json::json!({"lastState": {"terminated": {"exitCode": 1}}}));
        assert!(pod_failing(&crashed));
        let exited = pod(serde_json::json!({"lastState": {"terminated": {"exitCode": 0}}}));
        assert!(!pod_failing(&exited));
        // a pod without a status is not failing
        let new: Pod = serde_json::from_value(serde_json::json!({"metadata": {"name": "api-2"}}))
            .expect("pod should deserialize");
        assert!(!pod_failing(&new));
    }

    /// Pod problems describe waiting reasons and last terminations
    #[test]
    fn pod_problem_details() {
        // build a crash looping pod
        let pod: Pod = serde_json::from_value(serde_json::json!({
            "metadata": {"name": "api-1"},
            "status": {
                "containerStatuses": [{
                    "name": "api",
                    "ready": false,
                    "restartCount": 4,
                    "image": "thorium",
                    "imageID": "",
                    "state": {"waiting": {"reason": "CrashLoopBackOff", "message": "back-off 40s"}},
                    "lastState": {"terminated": {"exitCode": 101, "reason": "Error", "message": "Failed to connect to redis\n"}}
                }]
            }
        }))
        .expect("pod should deserialize");
        // both the waiting reason and the last exit are described
        let problems = pod_problems(&pod);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("pod api-1 container api"));
        assert!(problems[0].contains("CrashLoopBackOff: back-off 40s"));
        assert!(problems[0].contains("last exit Error (101): Failed to connect to redis"));
        // a container that is just starting has no problems
        let starting: Pod = serde_json::from_value(serde_json::json!({
            "metadata": {"name": "api-2"},
            "status": {
                "containerStatuses": [{
                    "name": "api",
                    "ready": false,
                    "restartCount": 0,
                    "image": "thorium",
                    "imageID": "",
                    "state": {"waiting": {"reason": "ContainerCreating"}}
                }]
            }
        }))
        .expect("pod should deserialize");
        assert_eq!(pod_problems(&starting), Vec::<String>::new());
        // an unschedulable pod is described by its condition
        let pending: Pod = serde_json::from_value(serde_json::json!({
            "metadata": {"name": "api-3"},
            "status": {"conditions": [{"type": "PodScheduled", "status": "False", "message": "0/1 nodes are available"}]}
        }))
        .expect("pod should deserialize");
        assert_eq!(
            pod_problems(&pending),
            vec!["pod api-3 is not scheduled: 0/1 nodes are available"]
        );
    }

    /// Every deployment rolling out leaves nothing pending, unfinished ones stay pending, and a
    /// stalled one is an error even when others are complete
    #[test]
    fn pending_rollout_decisions() {
        // build complete, pending, and stalled rollout states
        let done = with_status(
            2,
            &serde_json::json!({"observedGeneration": 2, "replicas": 2, "updatedReplicas": 2, "availableReplicas": 2}),
        );
        let rolling = with_status(
            2,
            &serde_json::json!({"observedGeneration": 2, "replicas": 3, "updatedReplicas": 1, "availableReplicas": 2}),
        );
        let stalled = with_status(
            2,
            &serde_json::json!({
                "observedGeneration": 2,
                "conditions": [{"type": "Progressing", "status": "False", "reason": "ProgressDeadlineExceeded"}]
            }),
        );
        // every deployment complete leaves nothing to wait on so the cluster can be ready
        let complete = vec![
            ("api".to_owned(), rollout_complete(&done)),
            ("search-streamer".to_owned(), rollout_complete(&done)),
        ];
        assert_eq!(pending_rollouts(complete), Ok(Vec::new()));
        // only the unfinished deployments stay pending
        let partial = vec![
            ("api".to_owned(), rollout_complete(&done)),
            ("event-handler".to_owned(), rollout_complete(&rolling)),
            ("search-streamer".to_owned(), rollout_complete(&rolling)),
        ];
        assert_eq!(
            pending_rollouts(partial),
            Ok(vec![
                "event-handler".to_owned(),
                "search-streamer".to_owned()
            ])
        );
        // a stalled deployment is an error naming it
        let failed = vec![
            ("api".to_owned(), rollout_complete(&rolling)),
            ("event-handler".to_owned(), rollout_complete(&stalled)),
        ];
        let (name, stalled) = pending_rollouts(failed).expect_err("a stall is an error");
        assert_eq!(name, "event-handler");
        assert!(stalled.contains("progress deadline"));
    }

    /// A pending rollout names its deployments and only adds pod details once a pod is failing
    #[test]
    fn pending_rollout_messages() {
        // a pending rollout without failing pods just names the deployments
        let pending = vec!["event-handler".to_owned(), "search-streamer".to_owned()];
        let waiting = waiting_message(&pending);
        assert_eq!(
            waiting,
            "Waiting for event-handler, search-streamer to roll out"
        );
        assert_eq!(pending_message(waiting.clone(), None), waiting);
        // a crash looping pod is failing and its details follow the wait
        let crashing: Pod = serde_json::from_value(serde_json::json!({
            "metadata": {"name": "event-handler-1"},
            "status": {
                "containerStatuses": [{
                    "name": "event-handler",
                    "ready": false,
                    "restartCount": 3,
                    "image": "thorium",
                    "imageID": "",
                    "state": {"waiting": {"reason": "CrashLoopBackOff"}},
                    "lastState": {"terminated": {"exitCode": 1, "reason": "Error"}}
                }]
            }
        }))
        .expect("pod should deserialize");
        let problems = summarize_pods(&[crashing]);
        assert!(problems.failing);
        let message = pending_message(
            waiting.clone(),
            Some(format!("event-handler: {}", problems.details)),
        );
        assert_eq!(
            message,
            "Waiting for event-handler, search-streamer to roll out: event-handler: pod \
             event-handler-1 container event-handler: CrashLoopBackOff; last exit Error (1)"
        );
        // pods that are only starting aren't failing
        let starting: Pod = serde_json::from_value(serde_json::json!({
            "metadata": {"name": "search-streamer-1"},
            "status": {"phase": "Pending"}
        }))
        .expect("pod should deserialize");
        let problems = summarize_pods(&[starting]);
        assert!(!problems.failing);
        assert!(!problems.stuck);
        assert_eq!(problems.details, "no pod reported an error");
    }

    /// Pods on a down node are described as possibly unreachable rather than failing, and
    /// terminating pods stuck on a down node are ignored
    #[test]
    fn down_node_pods() {
        // a running pod the node lifecycle controller marked unready while its containers
        // still report the last state the kubelet sent
        let unreachable: Pod = serde_json::from_value(serde_json::json!({
            "metadata": {"name": "api-1"},
            "status": {
                "phase": "Running",
                "conditions": [{"type": "Ready", "status": "False"}],
                "containerStatuses": [{
                    "name": "api",
                    "ready": true,
                    "restartCount": 0,
                    "image": "thorium",
                    "imageID": "",
                    "state": {"running": {}}
                }]
            }
        }))
        .expect("pod should deserialize");
        let problems = summarize_pods(std::slice::from_ref(&unreachable));
        assert!(!problems.failing);
        assert!(problems.stuck);
        assert!(
            problems.details.contains("pod api-1 is not ready")
                && problems.details.contains("node may be down or unreachable"),
            "{}",
            problems.details
        );
        // a replacement that can't be scheduled because of the down node's taint is described
        // but isn't failing
        let pending: Pod = serde_json::from_value(serde_json::json!({
            "metadata": {"name": "api-2"},
            "status": {
                "phase": "Pending",
                "conditions": [{
                    "type": "PodScheduled",
                    "status": "False",
                    "message": "0/2 nodes are available: 1 node(s) had untolerated taint {node.kubernetes.io/unreachable: }"
                }]
            }
        }))
        .expect("pod should deserialize");
        let problems = summarize_pods(std::slice::from_ref(&pending));
        assert!(!problems.failing);
        assert!(problems.stuck);
        assert!(problems.details.contains("node.kubernetes.io/unreachable"));
        // an evicted pod stuck terminating on the down node with a stale crash isn't counted
        let mut stuck: Pod = serde_json::from_value(serde_json::json!({
            "metadata": {"name": "api-0"},
            "status": {
                "phase": "Running",
                "containerStatuses": [{
                    "name": "api",
                    "ready": false,
                    "restartCount": 1,
                    "image": "thorium",
                    "imageID": "",
                    "lastState": {"terminated": {"exitCode": 1, "reason": "Error"}}
                }]
            }
        }))
        .expect("pod should deserialize");
        assert!(summarize_pods(std::slice::from_ref(&stuck)).failing);
        stuck.metadata.deletion_timestamp = Some(
            k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(chrono::Utc::now()),
        );
        let problems = summarize_pods(&[stuck, pending]);
        assert!(!problems.failing);
        assert!(!problems.details.contains("api-0"), "{}", problems.details);
        // a healthy running pod has no problems
        let mut healthy = unreachable;
        healthy.status.as_mut().expect("status").conditions = Some(vec![
            serde_json::from_value(serde_json::json!({"type": "Ready", "status": "True"}))
                .expect("condition"),
        ]);
        assert_eq!(pod_problems(&healthy), Vec::<String>::new());
        assert!(!summarize_pods(&[healthy]).stuck);
    }

    /// A status already reporting the same wait for the current spec is kept so its pod
    /// details aren't replaced by the bare wait on every requeue
    #[test]
    fn status_wait_detection() {
        // build statuses at a generation reporting a wait with or without pod details
        let status = |message: &str, generation: i64| crds::ThoriumClusterStatus {
            phase: Some(crds::ClusterPhase::Provisioning),
            message: Some(message.to_owned()),
            observed_generation: Some(generation),
            ..Default::default()
        };
        let waiting = "Waiting for event-handler to roll out";
        let current = Some(2);
        let detailed = status(
            "Waiting for event-handler to roll out: event-handler: pod crashed",
            2,
        );
        assert!(status_reports_wait(&detailed, current, waiting));
        // the bare wait is reported too
        assert!(status_reports_wait(&status(waiting, 2), current, waiting));
        // a wait on other deployments isn't this wait
        let other = status("Waiting for event-handler, search-streamer to roll out", 2);
        assert!(!status_reports_wait(&other, current, waiting));
        // a status describing an older spec doesn't count
        assert!(!status_reports_wait(&status(waiting, 1), current, waiting));
        // an empty status doesn't report anything
        assert!(!status_reports_wait(
            &crds::ThoriumClusterStatus::default(),
            current,
            waiting
        ));
    }

    /// Build every component Deployment a cluster spec asks for
    ///
    /// # Arguments
    ///
    /// * `spec` - The cluster spec
    fn component_templates(spec: serde_json::Value) -> Vec<Deployment> {
        // build the templates without contacting the kube API
        let fake = FakeKube::default();
        let meta = meta_for(namespaced_cluster(spec), &fake.client());
        let aliases = Vec::new();
        let api = api(&meta, &aliases).expect("api deployment");
        let optional = [
            scaler_template(&meta, &aliases),
            baremetal_scaler_template(&meta, &aliases),
            event_handler_template(&meta, &aliases),
            search_streamer_template(&meta, &aliases),
        ];
        std::iter::once(api)
            .chain(optional.into_iter().flatten().map(|raw| deployment(&raw)))
            .collect()
    }

    /// Get the volume names of a Deployment's pods
    ///
    /// # Arguments
    ///
    /// * `deployment` - The Deployment to inspect
    fn volume_names(deployment: &Deployment) -> Vec<String> {
        deployment
            .spec
            .as_ref()
            .and_then(|spec| spec.template.spec.as_ref())
            .and_then(|pod| pod.volumes.as_ref())
            .map_or_else(Vec::new, |volumes| {
                volumes.iter().map(|volume| volume.name.clone()).collect()
            })
    }

    /// Every component Deployment keeps the name and `app` selector legacy clusters were
    /// deployed with, so converted clusters are patched in place
    #[tokio::test]
    async fn component_templates_keep_legacy_selectors() {
        // build every component of a chart cluster
        let deployments = component_templates(full_spec());
        let names = deployments
            .iter()
            .map(|deployment| deployment.metadata.name.clone().unwrap_or_default())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "api",
                "scaler",
                "baremetal-scaler",
                "event-handler",
                "search-streamer"
            ]
        );
        // the upgrade catalog quiesces components by these names
        for component in [
            UpgradeComponent::Api,
            UpgradeComponent::Scaler,
            UpgradeComponent::BaremetalScaler,
            UpgradeComponent::EventHandler,
            UpgradeComponent::SearchStreamer,
        ] {
            assert!(names.contains(&component.deployment_name().to_owned()));
        }
        for deployment in &deployments {
            // the selector is the immutable app label that matches the pod labels
            let name = deployment.metadata.name.clone().unwrap_or_default();
            let spec = deployment.spec.as_ref().expect("spec");
            let selector = spec.selector.match_labels.clone().unwrap_or_default();
            assert_eq!(
                selector,
                std::collections::BTreeMap::from([("app".to_owned(), name.clone())])
            );
            let labels = spec
                .template
                .metadata
                .as_ref()
                .and_then(|meta| meta.labels.clone())
                .unwrap_or_default();
            assert_eq!(labels["app"], name);
            assert_eq!(labels["version"], "1.8.1");
            // pods run the spec's image with the chart's pull secret and mount thorium.yml
            let pod = spec.template.spec.as_ref().expect("pod spec");
            assert_eq!(
                pod.containers[0].image.as_deref(),
                Some("registry/thorium:1.8.1"),
                "{name}"
            );
            let pull_secrets = serde_json::to_value(&pod.image_pull_secrets).expect("pull secrets");
            assert_eq!(
                pull_secrets,
                serde_json::json!([{"name": "thorium-image-pull"}]),
                "{name}"
            );
            assert!(
                volume_names(deployment).contains(&"config".to_owned()),
                "{name}"
            );
            // only a k8s scaler using its service account gets a token, since no other
            // component calls the k8s API
            if name != "scaler" {
                assert_eq!(pod.automount_service_account_token, Some(false), "{name}");
            }
        }
    }

    /// Components left out of the spec get no Deployment
    #[tokio::test]
    async fn optional_components_are_skipped() {
        // build a cluster with only the api
        let deployments = component_templates(serde_json::json!({
            "components": {"api": {}},
            "registry": "registry/thorium",
            "config": {}
        }));
        // only the api is built
        assert_eq!(deployments.len(), 1);
        assert_eq!(deployments[0].metadata.name.as_deref(), Some("api"));
    }

    /// A scaler without a service account mounts the kube config, and registry auth mounts
    /// the skopeo credentials
    #[tokio::test]
    async fn scaler_mounts_follow_spec() {
        // a scaler using the service account with no registry auth mounts neither
        let mut spec = full_spec();
        spec["components"]["scaler"] = serde_json::json!({"service_account": true});
        let scaler = &component_templates(spec.clone())[1];
        let volumes = volume_names(scaler);
        assert!(!volumes.contains(&"kube-config".to_owned()));
        assert!(!volumes.contains(&"docker-skopeo".to_owned()));
        let pod = scaler
            .spec
            .as_ref()
            .and_then(|spec| spec.template.spec.clone())
            .expect("pod");
        assert_eq!(pod.service_account_name.as_deref(), Some("thorium"));
        // a scaler without one and with registry auth mounts both
        spec["components"]["scaler"] = serde_json::json!({"service_account": false});
        spec["registry_auth"] = serde_json::json!({"registry": "token"});
        let scaler = &component_templates(spec)[1];
        let volumes = volume_names(scaler);
        assert!(volumes.contains(&"kube-config".to_owned()));
        assert!(volumes.contains(&"docker-skopeo".to_owned()));
        let pod = scaler
            .spec
            .as_ref()
            .and_then(|spec| spec.template.spec.clone())
            .expect("pod");
        assert_eq!(pod.service_account_name, None);
        assert_eq!(pod.automount_service_account_token, Some(false));
    }

    /// Every component runs as the image's thorium user with the restricted Pod Security
    /// settings, and the API keeps its sysctl for binding port 80
    #[tokio::test]
    async fn components_run_as_thorium() {
        for mut deployment in component_templates(full_spec()) {
            // restrict this component as create_or_update does
            run_as_thorium(&mut deployment).expect("restrict");
            let name = deployment.metadata.name.clone().unwrap_or_default();
            let pod = deployment
                .spec
                .and_then(|spec| spec.template.spec)
                .expect("pod");
            // the pod runs as the thorium user under the default seccomp profile
            let context = pod.security_context.expect("pod context");
            assert_eq!(context.run_as_non_root, Some(true), "{name}");
            assert_eq!(context.run_as_user, Some(THORIUM_UID), "{name}");
            assert_eq!(context.run_as_group, Some(THORIUM_UID), "{name}");
            assert_eq!(
                context.seccomp_profile.map(|profile| profile.type_),
                Some("RuntimeDefault".to_owned()),
                "{name}"
            );
            // only the API lowers the unprivileged port range
            let sysctls = context.sysctls.unwrap_or_default();
            if name == "api" {
                assert_eq!(sysctls.len(), 1);
                assert_eq!(sysctls[0].name, "net.ipv4.ip_unprivileged_port_start");
                assert_eq!(sysctls[0].value, "0");
            } else {
                assert!(sysctls.is_empty(), "{name}");
            }
            // every container drops its capabilities and can't gain privileges
            for container in pod.containers {
                let context = container.security_context.expect("container context");
                assert_eq!(context.allow_privilege_escalation, Some(false), "{name}");
                assert_eq!(
                    context.capabilities.and_then(|caps| caps.drop),
                    Some(vec!["ALL".to_owned()]),
                    "{name}"
                );
            }
        }
    }

    /// The scaler's HOME and KUBECONFIG point at its mounts whatever its spec's env says, so a
    /// stored spec still carrying KUBECONFIG=/root/.kube/config works as the thorium user
    #[tokio::test]
    async fn scaler_env_points_at_mounts() {
        // a scaler without a service account whose env names root's kube config
        let mut spec = full_spec();
        spec["components"]["scaler"] = serde_json::json!({
            "service_account": false,
            "env": [
                {"name": "https_proxy", "value": "http://proxy:3128"},
                {"name": "KUBECONFIG", "value": "/root/.kube/config"},
                {"name": "HOME", "value": "/root"}
            ]
        });
        let env = |deployment: &Deployment| {
            deployment
                .spec
                .as_ref()
                .and_then(|spec| spec.template.spec.as_ref())
                .and_then(|pod| pod.containers[0].env.clone())
                .unwrap_or_default()
                .into_iter()
                .map(|var| (var.name, var.value.unwrap_or_default()))
                .collect::<Vec<_>>()
        };
        // other variables are kept and HOME/KUBECONFIG name the thorium user's mounts
        let scaler = &component_templates(spec.clone())[1];
        assert_eq!(
            env(scaler),
            [
                ("https_proxy".to_owned(), "http://proxy:3128".to_owned()),
                ("HOME".to_owned(), "/home/thorium".to_owned()),
                ("KUBECONFIG".to_owned(), "/home/thorium/.kube/config".to_owned()),
            ]
        );
        // a scaler using its service account gets no KUBECONFIG
        spec["components"]["scaler"]["service_account"] = serde_json::json!(true);
        let scaler = &component_templates(spec)[1];
        assert_eq!(
            env(scaler),
            [
                ("https_proxy".to_owned(), "http://proxy:3128".to_owned()),
                ("HOME".to_owned(), "/home/thorium".to_owned()),
            ]
        );
    }

    /// Scalers removed from the spec have their Deployments deleted
    #[tokio::test]
    async fn removed_scalers_are_deleted() {
        // a cluster with only the api whose scaler Deployments are already gone
        let fake = FakeKube::default();
        let meta = meta_for(
            namespaced_cluster(serde_json::json!({
                "components": {"api": {}},
                "registry": "registry/thorium",
                "config": {}
            })),
            &fake.client(),
        );
        let hashes = RolloutHashes {
            config: String::new(),
            mounts: String::new(),
        };
        deploy_scalers(&meta, &Vec::new(), &hashes, &hashes)
            .await
            .expect("deploy scalers");
        // both scaler Deployments were deleted and nothing was created
        let paths = fake
            .writes()
            .into_iter()
            .map(|request| format!("{} {}", request.method, request.path))
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            [
                "DELETE /apis/apps/v1/namespaces/thorium/deployments/scaler",
                "DELETE /apis/apps/v1/namespaces/thorium/deployments/baremetal-scaler"
            ]
        );
    }
}
