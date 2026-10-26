//! Summarize a local binary with ThorPal
//!
//! The summary runs the same session lifecycle headless chats use (build,
//! seed, run, bounded finish) with progress on stderr and ONLY the final
//! executive summary on stdout, so scripted consumers can keep capturing
//! stdout alone. Provider pauses follow `--pause-wait` exactly like headless
//! chats: the fail-fast default surfaces the provider error (exit 1), an
//! opted-in wait parks with config re-probing and exits 75 on expiry.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use thorium::{CtlConf, Error};
use thorpal::Session;

use super::headless;
use super::pricing::CostModel;
use super::session;
use crate::args::ai::Summary;

/// Summarize a local binary with ThorPal's reverse-engineering agents
///
/// Drives a ThorPal [`Session`] over a local binary and prints the executive
/// summary it produces.
///
/// # Arguments
///
/// * `conf` - The Thorctl config, whose `ai` block supplies the ThorPal settings
/// * `cmd` - The parsed `summary` command
pub async fn handle(conf: &CtlConf, cmd: &Summary, config_path: &Path) -> Result<(), Error> {
    // build the ThorPal run config from our config and args; summaries always
    // run under an auto-generated session name (summary stays single-sample)
    let mut config = session::build_config(
        conf,
        &cmd.binary,
        &[],
        cmd.ghidra_install.as_ref(),
        cmd.workflow.as_ref(),
        None,
        cmd.fresh_project,
        cmd.anonymize,
        // summaries carry no deny-token or blind-family flags; --anonymize
        // alone still neutralizes library names
        Vec::new(),
        Vec::new(),
    )
    .await?;
    // let an auth pause pick up fresh credentials from the config file
    config.credential_refresher = Some(session::credential_refresher(config_path.to_owned()));
    // install the event sink feeding the plain-text progress printer, spawned
    // before the session builds so no early event is missed
    let (events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel();
    config.events = Some(events_tx);
    let printer = tokio::spawn(session::print_events(events_rx));
    // build the opening question from the binary the session actually analyzes
    // (an anonymized run swaps in a staged copy, so the original path from the
    // command line must never reach the prompt); an anonymized run names only
    // the bare sha so the prompt carries no host directory layout either
    let target = match config.anonymized {
        true => config
            .ghidra
            .binary
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unknown".to_owned()),
        false => config.ghidra.binary.display().to_string(),
    };
    let question = format!(
        "Write me a detailed executive summary of the binary {target}. What \
         capabilities does it have? Does it make network requests? Is there \
         anything concerning about it or anything I should know before running \
         it? Your report should be markdown formatted.",
    );
    // build the cost estimator before the session consumes the config;
    // build_config already proved the ai block resolves, so re-resolving
    // for the pricing overrides here is cheap and cannot fail anew
    let overrides = conf
        .ai
        .as_ref()
        .and_then(|ai| ai.resolve().ok())
        .and_then(|resolved| resolved.pricing());
    let cost = CostModel::for_provider(&config.provider, overrides.as_ref());
    // build the session over the config, mapping its anyhow error to a thorium one
    let session = Session::build(config)
        .await
        .map_err(|err| Error::new(format!("{err:#}")))?;
    // remember the model id for the usage summary before the session drops
    let model = session.model().to_string();
    // tell the operator how to resume this session once it exists
    let name = session.session_name().await;
    eprintln!("session: {name} (resume with `thorctl analyze chat --resume {name}`)");
    // spawn the pause supervisor: it probes on config edits and enforces the
    // --pause-wait cap, flagging an expiry so the exit path can return 75
    let pause_expired = Arc::new(AtomicBool::new(false));
    let supervisor = tokio::spawn(session::supervise_pauses(
        session.pause_controller(),
        config_path.to_owned(),
        cmd.pause_wait,
        Arc::clone(&pause_expired),
    ));
    // seed the forum with static detector findings before the agent loop; this
    // is also where the slow Ghidra import/analysis happens on a fresh project
    eprintln!("seeding analysis...");
    session.seed().await;
    // drive the whole agent loop over the executive-summary question
    let run = session.run(question).await;
    // the run is over either way, so stop babysitting pauses
    supervisor.abort();
    // always emit the summaries and shut Ghidra down even when the run failed,
    // so the analyzed database is flushed and the usage is reported; the name
    // is re-read inside in case the model renamed the session mid-run
    let name = headless::finish_and_flush(session, printer, cost.as_ref(), &model).await;
    // surface the run's outcome, mapping its anyhow error to a thorium Error
    match run {
        // print ONLY the reverse-engineering summary ThorPal produced on stdout
        Ok(outcome) => {
            // the --pause-wait promise: a pause that outlived its budget exits
            // 75 even when earlier iterations settled, matching headless
            if pause_expired.load(Ordering::Acquire) {
                eprintln!("a provider pause outlived --pause-wait during the run");
                eprintln!(
                    "fix the provider credentials, then resume with `thorctl analyze chat \
                     --resume {name} --continue` (or --prompt)"
                );
                std::process::exit(75);
            }
            println!("\n\n{}", outcome.response);
            Ok(())
        }
        Err(err) => {
            // an interrupted run (a pause given up on, or one that outlived
            // its budget) froze its state cleanly for resume: exit with the
            // conventional "temporary failure, try again later" code and a
            // resume hint, exactly like headless chat
            if pause_expired.load(Ordering::Acquire) || thorpal::is_interrupted_error(&err) {
                eprintln!("the run was interrupted by a provider pause before it could finish");
                eprintln!(
                    "fix the provider credentials, then resume with `thorctl analyze chat \
                     --resume {name} --continue` (or --prompt)"
                );
                std::process::exit(75);
            }
            Err(Error::new(format!("{err:#}")))
        }
    }
}
