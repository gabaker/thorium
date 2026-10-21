//! `thoradm schema status`: reads the deployment's record for the operator

use crate::Error;
use crate::args::{Args, StatusSchema};
use crate::outcome::RunOutcome;

/// Print the deployment's record, a capped drift summary, and thoradm's catalog identity
///
/// # Arguments
///
/// * `cmd` - What to read and report
/// * `args` - The arguments thoradm was started with
#[expect(
    clippy::unused_async,
    reason = "the handler awaits its backends once the subcommand is built"
)]
pub(super) async fn handle(cmd: &StatusSchema, args: &Args) -> Result<RunOutcome, Error> {
    // nothing is read or changed until the subcommand is available
    let _ = (cmd, args);
    Err(super::not_built("status"))
}
