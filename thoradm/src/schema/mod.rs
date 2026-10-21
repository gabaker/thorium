//! Prints, hashes, applies, and checks Thorium's declared schema

use crate::Error;
use crate::args::{Args, SchemaSubCommands};
use crate::outcome::RunOutcome;

mod apply;
mod diff;
mod hash;
mod print;
mod status;
mod verify_views;

/// Report that a schema subcommand isn't built yet
///
/// # Arguments
///
/// * `command` - The subcommand, as typed after `thoradm schema`
fn not_built(command: &str) -> Error {
    // name the subcommand so scripts can tell it apart from a failure
    Error::NotImplemented(format!("thoradm schema {command} is not available yet"))
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
    }
}
