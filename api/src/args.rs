use clap::Parser;

/// The command line args passed to the Thorum API
#[derive(Parser, Debug)]
#[clap(version, author)]
pub struct Args {
    /// The path to load the config file from
    #[clap(short, long, default_value = "thorium.yml")]
    pub config: String,
    /// Put the api in benchmarking mode (doesn't do anything yet)
    #[clap(long)]
    pub bench: bool,
    /// Apply the Scylla schema before starting (dev only; deployments run `thoradm schema apply`)
    #[clap(long)]
    pub apply_schema: bool,
}
