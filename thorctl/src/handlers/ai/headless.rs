//! Run a ThorPal chat non-interactively (headless) over a local binary
//!
//! Headless mode drives the same self-terminating agent loop the TUI's
//! auto-report path uses ([`Session::run`]): seed the forum, run the loop to
//! completion from a single opening prompt (or continue a resumed session's
//! loop), optionally generate the report, and always finish the session so
//! Ghidra shuts down cleanly. Progress is streamed as plain text on stderr and
//! the final response lands on stdout, so scripted evaluation runs can be
//! batched and parsed.
//!
//! Exit codes: `0` on success (including a partial run that settled at least
//! one iteration), `1` on failure, `75` when a provider pause outlived an
//! opted-in `--pause-wait` budget (the finished session is resumable once the
//! provider is fixed), and `130` when an interrupting signal (SIGINT/SIGTERM)
//! stopped the run -- the first signal still finishes the session (snapshot +
//! Ghidra flush) so it stays resumable, and a second signal force-quits.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use thorium::{CtlConf, Error};
use thorpal::{Session, UsageTotals};

use super::chat;
use super::pricing::CostModel;
use super::session;
use super::session::{print_events, supervise_pauses};
use crate::args::ai::Chat;

/// Route tracing to the shared thorctl-ai log file
///
/// This mirrors the TUI's tracing setup minus its tool-log capture layer: that
/// layer lives in the global subscriber and would hold a clone of the event
/// channel's sender forever, keeping the progress printer alive after the
/// session drops. Headless runs keep stderr for progress lines, so tracing
/// (including ghidra-cli's analyzeHeadless output) lands only in
/// `~/.thorium/logs/thorctl-ai.log`. The file layer's filter honors `RUST_LOG`
/// and defaults to `info`. Installation is best-effort: an unresolvable log
/// file or an already-installed subscriber silently skips setup rather than
/// failing the run.
fn init_tracing_file() {
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    // resolve the log file under the user's .thorium home, best-effort
    let file = dirs::home_dir().and_then(|home| {
        let dir = home.join(".thorium").join("logs");
        // create the directory, skipping the file log when that fails
        std::fs::create_dir_all(&dir).ok()?;
        // open the log file in append mode so runs accumulate in one place
        std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(dir.join("thorctl-ai.log"))
            .ok()
    });
    // build the file layer when the log file resolved; honor an explicit
    // RUST_LOG for the file log only, defaulting to info-level everywhere
    let fmt_layer = file.map(|file| {
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
        tracing_subscriber::fmt::layer()
            .with_writer(std::sync::Mutex::new(file))
            .with_ansi(false)
            .with_filter(filter)
    });
    // install the file layer, ignoring an already-set global subscriber
    let _ = tracing_subscriber::registry().with(fmt_layer).try_init();
}

/// Read the headless run's opening prompt from its flag or prompt file
///
/// # Arguments
///
/// * `cmd` - The parsed `chat` command
async fn read_prompt(cmd: &Chat) -> Result<String, Error> {
    // take the inline prompt or read the prompt file off disk
    let prompt = match (&cmd.prompt, &cmd.prompt_file) {
        (Some(prompt), _) => prompt.clone(),
        (None, Some(path)) => tokio::fs::read_to_string(path).await.map_err(|err| {
            Error::new(format!(
                "failed to read the prompt file '{}': {err}",
                path.display()
            ))
        })?,
        // the dispatcher only routes here when one of the prompt flags was passed
        (None, None) => return Err(Error::new("headless mode needs --prompt or --prompt-file")),
    };
    // reject an empty prompt so the agent loop never starts on nothing
    if prompt.trim().is_empty() {
        return Err(Error::new("the opening prompt is empty"));
    }
    Ok(prompt)
}

