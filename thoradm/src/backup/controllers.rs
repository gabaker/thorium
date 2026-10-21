//! Controls the backup and restore workers for Thorium

mod backup;
mod restore;
mod scrub;
mod utils;

use crate::Error;
use crate::args::{Args, BackupSubCommands};

/// Spawn the correct backup specific controller
pub async fn handle(sub: &BackupSubCommands, args: &Args) -> Result<(), Error> {
    match sub {
        BackupSubCommands::New(take_args) => backup::handle(take_args, args).await,
        BackupSubCommands::Scrub(scrub_args) => scrub::handle(scrub_args, args.workers).await,
        BackupSubCommands::Restore(restore_args) => restore::handle(restore_args, args).await,
    }
}
