use k8s_openapi::api::core::v1::Namespace;
use kube::api::{Api, ListParams, ObjectList, PostParams};
use std::collections::{BTreeMap, HashSet};
use tracing::{event, instrument, Level, Span};

/// The label key naming the tool that manages a Kubernetes resource
const MANAGED_BY_LABEL: &str = "app.kubernetes.io/managed-by";

/// The managed-by label value on every namespace the scaler creates
const MANAGED_BY_SCALER: &str = "thorium-scaler";

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
    pub async fn list(&self) -> Result<ObjectList<Namespace>, kube::Error> {
        self.api.list(&ListParams::default()).await
    }

    /// Build a group namespace labelled as managed by the scaler
    ///
    /// The label lets cleanup tooling find the namespaces the scaler created.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the namespace
    fn labelled(name: &str) -> Namespace {
        // name the namespace and mark it as created by the scaler
        let mut ns = Namespace::default();
        ns.metadata.name = Some(name.to_owned());
        ns.metadata.labels = Some(BTreeMap::from([(
            MANAGED_BY_LABEL.to_owned(),
            MANAGED_BY_SCALER.to_owned(),
        )]));
        ns
    }

    /// Create a namespace
    ///
    /// # Arguments
    ///
    /// * `name` - The namespace to create
    /// * `bans` - The namespaces that failed to be created, which this one is added to on failure
    #[instrument(name = "k8s::Namespaces::create", skip(self, bans))]
    pub async fn create(&self, name: &str, bans: &mut HashSet<String>) {
        // build create params
        let params = PostParams::default();
        // build a namespace labelled as created by the scaler
        let ns = Self::labelled(name);
        // log that we are trying to create a namespace
        match self.api.create(&params, &ns).await {
            Ok(_) => event!(Level::INFO, msg = "Created namespace", namespace = name),
            Err(err) => {
                // log that we failed to create this namespacei and are banning it
                event!(
                    Level::ERROR,
                    msg = "Failed to create namespace",
                    namespace = name,
                    ban = name,
                    error = err.to_string()
                );
                // ban this namespace
                bans.insert(name.to_owned());
            }
        }
    }
}

impl std::fmt::Debug for Namespaces {
    /// Implement debug
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NameSpaces").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Group namespaces carry the scaler's managed-by label
    #[test]
    fn namespaces_labelled_as_scaler_managed() {
        // build a group namespace
        let ns = Namespaces::labelled("static");
        // it is named after the group and labelled as created by the scaler
        assert_eq!(ns.metadata.name.as_deref(), Some("static"));
        let labels = ns.metadata.labels.expect("labels");
        assert_eq!(labels.len(), 1);
        assert_eq!(labels[MANAGED_BY_LABEL], MANAGED_BY_SCALER);
    }
}
