//! `thoradm migrate revert`: moves a cluster back to an older revision

use crate::Error;
use crate::args::{Args, RevertMigrations};
use crate::outcome::RunOutcome;

/// Revert a cluster to an older revision
///
/// # Arguments
///
/// * `cmd` - The revision to revert to
/// * `args` - The arguments thoradm was started with
#[expect(
    clippy::unused_async,
    reason = "the handler awaits its backends once the subcommand is built"
)]
pub(super) async fn handle(cmd: &RevertMigrations, args: &Args) -> Result<RunOutcome, Error> {
    // nothing is changed until the subcommand is available
    let _ = (cmd, args);
    Err(super::not_built("revert"))
}
