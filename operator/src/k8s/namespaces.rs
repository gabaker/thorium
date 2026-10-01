use k8s_openapi::api::core::v1::Namespace;
use kube::{
    api::{ListParams, ObjectList, PostParams},
    Api, Client,
};
use thorium::Error;

/// Wrapper for namespace commands in k8s
pub struct Namespaces {
    /// API client for calling namespace commands in k8s
    api: Api<Namespace>,
}

impl Namespaces {
    /// Build a new wrapper for k8s functions regarding namespaces
    ///
    /// # Arguments
    ///
    /// * `client` - Kubernetes client
    pub fn new(client: &kube::Client) -> Self {
        // get namespaces api
        let api: Api<Namespace> = Api::all(client.clone());
        Namespaces { api }
    }

    /// List all namespaces
    #[allow(dead_code)]
    pub async fn list(&self) -> Result<ObjectList<Namespace>, kube::Error> {
        self.api.list(&ListParams::default()).await
    }

    /// Create a namespace
    ///
    /// # Arguments
    ///
    /// * `name` - The namespace to create
    pub async fn create(&self, name: &str) -> Result<(), Error> {
        // build create params
        let params = PostParams::default();
        // create namespace
        let mut namespace = Namespace::default();
        namespace.metadata.name = Some(name.to_owned());
        // create the namespace and handle errors if returned
        match self.api.create(&params, &namespace).await {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(error)) => {
                // do not panic if namespace exists
                if error.reason == "AlreadyExists" {
                    eprintln!("Warning: namespace \"{}\" already exists", name);
                    Ok(())
                } else {
                    Err(Error::new(format!(
                        "Failed to create namespace: {:?}",
                        error
                    )))
                }
            }
            Err(error) => Err(Error::new(format!(
                "Failed to create namespace: {:?}",
                error
            ))),
        }
    }
}

/// Attempt to create the ThoriumCluster namespace
///
/// # Arguments
///
/// * `namespace` - The namespace to create in k8s
pub async fn try_create(namespace: &str) -> Result<(), Error> {
    let client = Client::try_default()
        .await
        .expect("Failed to read kubeconfig from default paths.");
    let namespaces = Namespaces::new(&client);
    // create the namespace
    namespaces.create(namespace).await?;
    Ok(())
}
