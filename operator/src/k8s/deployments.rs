use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::Pod;
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

/// Build JSON template for api deployment
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
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
            "replicas": api_spec.replicas.clone(),
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
                    "containers": [
                        {
                            "name": "api",
                            "image": meta.cluster.get_image(),
                            "command": api_spec.cmd.clone(),
                            "args": api_spec.args.clone(),
                            "imagePullPolicy": meta.cluster.spec.image_pull_policy.clone(),
                            "resources": {
                                "limits": crds::Resources::request_conv(&api_spec.resources).expect("failed to convert resources to valid request format"),
                                "requests": crds::Resources::request_conv(&api_spec.resources).expect("failed to convert resources to valid request format"),
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

/// Build JSON template for baremetal-scaler deployment
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
async fn scaler_template(meta: &ClusterMeta, host_aliases: &Vec<K8sHostAliases>) -> Option<Value> {
    let scaler_spec = meta.cluster.get_scaler_spec();
    match scaler_spec {
        Some(scaler_spec) => {
            // reference the chart's pull secret and the one rendered from registry_auth
            let image_pull_secrets = meta.cluster.image_pull_secrets();
            let mut volumes = serde_json::json!([
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
            ]);
            let mut volume_mounts = serde_json::json!([
                {
                    "name": "config",
                    "mountPath": "/conf/thorium.yml",
                    "subPath": "thorium.yml"
                },
                {
                    "name": "keys",
                    "mountPath": "/keys/keys.yml",
                    "subPath": "keys.yml"
                },
            ]);
            // only include skopeo secret when registry auth is configured
            if !meta.cluster.spec.registry_auth.is_none() {
                volumes.as_array_mut().unwrap().push(serde_json::json!({
                    "name": "docker-skopeo",
                    "secret": {
                        "secretName": "docker-skopeo"
                    }
                }));
                volume_mounts
                    .as_array_mut()
                    .unwrap()
                    .push(serde_json::json!({
                        "name": "docker-skopeo",
                        "mountPath": "/root/.docker"
                    }));
            }
            if !scaler_spec.service_account {
                // if not using service account we must map in a user created kube-config secret
                volumes.as_array_mut().unwrap().push(serde_json::json!({
                    "name": "kube-config",
                    "secret": {
                        "secretName": "kube-config"
                    }
                }));
                volume_mounts
                    .as_array_mut()
                    .unwrap()
                    .push(serde_json::json!({
                        "name": "kube-config",
                        "mountPath": "/root/.kube/config",
                        "subPath": "config"
                    }));
            };
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
                            "automountServiceAccountToken": scaler_spec.service_account.clone(),
                            "containers": [
                                {
                                    "name": "scaler",
                                    "image": meta.cluster.get_image(),
                                    "imagePullPolicy": meta.cluster.spec.image_pull_policy.clone(),
                                    "command": scaler_spec.cmd.clone(),
                                    "args": scaler_spec.args.clone(),
                                    "resources": {
                                        "limits": crds::Resources::request_conv(&scaler_spec.resources).expect("failed to convert resources to valid request format"),
                                        "requests": crds::Resources::request_conv(&scaler_spec.resources).expect("failed to convert resources to valid request format"),
                                    },
                                    "env": scaler_spec.env.clone(),
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
        None => None,
    }
}

/// Build JSON template for baremetal-scaler deployment
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
async fn baremetal_scaler_template(
    meta: &ClusterMeta,
    host_aliases: &Vec<K8sHostAliases>,
) -> Option<Value> {
    let scaler_spec = meta.cluster.get_baremetal_scaler_spec();
    // reference the chart's pull secret and the one rendered from registry_auth
    let image_pull_secrets = meta.cluster.image_pull_secrets();
    match scaler_spec {
        Some(scaler_spec) => Some(serde_json::json!({
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
                        "containers": [
                            {
                                "name": "baremetal-scaler",
                                "image": meta.cluster.get_image(),
                                "imagePullPolicy": meta.cluster.spec.image_pull_policy.clone(),
                                "command": scaler_spec.cmd.clone(),
                                "args": scaler_spec.args.clone(),
                                "resources": {
                                    "limits": crds::Resources::request_conv(&scaler_spec.resources).expect("failed to convert resources to valid request format"),
                                    "requests": crds::Resources::request_conv(&scaler_spec.resources).expect("failed to convert resources to valid request format"),
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
        })),
        None => None,
    }
}

/// Build JSON template for event-handler deployment
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
async fn event_handler_template(
    meta: &ClusterMeta,
    host_aliases: &Vec<K8sHostAliases>,
) -> Option<Value> {
    let handler_spec = meta.cluster.get_event_handler_spec();
    // reference the chart's pull secret and the one rendered from registry_auth
    let image_pull_secrets = meta.cluster.image_pull_secrets();
    match handler_spec {
        Some(handler_spec) => Some(serde_json::json!({
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
                        "containers": [
                            {
                                "name": "event-handler",
                                "image": meta.cluster.get_image(),
                                "imagePullPolicy": meta.cluster.spec.image_pull_policy.clone(),
                                "command": handler_spec.cmd.clone(),
                                "args": handler_spec.args.clone(),
                                "resources": {
                                    "limits": crds::Resources::request_conv(&handler_spec.resources).expect("failed to convert resources to valid request format"),
                                    "requests": crds::Resources::request_conv(&handler_spec.resources).expect("failed to convert resources to valid request format"),
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
        })),
        None => None,
    }
}

/// Build JSON template for search-streamer deployment
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
async fn search_streamer_template(
    meta: &ClusterMeta,
    host_aliases: &Vec<K8sHostAliases>,
) -> Option<Value> {
    let streamer_spec = meta.cluster.get_search_streamer_spec();
    // reference the chart's pull secret and the one rendered from registry_auth
    let image_pull_secrets = meta.cluster.image_pull_secrets();
    match streamer_spec {
        Some(streamer_spec) => Some(serde_json::json!({
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
                        "containers": [
                            {
                                "name": "search-streamer",
                                "image": meta.cluster.get_image(),
                                "imagePullPolicy": meta.cluster.spec.image_pull_policy.clone(),
                                "command": streamer_spec.cmd.clone(),
                                "args": streamer_spec.args.clone(),
                                "resources": {
                                    "limits": crds::Resources::request_conv(&streamer_spec.resources).expect("failed to convert resources to valid request format"),
                                    "requests": crds::Resources::request_conv(&streamer_spec.resources).expect("failed to convert resources to valid request format"),
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
        })),
        None => None,
    }
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
    // get the name of this deployment to create or patch
    let params = PostParams::default();
    let name = deployment
        .metadata
        .name
        .clone()
        .expect("could not get cluster name from metadata");
    match meta.deploy_api.create(&params, &deployment).await {
        Ok(_) => {
            println!(
                "Deployment created {} in namespace {}",
                &name, &meta.namespace
            );
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
                        println!(
                            "Patched {} deployment in namespace {}",
                            &name, &meta.namespace
                        );
                        Ok(())
                    }
                    Err(error) => Err(Error::new(format!(
                        "Failed to patch {} deployment: {}",
                        &name, error
                    ))),
                }
            } else {
                Err(Error::new(format!(
                    "Failed to create {} deployment: {}",
                    &name, error
                )))
            }
        }
        Err(error) => Err(Error::new(format!(
            "Failed to create {} deployment: {}",
            &name, error
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
            "Deleted {} deployment from namespace {}",
            name, &meta.namespace
        ),
        Err(kube::Error::Api(error)) => {
            // don't panic if deployment doesn't exist, thats the desired state
            if error.code == 404 {
                println!(
                    "No {} deployment in namespace {} to delete, skipping cleanup",
                    &name, &meta.namespace
                );
                return Ok(());
            }
            return Err(Error::new(format!(
                "Failed things to delete {} deployment in namespace {}: {}",
                &name, &meta.namespace, error.message
            )));
        }
        Err(error) => {
            return Err(Error::new(format!(
                "Failed things to delete {} deployment in namespace {}: {}",
                &name, &meta.namespace, error
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
    // build the api deployment ot deploy
    let deployment = api(meta, host_aliases)?;
    // create or update this deployment
    create_or_update(deployment, meta, hashes).await?;
    Ok(())
}

/// Create or update the scaler deployment
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
    if let Some(deployment) = scaler_template(meta, host_aliases).await {
        let deployment: Deployment = serde_json::from_value(deployment)?;
        create_or_update(deployment, meta, scaler_hashes).await?;
    // component not present in cluster spec during upgrades, cleanup
    } else {
        delete_one("scaler", meta).await?;
    }
    // deploy any baremetal scaler from template
    if let Some(deployment) = baremetal_scaler_template(meta, host_aliases).await {
        let deployment: Deployment = serde_json::from_value(deployment)?;
        create_or_update(deployment, meta, hashes).await?;
    // component not present in cluster spec during upgrades, cleanup
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
    if let Some(deployment) = event_handler_template(meta, host_aliases).await {
        let deployment: Deployment = serde_json::from_value(deployment)?;
        create_or_update(deployment, meta, hashes).await?;
    // component not present in cluster spec during upgrades, cleanup
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
    if let Some(deployment) = search_streamer_template(meta, host_aliases).await {
        let deployment: Deployment = serde_json::from_value(deployment)?;
        create_or_update(deployment, meta, hashes).await?;
    // component not present in cluster spec during upgrades, cleanup
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
    // check whether any pod is really failing
    let failing = pods.iter().any(pod_failing);
    // describe the first few problems we find
    let problems = pods
        .iter()
        .flat_map(pod_problems)
        .take(MAX_POD_PROBLEMS)
        .collect::<Vec<String>>();
    let details = if problems.is_empty() {
        "no pod reported an error".to_owned()
    } else {
        problems.join("; ")
    };
    RolloutProblems { details, failing }
}

/// Describe the pending deployments whose pods are failing, if any are
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `pending` - The deployments that haven't finished rolling out
async fn failing_rollouts(meta: &ClusterMeta, pending: &[String]) -> Option<String> {
    // describe each pending deployment that has a failing pod
    let mut details = Vec::new();
    for name in pending {
        let problems = rollout_problems(meta, name).await;
        if problems.failing {
            details.push(format!("{name}: {}", problems.details));
        }
    }
    // only report when something is really failing
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
/// * `cluster` - The `ThoriumCluster` whose status to check
/// * `waiting` - The message naming the pending deployments
fn status_reports_wait(cluster: &ThoriumCluster, waiting: &str) -> bool {
    // get the current status if it describes our current spec
    let Some(status) = cluster
        .status
        .as_ref()
        .filter(|status| status.observed_generation == cluster.metadata.generation)
    else {
        return false;
    };
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
        if reported.as_ref() != Some(&waiting) && !status_reports_wait(&meta.cluster, &waiting) {
            crds::set_status(
                &meta.client,
                &meta.cluster,
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
            Err(kube::Error::Api(error)) => {
                // don't panic if pods don't exist, thats the desired state
                if error.code == 404 {
                    println!("No {} deployment to delete, skipping cleanup", &deployment);
                    continue;
                }
                return Err(Error::new(format!(
                    "Failed to delete {} deployment: {}",
                    &deployment, error.message
                )));
            }
            Err(error) => {
                return Err(Error::new(format!(
                    "Failed to delete {} deployment: {}",
                    &deployment, error
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

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
        assert_eq!(problems.details, "no pod reported an error");
    }

    /// A status already reporting the same wait for the current spec is kept so its pod
    /// details aren't replaced by the bare wait on every requeue
    #[test]
    fn status_wait_detection() {
        // build a cluster at generation 2 reporting a wait with pod details
        let spec: crds::ThoriumClusterSpec = serde_json::from_value(serde_json::json!({
            "components": {"api": {}},
            "registry": "registry/thorium",
            "config": {}
        }))
        .expect("spec should deserialize");
        let mut cluster = ThoriumCluster::new("thorium", spec);
        cluster.metadata.generation = Some(2);
        let status = |message: &str, generation: i64| crds::ThoriumClusterStatus {
            phase: Some(crds::ClusterPhase::Provisioning),
            message: Some(message.to_owned()),
            observed_generation: Some(generation),
            ..Default::default()
        };
        let waiting = "Waiting for event-handler to roll out";
        cluster.status = Some(status(
            "Waiting for event-handler to roll out: event-handler: pod crashed",
            2,
        ));
        assert!(status_reports_wait(&cluster, waiting));
        // the bare wait is reported too
        cluster.status = Some(status(waiting, 2));
        assert!(status_reports_wait(&cluster, waiting));
        // a wait on other deployments isn't this wait
        cluster.status = Some(status(
            "Waiting for event-handler, search-streamer to roll out",
            2,
        ));
        assert!(!status_reports_wait(&cluster, waiting));
        // a status describing an older spec doesn't count
        cluster.status = Some(status(waiting, 1));
        assert!(!status_reports_wait(&cluster, waiting));
        // a cluster without a status doesn't report anything
        cluster.status = None;
        assert!(!status_reports_wait(&cluster, waiting));
    }
}
