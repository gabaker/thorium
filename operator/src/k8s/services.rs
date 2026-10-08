use k8s_openapi::api::core::v1::Service;
use kube::{
    Api,
    api::{DeleteParams, Patch, PatchParams, PostParams},
    runtime::reflector::Lookup,
};
use thorium::Error;

use super::clusters::ClusterMeta;

/// Build a Thorium API service
///
/// This creates an API service JSON template that can be used to create a
/// kubernetes application service for the Thorium API.
fn api_service() -> Result<Service, serde_json::Error> {
    // build the template for a thorium api service
    let template = serde_json::json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": {
            "name": "thorium-api"
        },
        "spec": {
            "selector": {
                "app": "api"
            },
            "ports": [
                {
                    "name": "web",
                    "port": 80,
                    "targetPort": 80
                }
            ],
            "type": "ClusterIP"
        }
    });
    // parse our template into a service
    serde_json::from_value(template)
}

/// Build a Thorium API MCP service
///
/// This creates an API service JSON template that can be used to create a
/// kubernetes application service for the Thorium MCP API. The selector requires `app: api` as
/// well as the MCP watcher's `mcp: enabled` label so a pod of another app that carries the
/// label never receives MCP traffic.
fn mcp_service() -> Result<Service, serde_json::Error> {
    // build the template for a thorium mcp service
    let template = serde_json::json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": {
            "name": "thorium-mcp"
        },
        "spec": {
            "selector": {
                "app": "api",
                "mcp": "enabled"
            },
            "ports": [
                {
                    "name": "web",
                    "port": 80,
                    "targetPort": 80
                }
            ],
            "type": "ClusterIP"
        }
    });
    // parse our template into a service
    serde_json::from_value(template)
}

/// Patch an existing service to match a template
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `name` - The name of the service to patch
/// * `service` - The service template to patch towards
async fn update(meta: &ClusterMeta, name: &str, service: &Service) -> Result<(), Error> {
    // patch only the spec so fields k8s assigns (like the cluster IP) are kept
    let patch = serde_json::json!({
        "spec": &service.spec
    });
    let patch = Patch::Merge(&patch);
    // use default patch params
    let params: PatchParams = PatchParams::default();
    // apply this patch
    match meta.service_api.patch(name, &params, &patch).await {
        Ok(_) => {
            // log that we patched this service
            println!("Patched {name} service in namespace {}", meta.namespace);
            Ok(())
        }
        // an error occured during patching
        Err(error) => Err(Error::new(format!(
            "Failed to patch {name} service in namespace {}: {error}",
            meta.namespace
        ))),
    }
}

/// Create a service from a template, or patch it if it already exists
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `service` - The service to create or update
pub async fn create_or_update(meta: &ClusterMeta, service: &Service) -> Result<(), Error> {
    // get this services name
    let Some(name) = service.name() else {
        return Err(Error::new(format!(
            "Failed to get name of service: {service:?}"
        )));
    };
    // use default post params
    let post_params = PostParams::default();
    // try to create this service
    match meta.service_api.create(&post_params, service).await {
        // log that we successfully created this service
        Ok(_) => {
            println!("{name} service created in namespace {}", meta.namespace);
            Ok(())
        }
        // this service already exists so patch it instead
        Err(kube::Error::Api(error)) if error.reason == "AlreadyExists" => {
            update(meta, &name, service).await
        }
        // any other failure needs attention
        Err(error) => Err(Error::new(format!(
            "Failed to create {name} service in namespace {}: {error}",
            meta.namespace
        ))),
    }
}

/// Create or update the Thorium API and MCP services
///
/// These services route network traffic to the API from internal or external locations
/// (using an ingress proxy like Traefik).
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
pub async fn create_or_update_all(meta: &ClusterMeta) -> Result<(), Error> {
    // build API service template
    let api_service = api_service()?;
    // create or update the API service
    create_or_update(meta, &api_service).await?;
    // build mcp service template
    let mcp_service = mcp_service()?;
    // create or update the MCP service, which routes to the one API pod labelled for MCP
    create_or_update(meta, &mcp_service).await
}

