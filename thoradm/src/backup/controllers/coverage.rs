//! Prints the backup coverage registry without touching a config or a backend

use crate::Error;
use crate::args::CoverageBackup;
use crate::backup::coverage::Registry;
use crate::outcome::RunOutcome;
use std::io::Write;

/// Prints the coverage page, or its JSON listing, to stdout
///
/// # Arguments
///
/// * `args` - The coverage arguments
pub fn handle(args: &CoverageBackup) -> Result<RunOutcome, Error> {
    // render the same page the coverage_page example prints, or the JSON listing
    let output = if args.json {
        Registry::render_json()?
    } else {
        Registry::render_page()
    };
    // write it to stdout, treating a closed pipe as an error rather than a panic
    std::io::stdout()
        .lock()
        .write_all(output.as_bytes())
        .map_err(|error| Error::new(format!("Failed to write the coverage output: {error}")))?;
    Ok(RunOutcome::Complete)
}
