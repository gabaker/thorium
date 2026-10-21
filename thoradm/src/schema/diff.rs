//! `thoradm schema diff`: compares the live schema with the recorded revision

use crate::Error;
use crate::args::{Args, DiffSchema};
use crate::outcome::RunOutcome;

/// Compare the live schema with the recorded revision's snapshot
///
/// # Arguments
///
/// * `cmd` - How to report the drift
/// * `args` - The arguments thoradm was started with
#[expect(
    clippy::unused_async,
    reason = "the handler awaits its backends once the subcommand is built"
)]
pub(super) async fn handle(cmd: &DiffSchema, args: &Args) -> Result<RunOutcome, Error> {
    // nothing is read or changed until the subcommand is available
    let _ = (cmd, args);
    Err(super::not_built("diff"))
}
