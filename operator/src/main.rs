//! Operate a Thorium k8s cluster
mod app;
mod args;
mod k8s;
mod upgrades;

use clap::Parser;
use k8s::controller;

/// Run the operator subcommand given on the command line
#[tokio::main]
async fn main() {
    // load command line args
    let args = args::Args::parse();
    // execute the right handler
    match args.cmd {
        // operate the ThoriumClusters in k8s
        args::SubCommands::Operate(operate_args) => controller::run(operate_args).await,
        // apply the ThoriumCluster CRD to the cluster
        args::SubCommands::Crd(crd_args) if crd_args.apply => apply_crd().await,
        // print the ThoriumCluster CRD
        args::SubCommands::Crd(_) => k8s::crds::print_crd(),
    }
}

/// Apply the `ThoriumCluster` CRD to the cluster this runs in, exiting non-zero on failure
async fn apply_crd() {
    // set the crypto provider the kube client needs
    controller::install_crypto_provider();
    // connect to the cluster with the in-cluster or local kube config
    let client = match kube::Client::try_default().await {
        Ok(client) => client,
        Err(error) => {
            eprintln!("Failed to connect to Kubernetes: {error}");
            std::process::exit(1);
        }
    };
    // apply the CRD and wait for it to be established
    if let Err(error) = k8s::crds::create_or_update(&client).await {
        eprintln!("Failed to apply the ThoriumCluster CRD: {error}");
        std::process::exit(1);
    }
}
