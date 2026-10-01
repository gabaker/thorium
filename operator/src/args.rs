/// The arguments for operating a Thorium cluster
use clap::Parser;

/// The arguments for the operator
#[derive(Parser, Debug, Clone)]
#[clap(version, author)]
pub struct Args {
    /// The sub command for to execute
    #[clap(subcommand)]
    pub cmd: SubCommands,
}

/// The sub commands for cluster operation
#[derive(Parser, Debug, Clone)]
pub enum SubCommands {
    /// Operate a Thorium k8s cluster
    Operate(OperateCluster),
    /// Print the `ThoriumCluster` CRD as YAML
    Crd,
}

/// Operate a thorium cluster arguments
#[derive(Parser, Debug, Clone)]
pub struct OperateCluster {
    /// Thorium URL when not running local to k8s
    #[clap(short, long)]
    pub url: Option<String>,
    /// Only watch `ThoriumCluster` resources in this namespace (all namespaces if unset)
    #[clap(long, env = "THORIUM_OPERATOR_NAMESPACE")]
    pub namespace: Option<String>,
}
