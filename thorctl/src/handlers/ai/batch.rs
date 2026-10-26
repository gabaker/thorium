//! Pooled headless evaluation runs and evaluation-figure generation
//!
//! `thorctl analyze batch` drives the whole evaluation loop that previously
//! lived in evaluator-side shell and python scripts: staging samples out of the
//! password-protected dataset zips, launching `thorctl analyze chat` sessions
//! through an adaptive rolling pool, recovering failed sessions off the
//! headless exit-code contract, and regenerating the docs' progression
//! figures from the Scoreboard table.

mod figures;
mod manifest;
mod pool;
// scan is visible to the ai module so the setup wizard reuses disk_free_gib
pub(super) mod scan;
mod stage;
mod state;
mod status;

use thorium::Error;

use crate::args::Args;
use crate::args::ai::Batch;

/// Handle the batched-evaluation commands
///
/// # Arguments
///
/// * `args` - The base Thorctl arguments
/// * `cmd` - The batch command to run
pub async fn handle(args: &Args, cmd: &Batch) -> Result<(), Error> {
    // handle the correct batch command
    match cmd {
        Batch::Run(cmd) => pool::handle(args, cmd).await,
        Batch::Status(cmd) => status::handle(args, cmd).await,
        Batch::Figures(cmd) => figures::handle(cmd),
    }
}
