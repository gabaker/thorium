//! `thoradm schema hash`: prints the declared fingerprints

use crate::Error;
use crate::args::{Args, HashSchema};
use crate::outcome::RunOutcome;

/// Print the declared fingerprints
///
/// # Arguments
///
/// * `cmd` - Which backend to print the fingerprint of
/// * `args` - The arguments thoradm was started with
#[expect(
    clippy::unused_async,
    reason = "the handler awaits its backends once the subcommand is built"
)]
pub(super) async fn handle(cmd: &HashSchema, args: &Args) -> Result<RunOutcome, Error> {
    // nothing is read or changed until the subcommand is available
    let _ = (cmd, args);
    Err(super::not_built("hash"))
}
