//! `thoradm migrate record`: records the deployment's new revision at the end of a plan

use crate::Error;
use crate::args::{Args, RecordRevision};
use crate::outcome::RunOutcome;

/// Record the revision a plan reached
///
/// # Arguments
///
/// * `cmd` - The revision to record
/// * `args` - The arguments thoradm was started with
#[expect(
    clippy::unused_async,
    reason = "the handler awaits its backends once the subcommand is built"
)]
pub(super) async fn handle(cmd: &RecordRevision, args: &Args) -> Result<RunOutcome, Error> {
    // nothing is recorded until the subcommand is available
    let _ = (cmd, args);
    Err(super::not_built("record"))
}
