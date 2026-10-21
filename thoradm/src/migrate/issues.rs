//! `thoradm migrate issues`: lists the issue groups a migration recorded

use crate::Error;
use crate::args::{Args, ListIssues};
use crate::outcome::RunOutcome;

/// List the issue groups a migration recorded, or one group's sample keys
///
/// # Arguments
///
/// * `cmd` - The migration and, optionally, the group to show
/// * `args` - The arguments thoradm was started with
#[expect(
    clippy::unused_async,
    reason = "the handler awaits its backends once the subcommand is built"
)]
pub(super) async fn handle(cmd: &ListIssues, args: &Args) -> Result<RunOutcome, Error> {
    // nothing is read until the subcommand is available
    let _ = (cmd, args);
    Err(super::not_built("issues"))
}
