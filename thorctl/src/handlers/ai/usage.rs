//! Global ThorPal usage and cost analytics across every saved session
//!
//! `thorctl analyze usage` opens a standalone, strictly read-only TUI over the
//! sessions root (`~/.thorium/sessions` by default): a background scanner
//! folds every session's `meta.json`, `session.json`, and `transcript.jsonl`
//! into compact per-session summaries, and seven views slice the resulting
//! aggregates -- an overview dashboard, a sortable sessions table, global
//! usage bars, a cross-session timeline, a day/hour heatmap, a daily trend,
//! and a per-model breakdown. Pressing Enter on a session drills into the
//! chat TUI's own Usage and Timeline views, rebuilt for that one session from
//! its transcript exactly as a resume would.
//!
//! Read-only means read-only: the scanner never takes [`SessionStore`] handles
//! (opening one creates directories), never writes, and tolerates every race a
//! live writer can produce -- atomic `rename` replacements, an appended torn
//! transcript tail, `*.tmp` siblings, and sessions renamed mid-scan.

mod app;
mod drill;
mod model;
mod scan;
mod views;

use thorium::{CtlConf, Error};
use thorpal::SessionStore;

use crate::args::Args;
use crate::args::ai::Usage as UsageArgs;

use super::tui;
use app::UsageApp;
use scan::PricingCtx;

/// Run the global usage stats TUI over every saved ThorPal session
///
/// # Arguments
///
/// * `args` - The base Thorctl arguments
/// * `cmd` - The parsed `usage` command
pub async fn handle(args: &Args, cmd: &UsageArgs) -> Result<(), Error> {
    // resolve the sessions root: the explicit flag outranks the default
    let root = match &cmd.sessions_root {
        Some(root) => root.clone(),
        None => SessionStore::default_root().map_err(|err| Error::new(format!("{err:#}")))?,
    };
    // pricing is best-effort, exactly like `batch status`: without a usable
    // config the cost columns thin out instead of failing the command
    let pricing = PricingCtx::from_conf(CtlConf::from_path(&args.config).ok());
    // route tracing to the shared log file before taking the screen; the TUI
    // owns the terminal and a read-only viewer streams no tool logs
    tui::init_tracing_log(None);
    // take the screen only after everything that can fail has failed cleanly
    let mut terminal = tui::init_terminal()?;
    // build and run the app, capturing the result so teardown always runs
    let mut app = UsageApp::new(root, pricing);
    let result = app.start(&mut terminal).await;
    // give the screen back before surfacing any error
    tui::restore_terminal();
    result?;
    Ok(())
}
