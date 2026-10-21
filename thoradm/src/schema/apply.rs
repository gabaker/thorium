//! `thoradm schema apply`: converges the live schema to the declared one

use crate::Error;
use crate::args::{ApplySchema, Args};
use crate::outcome::RunOutcome;

/// Apply the declared schema
///
/// # Arguments
///
/// * `cmd` - How to apply it
/// * `args` - The arguments thoradm was started with
#[expect(
    clippy::unused_async,
    reason = "the handler awaits its backends once the subcommand is built"
)]
pub(super) async fn handle(cmd: &ApplySchema, args: &Args) -> Result<RunOutcome, Error> {
    // nothing is read or changed until the subcommand is available
    let _ = (cmd, args);
    Err(super::not_built("apply"))
}
