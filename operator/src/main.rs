//! Operate a Thorium k8s cluster
mod app;
mod args;
mod k8s;
mod trace;
mod upgrades;

use clap::Parser;
use k8s::controller;
use std::process::ExitCode;
use std::time::Duration;
use tracing::{Level, event};

/// Run the operator subcommand given on the command line
#[tokio::main]
async fn main() -> ExitCode {
    // load command line args
    let args = args::Args::parse();
    // printing the CRD writes YAML to stdout, so it runs without logs mixed into it
    if let args::SubCommands::Crd(crd_args) = &args.cmd
        && !crd_args.apply
    {
        k8s::crds::print_crd();
        return ExitCode::SUCCESS;
    }
    // build the tracing config our log arguments describe
    let trace_conf = match trace::config(&args.trace) {
        Ok(trace_conf) => trace_conf,
        Err(error) => {
            // no logs are set up yet, so report a bad tracing config on stderr
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    // send our logs to stdout and any external collector the config names
    let provider = thorium::utils::trace::setup(trace::TRACE_NAME, &trace_conf);
    // execute the right handler
    let code = match args.cmd {
        // operate the ThoriumClusters in k8s
        args::SubCommands::Operate(operate_args) => {
            controller::run(operate_args).await;
            ExitCode::SUCCESS
        }
        // apply the ThoriumCluster CRD to the cluster
        args::SubCommands::Crd(_) => apply_crd().await,
        // delete a ThoriumCluster and wait for the operator to clean it up
        args::SubCommands::Uninstall(uninstall) => {
            let timeout = Duration::from_secs(uninstall.timeout);
            k8s::uninstall::run(&uninstall.namespace, &uninstall.name, timeout).await
        }
    };
    // flush any traces still waiting to be sent
    thorium::utils::trace::shutdown(provider);
    code
}

/// Apply the `ThoriumCluster` CRD to the cluster this runs in, failing if it can't be applied
async fn apply_crd() -> ExitCode {
    // set the crypto provider the kube client needs
    controller::install_crypto_provider();
    // connect to the cluster with the in-cluster or local kube config
    let client = match kube::Client::try_default().await {
        Ok(client) => client,
        Err(error) => {
            event!(
                Level::ERROR,
                msg = "Failed to connect to Kubernetes",
                error = app::helpers::error_chain(&error)
            );
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
            event!(
                Level::ERROR,
                msg = "Failed to apply the ThoriumCluster CRD",
                error = trace::describe(&error)
            );
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