/// Generate the session's report and write it to the path the operator chose
///
/// The template contents are read off disk when one resolved (the builtin
/// default is used otherwise) and [`Session::generate_report`] also saves the
/// session store's own `report.md` copy.
///
/// # Arguments
///
/// * `session` - The ThorPal session the report covers
/// * `path` - Where to write the generated report
/// * `template` - The resolved template path if one was set
/// * `defang` - Defang IOCs in the written copy (the session store's
///   `report.md` copy always stays pristine)
async fn write_report(
    session: &Session,
    path: &Path,
    template: Option<&PathBuf>,
    defang: bool,
) -> Result<(), Error> {
    // read the resolved template off disk when one was set
    let template = match template {
        Some(template_path) => Some(tokio::fs::read_to_string(template_path).await.map_err(
            |err| {
                Error::new(format!(
                    "failed to read report template '{}': {err}",
                    template_path.display()
                ))
            },
        )?),
        None => None,
    };
    // generate the report, which also saves the session store's report.md
    let report = session
        .generate_report(template.as_deref())
        .await
        .map_err(|err| Error::new(format!("failed to generate report: {err:#}")))?;
    // defang the written copy when asked; the store copy is already saved raw
    let report = if defang {
        super::defang::defang(&report)
    } else {
        report
    };
    // write the report to the path the operator chose
    tokio::fs::write(path, &report).await.map_err(|err| {
        Error::new(format!(
            "failed to write report to '{}': {err} (the report was still saved \
             in the session store as report.md)",
            path.display()
        ))
    })?;
    // tell the operator where both copies of the report landed
    eprintln!(
        "report written to {} (also saved in the session store as report.md)",
        path.display()
    );
    Ok(())
}

/// Print the run's token usage and estimated cost summary on stderr
///
/// Sub-agents report their usage separately from the main agent, so summing
/// the per-agent totals yields a true grand total. The cost line only prints
/// when the model resolved to a price (builtin table or config overrides) --
/// an unpriceable model still gets its token summary. Evaluation runs consume
/// these lines to track cost per analysis, so the format should stay stable.
///
/// # Arguments
///
/// * `usage` - The per-agent usage totals snapshot from the session
/// * `cost` - The resolved cost estimator for the run's model, if priceable
/// * `model` - The model id the run executed with
fn print_usage(usage: &BTreeMap<String, UsageTotals>, cost: Option<&CostModel>, model: &str) {
    // sum the per-agent tallies into one grand total for the run
    let mut total = UsageTotals::default();
    for agent_usage in usage.values() {
        total += *agent_usage;
    }
    // print the token summary in a stable, machine-consumable format
    eprintln!(
        "usage: input={} output={} cache_read={} cache_write={} total={} agents={}",
        total.input_tokens,
        total.output_tokens,
        total.cached_input_tokens,
        total.cache_creation_input_tokens,
        total.total_tokens,
        usage.len(),
    );
    // price the totals when the model resolved to a cost estimator
    if let Some(cost) = cost {
        let dollars = cost.cost(
            total.input_tokens,
            total.output_tokens,
            total.cached_input_tokens,
            total.cache_creation_input_tokens,
        );
        eprintln!("estimated cost: ${dollars:.4} ({model})");
    }
}

