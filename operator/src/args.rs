//! The arguments for operating a Thorium cluster

use clap::Parser;

/// The arguments for the operator
#[derive(Parser, Debug, Clone)]
#[clap(version, author)]
pub struct Args {
    /// The sub command to execute
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The operate and crd subcommands parse with their flags
    #[test]
    fn args_parse() {
        // operate takes a url and a namespace
        let args = Args::try_parse_from([
            "thorium-operator",
            "operate",
            "--url",
            "http://localhost:8080",
            "--namespace",
            "thorium",
        ])
        .expect("operate should parse");
        let SubCommands::Operate(operate) = args.cmd else {
            panic!("expected the operate subcommand");
        };
        assert_eq!(operate.url.as_deref(), Some("http://localhost:8080"));
        assert_eq!(operate.namespace.as_deref(), Some("thorium"));
        // crd takes no arguments
        let args = Args::try_parse_from(["thorium-operator", "crd"]).expect("crd should parse");
        assert!(matches!(args.cmd, SubCommands::Crd));
        // a subcommand is required
        assert!(Args::try_parse_from(["thorium-operator"]).is_err());
    }
}
