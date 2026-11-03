//! Delete a `ThoriumCluster` and wait for the operator to clean it up, as the chart's uninstall
//! hook does before Helm removes the operator

use kube::{Api, Client, api::DeleteParams};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use super::crds::ThoriumCluster;

/// How long to wait between checks for the deleted `ThoriumCluster`
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Delete a `ThoriumCluster` and wait until the operator's finalizer releases it
///
/// The cluster being gone already counts as success. A refused delete fails at once; a check
/// that fails while waiting is reported and retried until the timeout, after which the
/// cluster's last status is printed and the command fails.
///
/// # Arguments
///
/// * `client` - The kube client for the cluster the `ThoriumCluster` is in
/// * `namespace` - The namespace of the `ThoriumCluster`
/// * `name` - The name of the `ThoriumCluster`
/// * `timeout` - How long to wait for the operator to clean up
/// * `interval` - How long to wait between checks
pub async fn delete_and_wait(
    client: &Client,
    namespace: &str,
    name: &str,
    timeout: Duration,
    interval: Duration,
) -> ExitCode {
    // talk to ThoriumClusters in the release namespace
    let api: Api<ThoriumCluster> = Api::namespaced(client.clone(), namespace);
    // ask for the delete, which the operator's finalizer holds until it has cleaned up
    match api.delete(name, &DeleteParams::default()).await {
        Ok(_) => println!("Deleting the ThoriumCluster; waiting for the operator to clean up"),
        Err(kube::Error::Api(response)) if response.code == 404 => {
            println!("The ThoriumCluster is already gone");
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            eprintln!("Failed to delete the ThoriumCluster {namespace}/{name}: {error}");
            return ExitCode::FAILURE;
        }
    }
    // check until the cluster is gone or the timeout passes, keeping the last cluster seen
    let deadline = Instant::now() + timeout;
    let mut last = None;
    loop {
        match api.get_opt(name).await {
            Ok(None) => {
                println!("The ThoriumCluster has been cleaned up");
                return ExitCode::SUCCESS;
            }
            Ok(Some(cluster)) => last = Some(cluster),
            // a failed check may be the API server briefly unavailable, so keep waiting
            Err(error) => eprintln!("Failed to check the ThoriumCluster: {error}"),
        }
        // stop once another wait would pass the deadline
        if Instant::now() + interval > deadline {
            break;
        }
        tokio::time::sleep(interval).await;
    }
    // report what the operator last said about the cluster
    let status = last
        .and_then(|cluster| cluster.status)
        .and_then(|status| serde_json::to_string(&status).ok())
        .unwrap_or_else(|| "{}".to_owned());
    eprintln!(
        "Timed out after {}s waiting for the operator to clean up the ThoriumCluster",
        timeout.as_secs()
    );
    eprintln!("Last status: {status}");
    ExitCode::FAILURE
}

/// Delete a `ThoriumCluster` with the in-cluster or local kube config and wait for it to go
///
/// # Arguments
///
/// * `namespace` - The namespace of the `ThoriumCluster`
/// * `name` - The name of the `ThoriumCluster`
/// * `timeout` - How long to wait for the operator to clean up
pub async fn run(namespace: &str, name: &str, timeout: Duration) -> ExitCode {
    // set the crypto provider the kube client needs
    super::controller::install_crypto_provider();
    // connect to the cluster this runs in
    let client = match Client::try_default().await {
        Ok(client) => client,
        Err(error) => {
            eprintln!("Failed to connect to Kubernetes: {error}");
            return ExitCode::FAILURE;
        }
    };
    // delete the cluster and wait for the operator to release it
    delete_and_wait(&client, namespace, name, timeout, POLL_INTERVAL).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::k8s::clusters::tests::{FakeKube, full_spec, kube_status, namespaced_cluster};
    use crate::k8s::crds::{ClusterPhase, ThoriumClusterStatus};

    /// The path of the test `ThoriumCluster`
    const PATH: &str = "/apis/sandia.gov/v1/namespaces/thorium/thoriumclusters/thorium";

    /// Run the delete against a fake with a short timeout and interval
    ///
    /// # Arguments
    ///
    /// * `fake` - The fake kube API to delete through
    async fn delete(fake: &FakeKube) -> ExitCode {
        // a timeout of a few intervals keeps the tests fast
        delete_and_wait(
            &fake.client(),
            "thorium",
            "thorium",
            Duration::from_millis(50),
            Duration::from_millis(10),
        )
        .await
    }

    /// Build the body the kube API answers a GET of a terminating `ThoriumCluster` with
    fn terminating() -> serde_json::Value {
        // a cluster whose operator is still cleaning up
        let mut cluster = namespaced_cluster(full_spec());
        cluster.status = Some(ThoriumClusterStatus {
            phase: Some(ClusterPhase::Ready),
            message: Some("cleaning up".to_owned()),
            ..ThoriumClusterStatus::default()
        });
        serde_json::to_value(cluster).expect("cluster serializes")
    }

    /// A cluster that is already gone succeeds without waiting
    #[tokio::test]
    async fn already_gone_succeeds() {
        // every request is answered as missing
        let fake = FakeKube::default();
        assert_eq!(delete(&fake).await, ExitCode::SUCCESS);
        // only the delete was sent
        assert_eq!(fake.requests().len(), 1);
        assert_eq!(fake.requests()[0].method, "DELETE");
    }

    /// A deleted cluster the operator releases succeeds once it is gone
    #[tokio::test]
    async fn deleted_then_gone_succeeds() {
        // the delete is accepted and the following GET finds nothing
        let fake = FakeKube::default().route("DELETE", PATH, 200, terminating());
        assert_eq!(delete(&fake).await, ExitCode::SUCCESS);
        // the delete was followed by a check
        let methods: Vec<_> = fake.requests().into_iter().map(|r| r.method).collect();
        assert_eq!(methods, ["DELETE", "GET"]);
    }

    /// A cluster the operator never releases fails after the timeout
    #[tokio::test]
    async fn never_released_times_out() {
        // the delete is accepted but the cluster stays
        let fake = FakeKube::default()
            .route("DELETE", PATH, 200, terminating())
            .route("GET", PATH, 200, terminating());
        assert_eq!(delete(&fake).await, ExitCode::FAILURE);
        // it checked more than once before giving up
        let gets = fake.requests().iter().filter(|r| r.method == "GET").count();
        assert!(gets > 1, "expected repeated checks, got {gets}");
    }

    /// A refused delete fails without waiting
    #[tokio::test]
    async fn refused_delete_fails() {
        // the delete is forbidden
        let fake = FakeKube::default().route("DELETE", PATH, 403, kube_status(403, "Forbidden"));
        assert_eq!(delete(&fake).await, ExitCode::FAILURE);
        // nothing was checked after the refusal
        assert_eq!(fake.requests().len(), 1);
    }

    /// A failing check is retried until the cluster is gone
    #[tokio::test]
    async fn failed_check_is_retried() {
        // the first check fails and the next finds nothing
        let mut checks = 0;
        let fake = FakeKube::default()
            .route("DELETE", PATH, 200, terminating())
            .with_handler(move |request| {
                // answer the first GET with a server error and leave the rest to the routes
                if request.method == "GET" {
                    checks += 1;
                    if checks == 1 {
                        return Some((500, kube_status(500, "InternalError")));
                    }
                }
                None
            });
        assert_eq!(delete(&fake).await, ExitCode::SUCCESS);
    }
}
