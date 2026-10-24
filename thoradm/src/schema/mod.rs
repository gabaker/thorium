//! Prints, hashes, applies, and checks Thorium's declared schema

use thorium::Conf;

use crate::Error;
use crate::args::{Args, SchemaSubCommands};
use crate::outcome::RunOutcome;

mod apply;
mod diff;
mod drop_legacy;
mod hash;
mod legacy;
mod print;
mod recreate_view;
mod status;
mod verify_views;

/// Load the cluster's config, as a usage error when it can't be read
///
/// # Arguments
///
/// * `args` - The arguments thoradm was started with
pub(crate) fn load_conf(args: &Args) -> Result<Conf, Error> {
    // a config that can't be loaded is the caller's to fix, never worth retrying
    Conf::new(&args.cluster_conf).map_err(|err| {
        Error::Usage(format!(
            "the config at {} can't be loaded: {err}",
            args.cluster_conf.display()
        ))
    })
}

/// Handle a schema subcommand
///
/// # Arguments
///
/// * `cmd` - The schema subcommand to handle
/// * `args` - The arguments thoradm was started with
pub async fn handle(cmd: &SchemaSubCommands, args: &Args) -> Result<RunOutcome, Error> {
    // dispatch to the subcommand's handler
    match cmd {
        SchemaSubCommands::Print(print_cmd) => print::handle(print_cmd, args).await,
        SchemaSubCommands::Hash(hash_cmd) => hash::handle(hash_cmd, args).await,
        SchemaSubCommands::Apply(apply_cmd) => apply::handle(apply_cmd, args).await,
        SchemaSubCommands::Diff(diff_cmd) => diff::handle(diff_cmd, args).await,
        SchemaSubCommands::Status(status_cmd) => status::handle(status_cmd, args).await,
        SchemaSubCommands::VerifyViews(views_cmd) => verify_views::handle(views_cmd, args).await,
        SchemaSubCommands::Legacy(legacy_cmd) => legacy::handle(legacy_cmd, args).await,
        SchemaSubCommands::DropLegacy(drop_cmd) => drop_legacy::handle(drop_cmd, args).await,
    }
}
