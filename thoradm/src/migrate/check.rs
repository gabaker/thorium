//! `thoradm migrate check`: classifies what a migration would touch, writing nothing

use crate::Error;
use crate::args::{Args, CheckMigration};
use crate::outcome::RunOutcome;

/// Check what a migration would touch
///
/// # Arguments
///
/// * `cmd` - The migration to check
/// * `args` - The arguments thoradm was started with
#[expect(
    clippy::unused_async,
    reason = "the handler awaits its backends once the subcommand is built"
)]
pub(super) async fn handle(cmd: &CheckMigration, args: &Args) -> Result<RunOutcome, Error> {
    // nothing is read until the subcommand is available
    let _ = (cmd, args);
    Err(super::not_built("check"))
}