/// Finish a session with a bounded wait, report usage, and flush the printer
///
/// The shared tail of every headless exit path: the finish is bounded so a
/// wedged Ghidra bridge can never hold the exit hostage, the usage summary
/// prints even for a failed run, and dropping the session closes the event
/// channel so the progress printer drains before the process moves on.
/// Returns the session's final name (the model may have renamed it mid-run)
/// for the caller's resume hints.
///
/// # Arguments
///
/// * `session` - The session to finish and drop
/// * `printer` - The progress printer draining the session's event channel
/// * `cost` - The resolved cost estimator for the run's model, if priceable
/// * `model` - The model id the run executed with
pub(super) async fn finish_and_flush(
    session: Session,
    printer: tokio::task::JoinHandle<()>,
    cost: Option<&CostModel>,
    model: &str,
) -> String {
    // point the operator at the review queue when the run flagged anything
    let flags = session.forum().flags_snapshot().await.len();
    if flags > 0 {
        eprintln!(
            "[review] {flags} item(s) flagged for analyst review — see the report \
             appendix or resume the session to browse them"
        );
    }
    // finish with a bound so a wedged bridge cannot block the exit forever
    match tokio::time::timeout(std::time::Duration::from_secs(60), session.finish()).await {
        Ok(Ok(())) => (),
        Ok(Err(err)) => {
            tracing::warn!(?err, "failed to finish the session cleanly");
            eprintln!("warning: failed to finish the session cleanly: {err:#}");
        }
        Err(_) => eprintln!(
            "warning: timed out finishing the session; the Ghidra bridge may still \
             hold the project"
        ),
    }
    // report what the run consumed, even when it failed, so every analysis
    // (including a broken one) gets a token/cost accounting
    print_usage(&session.usage().await, cost, model);
    // re-read the name in case the model renamed the session mid-run
    let name = session.session_name().await;
    // drop the session so the printer's event channel closes and it drains
    drop(session);
    // give the printer a moment to flush its buffered progress lines
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), printer).await;
    name
}

/// Waits for the first interrupting signal (SIGINT or SIGTERM)
///
/// Resolves to the signal's name for the interrupt note. A failed signal
/// registration parks that branch forever, leaving the process's default
/// signal behavior in place rather than erroring the run.
async fn wait_for_interrupt() -> &'static str {
    // ctrl_c registration can fail; park instead of resolving spuriously
    let ctrl_c = async {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    {
        // an external kill sends SIGTERM; treat it like an interactive Ctrl-C
        let mut sigterm =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(sigterm) => sigterm,
                // registration failed: fall back to Ctrl-C alone
                Err(_) => {
                    ctrl_c.await;
                    return "interrupt";
                }
            };
        // resolve on whichever interrupting signal lands first
        tokio::select! {
            () = ctrl_c => "interrupt",
            _ = sigterm.recv() => "terminated",
        }
    }
    #[cfg(not(unix))]
    {
        // non-unix targets only have the Ctrl-C notion
        ctrl_c.await;
        "interrupt"
    }
}

/// Finish an interrupted session, report its usage, and exit with code 130
///
/// The first signal lands here: the in-flight run future has already been
/// dropped, the session is finished (bounded, so a wedged Ghidra bridge can
/// never hold the exit hostage), the usage summary still prints, and the
/// resume hint names how to continue. A second signal force-quits through the
/// watcher this spawns.
///
/// # Arguments
///
/// * `session` - The interrupted session to finish
/// * `signal` - The name of the signal that interrupted the run
/// * `cost` - The resolved cost estimator for the run's model, if priceable
/// * `model` - The model id the run executed with
async fn finish_interrupted(
    session: &Session,
    signal: &str,
    cost: Option<&CostModel>,
    model: &str,
) -> ! {
    // name the resumable session before any slow teardown work
    let name = session.session_name().await;
    eprintln!("{signal}: finishing session (snapshot + Ghidra flush); signal again to force-quit");
    // a second signal skips the teardown entirely
    tokio::spawn(async {
        wait_for_interrupt().await;
        std::process::exit(130);
    });
    // finish with a bound so a wedged bridge cannot block the exit forever
    match tokio::time::timeout(std::time::Duration::from_secs(60), session.finish()).await {
        Ok(Ok(())) => (),
        Ok(Err(err)) => eprintln!("warning: failed to finish the session cleanly: {err:#}"),
        Err(_) => eprintln!(
            "warning: timed out finishing the session; the Ghidra bridge may still \
             hold the project"
        ),
    }
    // report what the interrupted run consumed so partial work is accounted
    print_usage(&session.usage().await, cost, model);
    // say exactly how to pick the analysis back up
    eprintln!(
        "interrupted — resume with `thorctl analyze chat --resume {name}` (interactive), or \
         add --continue or --prompt for headless"
    );
    // 130 is the conventional killed-by-interrupt exit code
    std::process::exit(130);
}

