//! `thoradm schema print`: prints the declared schema

use crate::Error;
use crate::args::{Args, PrintSchema};
use crate::outcome::RunOutcome;

/// Print the declared schema, as CQL or in canonical form
///
/// # Arguments
///
/// * `cmd` - Which backend to print and in what form
/// * `args` - The arguments thoradm was started with
#[expect(
    clippy::unused_async,
    reason = "the handler awaits its backends once the subcommand is built"
)]
pub(super) async fn handle(cmd: &PrintSchema, args: &Args) -> Result<RunOutcome, Error> {
    // nothing is read or changed until the subcommand is available
    let _ = (cmd, args);
    Err(super::not_built("print"))
}
