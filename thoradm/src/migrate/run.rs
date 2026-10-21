//! `thoradm migrate run`: runs a data migration

use crate::Error;
use crate::args::{Args, RunMigration};
use crate::outcome::RunOutcome;

/// Run a data migration, which reports how to perform it by hand until thoradm can run it
///
/// # Arguments
///
/// * `cmd` - The migration to run
/// * `args` - The arguments thoradm was started with
#[expect(
    clippy::unused_async,
    reason = "the handler awaits its backends once the subcommand is built"
)]
pub(super) async fn handle(cmd: &RunMigration, args: &Args) -> Result<RunOutcome, Error> {
    // nothing is read until the migration can run
    let _ = args;
    // find the migration and report its manual procedure, since none can run yet
    super::run_or_verify(&cmd.target, "run")
}
