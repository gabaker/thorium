use k8s_openapi::api::core::v1::ConfigMap;
use kube::Api;
use kube::api::{DeleteParams, ObjectMeta, Patch, PatchParams, PostParams};
use std::collections::BTreeMap;
use thorium::{Error, conf::Tracing};

use super::clusters::ClusterMeta;

/// The `ConfigMap` holding the tracing.yml the node provision pods install
pub const TRACING_CONFIG_MAP: &str = "tracing-conf";

/// Create or update a `ConfigMap`
///
/// This creates a kubernetes `ConfigMap` in the `ThoriumCluster` namespace using a
/// preconstructed `ConfigMap` object, or patches its data if it already exists.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `cm` - Kubernetes `ConfigMap` to create or patch
pub async fn create_or_update(meta: &ClusterMeta, cm: &ConfigMap) -> Result<(), Error> {
    // get the name of this ConfigMap for logging and patching
    let Some(name) = cm.metadata.name.as_deref() else {
        return Err(Error::new("Cannot create a ConfigMap without a name"));
    };
    // first attempt to create the ConfigMap
    let params = PostParams::default();
    match meta.cm_api.create(&params, cm).await {
        Ok(_) => {
            println!("Created {name} ConfigMap in namespace {}", meta.namespace);
            Ok(())
        }
        // an existing ConfigMap has the keys we manage overwritten
        Err(kube::Error::Api(error)) if error.reason == "AlreadyExists" => {
            let patch = serde_json::json!({
                "data": cm.data
            });
            let patch = Patch::Merge(&patch);
            let params: PatchParams = PatchParams::default();
            match meta.cm_api.patch(name, &params, &patch).await {
                Ok(_) => {
                    println!("Patched {name} ConfigMap in namespace {}", meta.namespace);
                    Ok(())
                }
                Err(error) => Err(Error::new(format!(
                    "Failed to patch {name} ConfigMap: {error}"
                ))),
            }
        }
        Err(error) => Err(Error::new(format!(
            "Failed to create {name} ConfigMap: {error}"
        ))),
    }
}

/// Create Thorium tracing `ConfigMap`
///
/// The tracing.yml `ConfigMap` is used by the node provision pods, which install it for the
/// agents and the Thorium reactor; its content comes from the tracing section of the
/// `ThoriumCluster`'s Thorium config.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `tracing` - Thorium's tracing configuration settings
async fn create_tracing_cm(meta: &ClusterMeta, tracing: &Tracing) -> Result<(), Error> {
    // render the tracing config as YAML, going through JSON so enums aren't written as YAML tags
    let tracing_yaml = serde_norway::to_string(&serde_json::to_value(tracing)?)?;
    let data = BTreeMap::from([("tracing.yml".to_owned(), tracing_yaml)]);
    // build the tracing ConfigMap
    let tracing_conf = ConfigMap {
        metadata: ObjectMeta {
            name: Some(TRACING_CONFIG_MAP.to_owned()),
            namespace: Some(meta.namespace.clone()),
            ..Default::default()
        },
        data: Some(data),
        ..Default::default()
    };
    // create or update the tracing ConfigMap
    create_or_update(meta, &tracing_conf).await?;
    Ok(())
}

/// Create all `ConfigMaps` for a Thorium cluster
///
/// Since most of Thorium's config holds secrets, the only `ConfigMap` the operator creates
/// is the tracing.yml config.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
pub async fn create_or_update_all(meta: &ClusterMeta) -> Result<(), Error> {
    // create tracing config
    create_tracing_cm(meta, &meta.conf.thorium.tracing).await?;
    Ok(())
}

/// The chart-created `ConfigMap` holding the login banner the API mounts
pub const BANNER_CONFIG_MAP: &str = "banner";

/// The key in [`BANNER_CONFIG_MAP`] holding the banner text
const BANNER_KEY: &str = "banner.txt";

/// Read the login banner the API mounts and only reads at startup
///
/// A missing `ConfigMap` or key is an empty banner, since the API mounts it as optional.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
pub async fn banner(meta: &ClusterMeta) -> Result<String, Error> {
    // get the banner ConfigMap if it exists
    let cm = meta
        .cm_api
        .get_opt(BANNER_CONFIG_MAP)
        .await
        .map_err(|error| {
            Error::new(format!(
                "Failed to get {BANNER_CONFIG_MAP} ConfigMap in {}: {error}",
                meta.namespace
            ))
        })?;
    // take the banner text out of it
    Ok(cm
        .and_then(|cm| cm.data)
        .and_then(|mut data| data.remove(BANNER_KEY))
        .unwrap_or_default())
}

