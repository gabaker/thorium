//! Export privacy-safe ThorPal telemetry bundles from saved sessions.
//!
//! A bundle is one reviewable JSON document of counts, token totals,
//! histograms, enum tallies, timestamps, and hashes folded from the session
//! store -- never message text, tool arguments or results, forum prose, file
//! names, or paths. Level 0 is strictly content-free; level 1 opt-in adds
//! normalized failure signatures (short, defanged, number-scrubbed prefixes
//! of failing tool results). Nothing ships without review: the interactive
//! flow renders the exact bytes that will leave the machine and writes them
//! only after confirmation, and the headless `--yes` path applies the same
//! redaction before writing. Everything here is strictly read-only over the
//! sessions root, exactly like `thorctl analyze usage`.

pub mod bundle;
pub mod explore;
pub mod export;
pub mod fold;
pub mod help;
pub mod shell;
pub mod sig;

use thorium::Error;

use crate::args::Args;
use crate::args::ai::Telemetry;

/// Handle the telemetry commands
///
/// # Arguments
///
/// * `args` - The base Thorctl arguments
/// * `cmd` - The telemetry command to run
pub async fn handle(args: &Args, cmd: &Telemetry) -> Result<(), Error> {
    // route each telemetry verb to its handler
    match cmd {
        Telemetry::Export(export) => export::handle(args, export).await,
        Telemetry::Explore(explore) => explore::handle(args, explore).await,
    }
}
