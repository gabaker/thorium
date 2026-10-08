//! Operate a Thorium k8s cluster
mod app;
mod args;
mod k8s;
mod upgrades;

use clap::Parser;
use k8s::controller;
use std::process::ExitCode;

/// Run the operator subcommand given on the command line
#[tokio::main]
async fn main() -> ExitCode {
    // load command line args
    let args = args::Args::parse();
    // execute the right handler
    match args.cmd {
        // operate the ThoriumClusters in k8s
        args::SubCommands::Operate(operate_args) => {
            controller::run(operate_args).await;
            ExitCode::SUCCESS
        }
        // apply the ThoriumCluster CRD to the cluster
        args::SubCommands::Crd(crd_args) if crd_args.apply => apply_crd().await,
        // print the ThoriumCluster CRD
        args::SubCommands::Crd(_) => {
            k8s::crds::print_crd();
            ExitCode::SUCCESS
        }
    }
}

/// Apply the `ThoriumCluster` CRD to the cluster this runs in, failing if it can't be applied
async fn apply_crd() -> ExitCode {
    // set the crypto provider the kube client needs
    controller::install_crypto_provider();
    // connect to the cluster with the in-cluster or local kube config
    let client = match kube::Client::try_default().await {
        Ok(client) => client,
        Err(error) => {
            eprintln!("Failed to connect to Kubernetes: {error}");
            return ExitCode::FAILURE;
        }
    };
    // apply the CRD with this client
    apply_crd_with(&client).await
}

/// Apply the `ThoriumCluster` CRD and wait for it to be established, failing if either fails
///
/// # Arguments
///
/// * `client` - The kube client for the cluster to apply the CRD in
async fn apply_crd_with(client: &kube::Client) -> ExitCode {
    // apply the CRD and wait for it to be established
    match k8s::crds::create_or_update(client).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // report why the CRD couldn't be applied
            eprintln!("Failed to apply the ThoriumCluster CRD: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::k8s::clusters::tests::FakeKube;

    /// A CRD that can't be applied fails the command so the chart's hook Job fails
    #[tokio::test]
    async fn apply_crd_fails_when_apply_fails() {
        // a fake kube API that answers the CRD apply as missing
        let fake = FakeKube::default();
        // the command fails
        assert_eq!(apply_crd_with(&fake.client()).await, ExitCode::FAILURE);
    }
}