/// Cleanup Thorium config maps
///
/// This deletes the tracing `ConfigMap` when a `ThoriumCluster` is deleted; every other config
/// the operator renders is a Secret, and the banner and upgrade state `ConfigMaps` are kept.
///
/// # Arguments
///
/// * `cm_api` - The `ConfigMap` API for the `ThoriumCluster`'s namespace
pub async fn delete(cm_api: &Api<ConfigMap>) -> Result<(), Error> {
    let params: DeleteParams = DeleteParams::default();
    // delete the tracing ConfigMap
    match cm_api.delete(TRACING_CONFIG_MAP, &params).await {
        Ok(_) => println!("Deleted {TRACING_CONFIG_MAP} ConfigMap"),
        // a missing ConfigMap is already in the state we want
        Err(kube::Error::Api(error)) if error.code == 404 => {
            println!("ConfigMap {TRACING_CONFIG_MAP} does not exist, skipping deletion");
        }
        Err(error) => {
            return Err(Error::new(format!(
                "Could not delete {TRACING_CONFIG_MAP} ConfigMap: {error}"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::k8s::clusters::tests::{
        FakeKube, full_spec, kube_status, meta_for, namespaced_cluster,
    };

    /// The path `ConfigMaps` are created at in the test namespace
    const CONFIG_MAPS_PATH: &str = "/api/v1/namespaces/thorium/configmaps";

    /// The tracing config is written without YAML tags and patched when it already exists
    #[tokio::test]
    async fn tracing_config_map_is_rendered_without_tags() {
        // the tracing ConfigMap already exists
        let fake = FakeKube::default()
            .route(
                "POST",
                CONFIG_MAPS_PATH,
                409,
                kube_status(409, "AlreadyExists"),
            )
            .route(
                "PATCH",
                &format!("{CONFIG_MAPS_PATH}/{TRACING_CONFIG_MAP}"),
                200,
                serde_json::json!({}),
            );
        let meta = meta_for(namespaced_cluster(full_spec()), &fake.client());
        create_or_update_all(&meta).await.expect("tracing");
        // the created ConfigMap holds tracing.yml without YAML tags the agents would reject
        let writes = fake.writes();
        assert_eq!(writes[0].body["metadata"]["name"], TRACING_CONFIG_MAP);
        let yaml = writes[0].body["data"]["tracing.yml"]
            .as_str()
            .expect("tracing.yml");
        assert!(!yaml.contains('!'), "{yaml}");
        assert!(yaml.contains("Grpc"));
        // the existing ConfigMap has only its data patched
        assert_eq!(writes[1].method, "PATCH");
        assert_eq!(writes[1].body["data"]["tracing.yml"], yaml);
    }

    /// A missing banner is empty and an existing one is read
    #[tokio::test]
    async fn banner_is_optional() {
        // a namespace without a banner has an empty one
        let fake = FakeKube::default();
        let meta = meta_for(namespaced_cluster(full_spec()), &fake.client());
        assert_eq!(banner(&meta).await.expect("banner"), "");
        // a banner ConfigMap's text is read
        let fake = FakeKube::default().route(
            "GET",
            &format!("{CONFIG_MAPS_PATH}/{BANNER_CONFIG_MAP}"),
            200,
            serde_json::json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "banner"}, "data": {"banner.txt": "hello"}}),
        );
        let meta = meta_for(namespaced_cluster(full_spec()), &fake.client());
        assert_eq!(banner(&meta).await.expect("banner"), "hello");
    }

    /// Cleanup only deletes the tracing `ConfigMap` and tolerates it being gone
    #[tokio::test]
    async fn cleanup_keeps_other_config_maps() {
        // the tracing ConfigMap is already gone
        let fake = FakeKube::default();
        let api: Api<ConfigMap> = Api::namespaced(fake.client(), "thorium");
        delete(&api).await.expect("delete");
        // only the tracing ConfigMap was deleted, never the banner or upgrade state
        let writes = fake.writes();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].method, "DELETE");
        assert_eq!(
            writes[0].path,
            format!("{CONFIG_MAPS_PATH}/{TRACING_CONFIG_MAP}")
        );
        // any other failure is an error
        let fake = FakeKube::default().route(
            "DELETE",
            &format!("{CONFIG_MAPS_PATH}/{TRACING_CONFIG_MAP}"),
            403,
            kube_status(403, "Forbidden"),
        );
        let api: Api<ConfigMap> = Api::namespaced(fake.client(), "thorium");
        assert!(delete(&api).await.is_err());
    }
}