/// Resume a finished session headless and regenerate just its report
///
/// Recovers a report lost to a provider fault at the very end of a run: the
/// saved session is restored (conversation, forum, and usage intact), the
/// report is regenerated over that state and written to `--report`, and the
/// session is finished again so everything shuts down cleanly. The agent loop
/// never runs, so this costs one completion round trip.
///
/// # Arguments
///
/// * `conf` - The Thorctl config, whose `ai` block supplies the ThorPal settings
/// * `cmd` - The parsed `chat` command
async fn report_only(conf: &CtlConf, cmd: &Chat, config_path: &Path) -> Result<(), Error> {
    // resolve the report template early so a typo fails before the session opens
    let template = session::resolve_report_template(conf, cmd.report_template.as_ref())?;
    // clap guarantees both of these are present via `requires` on --report-only
    let name = cmd
        .resume
        .as_ref()
        .ok_or_else(|| Error::new("--report-only needs --resume"))?;
    let path = cmd
        .report
        .as_ref()
        .ok_or_else(|| Error::new("--report-only needs --report"))?;
    // install the event sink feeding the plain-text progress printer so any
    // retry backoff during the regeneration stays visible
    let (events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel();
    let printer = tokio::spawn(print_events(events_rx));
    // reopen the saved session around its own store; the saved conversation
    // and usage are not needed here beyond what the session itself restores
    let (session, _, _, cost) =
        chat::resume_session(conf, cmd, name, events_tx, config_path).await?;
    // remember the model id for the usage summary before the session drops
    let model = session.model().to_string();
    // spawn the pause supervisor exactly like a full run does: without it a
    // provider pause during the regeneration parks the report request forever
    // (default --pause-wait none aborts the pause so the error surfaces)
    let pause_expired = Arc::new(AtomicBool::new(false));
    let supervisor = tokio::spawn(supervise_pauses(
        session.pause_controller(),
        config_path.to_owned(),
        cmd.pause_wait,
        Arc::clone(&pause_expired),
    ));
    // regenerate the report over the restored state and write it out
    let report = write_report(&session, path, template.as_ref(), cmd.defang).await;
    // the regeneration is over either way, so stop babysitting pauses
    supervisor.abort();
    // finish again (bounded), account the regeneration's usage, and let the
    // printer drain, so Ghidra and the store shut down cleanly either way
    let name = finish_and_flush(session, printer, cost.as_ref(), &model).await;
    // honor the --pause-wait promise: an expired pause exits 75 with a resume
    // hint so a scheduler retries instead of reading the failure as fatal
    if pause_expired.load(Ordering::Acquire) {
        if let Err(err) = &report {
            eprintln!("paused longer than --pause-wait: {err}");
        }
        eprintln!(
            "fix the provider credentials, then regenerate with `thorctl analyze chat \
             --resume {name} --report-only --report <path>`"
        );
        std::process::exit(75);
    }
    report
}

/// Run a ThorPal chat headless over a local binary and print its final response
///
/// Builds a ThorPal [`Session`] exactly like the TUI would, then drives the
/// whole lifecycle non-interactively: seed, run the agent loop to completion
/// from the opening prompt, generate the report when `--report` was passed,
/// and always finish so Ghidra shuts down and the analysis is flushed. A run
/// failure outranks a report failure; either exits nonzero through the normal
/// error path.
///
/// # Arguments
///
/// * `conf` - The Thorctl config, whose `ai` block supplies the ThorPal settings
/// * `cmd` - The parsed `chat` command
pub async fn handle(conf: &CtlConf, cmd: &Chat, config_path: &Path) -> Result<(), Error> {
    // route tracing to the shared log file before any session work starts
    init_tracing_file();
    // a report-only regeneration resumes a finished session instead of running
    if cmd.report_only {
        return report_only(conf, cmd, config_path).await;
    }
    // read the opening prompt before any heavier work runs; a --continue run
    // has none by construction (the loop reopens on the forum's questions)
    let prompt = if cmd.continue_run {
        None
    } else {
        Some(read_prompt(cmd).await?)
    };
    // resolve the report template early so a typo fails before the session
    // claims a store directory
    let template = session::resolve_report_template(conf, cmd.report_template.as_ref())?;
    // install the event sink feeding the plain-text progress printer
    let (events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel();
    // build a fresh session over the given binaries, or restore a saved one
    let (session, cost) = match &cmd.resume {
        // reopen the saved session around its own store with live credentials
        Some(name) => {
            let (session, _, _, cost) =
                chat::resume_session(conf, cmd, name, events_tx, config_path).await?;
            (session, cost)
        }
        // a fresh run builds its session from the binaries and config
        None => {
            // the first binary is the primary sample and the rest are siblings
            // imported alongside it (clap requires at least one sans --resume)
            let (binary, extras) = cmd
                .binary
                .split_first()
                .ok_or_else(|| Error::new("a binary path is required in headless mode"))?;
            // load the operator's library deny tokens before any heavier work
            let redact_tokens = session::load_redact_tokens(cmd.redact_tokens.as_ref()).await?;
            // build the ThorPal run config from our config and args
            let mut config = session::build_config(
                conf,
                binary,
                extras,
                cmd.ghidra_install.as_ref(),
                cmd.workflow.as_ref(),
                cmd.name.as_ref(),
                cmd.fresh_project,
                cmd.anonymize,
                redact_tokens,
                cmd.blind_family.clone(),
            )
            .await?;
            // raise the main loop's turn cap when the operator asked for one
            config.main_max_turns = cmd.max_turns;
            // honor the opt-out; seeding is on unless --disable-detectors
            config.seed_detectors = !cmd.disable_detectors;
            // honor the analyst-interaction opt-out too (headless asks never
            // wait either way; the gate removes the tools entirely)
            if cmd.no_analyst_asks {
                config.workflow.analyst_asks = false;
            }
            config.events = Some(events_tx);
            // let an auth pause pick up fresh credentials from the config file
            config.credential_refresher =
                Some(session::credential_refresher(config_path.to_owned()));
            // build the cost estimator before the session consumes the config;
            // build_config already proved the ai block resolves, so re-resolving
            // for the pricing overrides here is cheap and cannot fail anew
            let overrides = conf
                .ai
                .as_ref()
                .and_then(|ai| ai.resolve().ok())
                .and_then(|resolved| resolved.pricing());
            let cost = CostModel::for_provider(&config.provider, overrides.as_ref());
            // build the session before spawning the printer so errors print cleanly
            let session = Session::build(config)
                .await
                .map_err(|err| Error::new(format!("{err:#}")))?;
            (session, cost)
        }
    };
    // remember the model id for the usage summary (a resume keeps its saved one)
    let model = session.model().to_string();
    // tell the operator how to resume this session once it exists
    let name = session.session_name().await;
    eprintln!("session: {name} (resume with `thorctl analyze chat --resume {name}`)");
    // spawn the printer streaming run progress onto stderr
    let printer = tokio::spawn(print_events(events_rx));
    // spawn the pause supervisor: it probes on config edits and enforces the
    // --pause-wait cap, flagging an expiry so the exit path can return 75
    let pause_expired = Arc::new(AtomicBool::new(false));
    let supervisor = tokio::spawn(supervise_pauses(
        session.pause_controller(),
        config_path.to_owned(),
        cmd.pause_wait,
        Arc::clone(&pause_expired),
    ));
    // arm the one interrupt listener covering both the seed and the run: its
    // registered signal state persists across the two selects below, so a
    // signal landing between them is latched rather than lost (registration
    // happens on the listener's first poll, so a signal in the instant before
    // the seed select first polls it still takes the default disposition)
    let mut interrupt = Box::pin(wait_for_interrupt());
    // seed the forum with static detector findings before the agent loop; this
    // is also where the slow Ghidra import/analysis happens on a fresh project
    // (a resumed session restores with seeding off, making this a no-op)
    if cmd.resume.is_none() {
        eprintln!("seeding analysis...");
    }
    // race the seed against an interrupting signal too, so a Ctrl-C during the
    // (potentially long) Ghidra import still leaves a resumable session
    tokio::select! {
        // the first interrupting signal finishes the session and exits 130
        signal = &mut interrupt => {
            finish_interrupted(&session, signal, cost.as_ref(), &model).await
        }
        // the seed pass completed; fall through to the agent loop
        () = session.seed() => {}
    }
    // drive the agent loop, racing it against the same interrupt listener so a
    // Ctrl-C or an external kill still flushes Ghidra, prints the usage, and
    // leaves a resumable session behind instead of orphaning the bridge
    let run = tokio::select! {
        // the loop itself: a prompted run or a promptless --continue
        run = async {
            match &prompt {
                Some(prompt) => session.run(prompt.clone()).await,
                None => session.run_resumed().await,
            }
        } => run,
        // the first interrupting signal finishes the session and exits 130
        signal = &mut interrupt => {
            finish_interrupted(&session, signal, cost.as_ref(), &model).await
        }
    };
    // the run is over: teardown from here is non-interactive, so the next
    // signal force-quits instead of being swallowed; reuse the armed listener
    // so a signal landing right as the run settled is still caught
    tokio::spawn(async move {
        let signal = interrupt.await;
        eprintln!("{signal}: force-quitting during session teardown");
        std::process::exit(130);
    });
    // generate and write the report when the run succeeded and one was asked for
    let report = match (&run, &cmd.report) {
        (Ok(_), Some(path)) => write_report(&session, path, template.as_ref(), cmd.defang).await,
        _ => Ok(()),
    };
    // the run is over either way, so stop babysitting pauses
    supervisor.abort();
    // always emit the summaries and shut Ghidra down even when the run failed,
    // so the analyzed database is flushed and the usage is reported; the name
    // is re-read inside in case the model renamed the session mid-run
    let name = finish_and_flush(session, printer, cost.as_ref(), &model).await;
    // surface a run failure over a report failure, mapping to a thorium error
    let outcome = match run {
        Ok(outcome) => {
            // the --pause-wait promise: a pause that outlived its budget exits
            // 75 even when earlier iterations settled, so a scheduler retries
            // instead of reading the partial run as success; the report (when
            // asked for) was still written above
            if pause_expired.load(Ordering::Acquire) {
                // a report failure on this path would otherwise be swallowed
                if let Err(err) = &report {
                    eprintln!("warning: {err}");
                }
                eprintln!("a provider pause outlived --pause-wait during the run");
                eprintln!(
                    "fix the provider credentials, then resume with `thorctl analyze chat \
                     --resume {name} --continue` (or --prompt)"
                );
                std::process::exit(75);
            }
            outcome
        }
        Err(err) => {
            // an interrupted run (a pause given up on at --pause-wait none, or
            // one that outlived its budget) froze its state cleanly for
            // resume: exit with the conventional "temporary failure, try
            // again later" code and a resume hint so a scheduler retries
            // instead of reading it as fatal
            if pause_expired.load(Ordering::Acquire) || thorpal::is_interrupted_error(&err) {
                // the one-line summary beats the full anyhow chain here: the
                // marker-bearing causes repeat each other, and the pause
                // banner above already named the fault
                eprintln!("the run was interrupted by a provider pause before it could finish");
                eprintln!(
                    "fix the provider credentials, then resume with `thorctl analyze chat \
                     --resume {name} --continue` (or --prompt)"
                );
                std::process::exit(75);
            }
            return Err(Error::new(format!("{err:#}")));
        }
    };
    report?;
    // print the final response on stdout for scripts to consume
    println!("{}", outcome.response);
    Ok(())
}
