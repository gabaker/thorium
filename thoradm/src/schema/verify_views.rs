//! `thoradm schema verify-views`: compares declared views with their base tables

use crate::Error;
use crate::args::{Args, VerifyViews};
use crate::outcome::RunOutcome;

/// Compare declared materialized views with their base tables
///
/// # Arguments
///
/// * `cmd` - Which views to check and how many ranges
/// * `args` - The arguments thoradm was started with
#[expect(
    clippy::unused_async,
    reason = "the handler awaits its backends once the subcommand is built"
)]
pub(super) async fn handle(cmd: &VerifyViews, args: &Args) -> Result<RunOutcome, Error> {
    // nothing is read or changed until the subcommand is available
    let _ = (cmd, args);
    Err(super::not_built("verify-views"))
}
