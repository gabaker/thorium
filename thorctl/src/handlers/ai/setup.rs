//! The interactive ThorPal setup wizard
//!
//! `thorctl analyze setup` opens a wizard TUI that walks through logging into
//! Thorium (when no config exists yet), configuring an LLM provider with an
//! optional live connection test, detecting or downloading a Ghidra install,
//! probing for the full JDK Ghidra needs, and choosing the Ghidra projects
//! directory. Nothing touches the config file until the final review step is
//! confirmed, and the write is atomic (temp file plus rename) so a crash can
//! never truncate the only copy of a user's token.

mod app;
mod config;
mod ghidra;
mod jdk;
mod steps;

use std::io::IsTerminal;

use thorium::Error;

use crate::args::Args;
use crate::args::ai::Setup as SetupArgs;

use super::tui;
use app::SetupApp;

/// Run the interactive ThorPal setup wizard
///
/// # Arguments
///
/// * `args` - The base Thorctl arguments
/// * `cmd` - The parsed `setup` command
pub async fn handle(args: &Args, cmd: &SetupArgs) -> Result<(), Error> {
    // the wizard is interactive by nature: fail closed without a terminal
    // instead of dumping raw tui bytes into a pipe
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err(Error::new(
            "thorctl analyze setup is interactive and needs a terminal; configure \
            non-interactively with `thorctl login`, `thorctl config --ghidra-install \
            ...`, and by editing the `ai` block of your thorctl config instead",
        ));
    }
    // load the config best-effort: a missing or broken one enables the wizard's
    // login step rather than failing the command
    let (existing, config_error) = config::load_state(&args.config);
    // resolve the install root downloads land under: the flag, else the data dir
    let install_root = match &cmd.ghidra_dir {
        Some(dir) => dir.clone(),
        None => ghidra::default_install_root().ok_or_else(|| {
            Error::new(
                "could not determine the local data directory for Ghidra installs; \
                pass --ghidra-dir",
            )
        })?,
    };
    // resolve the shared projects default once, for prefills and the merge
    let default_projects =
        thorpal::default_ghidra_projects_dir().map_err(|err| Error::new(format!("{err:#}")))?;
    // route tracing to the shared log file before taking the screen; the TUI
    // owns the terminal and the wizard streams no tool logs
    tui::init_tracing_log(None);
    // take the screen only after everything that can fail has failed cleanly
    let mut terminal = tui::init_terminal()?;
    // build and run the app, capturing the result so teardown always runs
    let mut app = SetupApp::new(
        args.config.clone(),
        existing,
        config_error,
        install_root,
        default_projects,
        cmd,
    );
    let result = app.start(&mut terminal).await;
    // give the screen back before surfacing any error
    tui::restore_terminal();
    result?;
    Ok(())
}