/// Cleanup the Thorium API and MCP services
///
/// This deletes the `thorium-api` and `thorium-mcp` services the operator creates.
///
/// # Arguments
///
/// * `service_api` - The Service API for the `ThoriumCluster`'s namespace
pub async fn delete(service_api: &Api<Service>) -> Result<(), Error> {
    let params: DeleteParams = DeleteParams::default();
    // delete each service we create
    for service_name in ["thorium-api", "thorium-mcp"] {
        match service_api.delete(service_name, &params).await {
            Ok(_) => println!("Deleted {service_name} service"),
            // a missing service is already in the state we want
            Err(kube::Error::Api(error)) if error.code == 404 => {
                println!("Service {service_name} does not exist, skipping deletion");
            }
            Err(kube::Error::Api(error)) => {
                return Err(Error::new(format!(
                    "Could not delete {service_name} service: {}",
                    error.message
                )));
            }
            Err(error) => {
                return Err(Error::new(format!(
                    "Could not delete {service_name} service: {error}"
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
        FakeKube, full_spec, kube_status, meta_for, namespaced_cluster,
    };

    /// The path Services are created at in the test namespace
    const SERVICES_PATH: &str = "/api/v1/namespaces/thorium/services";

    /// Get the selector and first port of a service
    ///
    /// # Arguments
    ///
    /// * `service` - The service to inspect
    fn selector_and_port(service: &Service) -> (serde_json::Value, i32) {
        // get the service spec
        let spec = service.spec.as_ref().expect("spec");
        let port = spec.ports.as_ref().expect("ports")[0].port;
        (
            serde_json::to_value(&spec.selector).expect("selector"),
            port,
        )
    }

    /// The API service routes to every API pod and the MCP service to the labelled one
    #[test]
    fn services_select_their_pods() {
        // the API service selects the api Deployment's pods
        let api = api_service().expect("api service");
        assert_eq!(api.metadata.name.as_deref(), Some("thorium-api"));
        assert_eq!(
            selector_and_port(&api),
            (serde_json::json!({"app": "api"}), 80)
        );
        // the MCP service selects the api pod the MCP watcher labels
        let mcp = mcp_service().expect("mcp service");
        assert_eq!(mcp.metadata.name.as_deref(), Some("thorium-mcp"));
        assert_eq!(
            selector_and_port(&mcp),
            (serde_json::json!({"app": "api", "mcp": "enabled"}), 80)
        );
    }

    /// Existing services have only their spec patched so assigned fields are kept
    #[tokio::test]
    async fn existing_services_are_patched() {
        // both services already exist
        let fake = FakeKube::default()
            .route(
                "POST",
                SERVICES_PATH,
                409,
                kube_status(409, "AlreadyExists"),
            )
            .route(
                "PATCH",
                &format!("{SERVICES_PATH}/thorium-api"),
                200,
                serde_json::json!({}),
            )
            .route(
                "PATCH",
                &format!("{SERVICES_PATH}/thorium-mcp"),
                200,
                serde_json::json!({}),
            );
        let meta = meta_for(namespaced_cluster(full_spec()), &fake.client());
        create_or_update_all(&meta).await.expect("services");
        // each service is created and then patched with just its spec
        let patches = fake
            .writes()
            .into_iter()
            .filter(|request| request.method == "PATCH")
            .collect::<Vec<_>>();
        assert_eq!(patches.len(), 2);
        assert!(
            patches
                .iter()
                .all(|patch| patch.body.get("metadata").is_none())
        );
        assert_eq!(patches[0].body["spec"]["selector"]["app"], "api");
        // an MCP service created with only the mcp label gains the app label through the
        // merge patch
        assert_eq!(
            patches[1].body["spec"]["selector"],
            serde_json::json!({"app": "api", "mcp": "enabled"})
        );
        // any other failure stops the update
        let fake =
            FakeKube::default().route("POST", SERVICES_PATH, 403, kube_status(403, "Forbidden"));
        let meta = meta_for(namespaced_cluster(full_spec()), &fake.client());
        assert!(create_or_update_all(&meta).await.is_err());
    }
}
