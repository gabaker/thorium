//! `thoradm migrate reset-state`: rewrites the recorded revision after a data restore

use crate::Error;
use crate::args::{Args, ResetState};
use crate::outcome::RunOutcome;

/// Reset the recorded revision after a restore
///
/// # Arguments
///
/// * `cmd` - The revision the restored data is at
/// * `args` - The arguments thoradm was started with
#[expect(
    clippy::unused_async,
    reason = "the handler awaits its backends once the subcommand is built"
)]
pub(super) async fn handle(cmd: &ResetState, args: &Args) -> Result<RunOutcome, Error> {
    // nothing is recorded until the subcommand is available
    let _ = (cmd, args);
    Err(super::not_built("reset-state"))
}
