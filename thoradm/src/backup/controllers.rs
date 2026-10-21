//! Controls the backup and restore workers for Thorium

pub mod backup;
pub mod coverage;
pub mod preflight;
pub mod restore;
pub mod scrub;
pub mod utils;

use crate::Error;
use crate::args::{Args, BackupSubCommands};
use crate::outcome::RunOutcome;

/// Spawn the correct backup specific controller
///
/// # Arguments
///
/// * `sub` - The backup subcommand to run
/// * `args` - The Thoradm args
pub async fn handle(sub: &BackupSubCommands, args: &Args) -> Result<RunOutcome, Error> {
    match sub {
        BackupSubCommands::New(take_args) => Box::pin(backup::handle(take_args, args)).await,
        BackupSubCommands::Scrub(scrub_args) => scrub::handle(scrub_args, args.workers).await,
        BackupSubCommands::Verify(verify_args) => {
            crate::backup::verify::handle(verify_args, args).await
        }
        BackupSubCommands::Restore(restore_args) => restore::handle(restore_args, args).await,
        BackupSubCommands::Coverage(coverage_args) => coverage::handle(coverage_args),
    }
}
