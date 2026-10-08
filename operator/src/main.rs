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
        // print the ThoriumCluster CRD
        args::SubCommands::Crd => k8s::crds::print_crd(),
    }
}
