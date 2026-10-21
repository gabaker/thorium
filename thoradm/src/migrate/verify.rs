//! `thoradm migrate verify`: checks whether a data migration still needs to run

use crate::Error;
use crate::args::{Args, MigrationTarget};
use crate::outcome::RunOutcome;

/// Verify a data migration, which reports how to check it by hand until thoradm can verify it
///
/// # Arguments
///
/// * `cmd` - The migration to verify
/// * `args` - The arguments thoradm was started with
#[expect(
    clippy::unused_async,
    reason = "the handler awaits its backends once the subcommand is built"
)]
pub(super) async fn handle(cmd: &MigrationTarget, args: &Args) -> Result<RunOutcome, Error> {
    // nothing is read until the migration can be verified
    let _ = args;
    // find the migration and report its manual procedure, since none can be verified yet
    super::run_or_verify(cmd, "verify")
}
