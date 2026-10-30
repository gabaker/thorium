//! The rolling evaluation pool: spawn, supervise, and recover chat sessions
//!
//! Reproduces the original evaluator pool as `thorctl analyze chat` child processes:
//! adaptive concurrency with delta-based throttle backoff, per-spawn
//! credential sourcing from an env file, a disk-free hold, and automated
//! recovery keyed off the headless exit-code contract (0 success, 1 failure,
//! 75 expired pause, 130 interrupted). Re-invoking the command continues a
//! run: completed units are skipped, failed ones retry on their remaining
//! budgets, orphaned live sessions are adopted, and units newly added to the
//! manifest are staged and launched.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use chrono::{SecondsFormat, Utc};
use thorium::{CtlConf, Error};
use tokio::process::{Child, Command};

use super::manifest::{BatchConf, SampleRef, UnitSpec};
use super::scan::{self, MarkerScanner};
use super::stage::{self, DatasetIndex};
use super::state::{Attempt, LaunchMode, RunLock, RunState, UnitState, UnitStatus};
use crate::args::Args;
use crate::args::ai::BatchRun;

/// How long the shutdown drain waits for signalled children to finish
///
/// Headless children bound their own session finish at 60s, so 90s covers the
/// finish plus the usage summary and process teardown.
const DRAIN_SECS: u64 = 90;

/// How long a disk-free hold defers launches before rechecking
const DISK_HOLD_SECS: u64 = 120;

/// How a session store relates to a unit's samples
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreOwnership {
    /// The store provably records this unit's samples
    Owned,
    /// The store provably records different samples
    Foreign,
    /// Ownership cannot be proven either way: the store's meta.json or the
    /// unit's primary sample is unreadable, and an anonymized store needs
    /// the sample's content hash to match (anonymize-at-rest records no
    /// operator path). Unverifiable stores are never wiped and never abort
    /// the run: resumes that need no source file proceed, and everything
    /// else skips with a restore hint instead of failing permanently.
    Unverifiable,
}

/// What a finished attempt looked like to the pool
#[derive(Debug, Clone)]
pub struct ExitObs {
    /// The launch mode the exited attempt ran under
    pub mode: LaunchMode,
    /// The exit code, or None when the child died to a signal
    pub exit: Option<i32>,
    /// Whether the session store exists on disk
    pub store_exists: bool,
    /// How the session store relates to this unit's samples
    pub ownership: StoreOwnership,
    /// Whether the session spent anything — a `usage:` line anywhere in the
    /// log across every attempt, OR a durable usage line in the store's
    /// transcript (the log summary only flushes at attempt end, so a killed
    /// process never prints one; the transcript survives the kill) — the
    /// stillborn/continue classifier
    pub usage_present: bool,
    /// Whether the ended attempt itself printed a `usage:` line (scanned
    /// from its launch offset) — the scoreable gate, so a stale line from an
    /// earlier attempt can never vouch for this one (an audit finding)
    pub attempt_usage_present: bool,
    /// Whether the report file exists with content
    pub report_nonempty: bool,
    /// Whether the pool is shutting down on an operator signal
    pub shutting_down: bool,
}

/// The retry budgets a disposition decision is made against
#[derive(Debug, Clone, Copy)]
pub struct Budget {
    /// How many fresh/continue attempts have run so far
    pub run_attempts: u32,
    /// How many report-only attempts have run so far
    pub report_attempts: u32,
    /// The manifest's fresh/continue retry budget
    pub max_retries: u32,
    /// The manifest's report-only attempt budget
    pub report_retries: u32,
}

/// What the pool does with a unit after one of its attempts ends
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Disposition {
    /// The unit produced a scoreable report
    Complete,
    /// Launch another attempt in the given mode
    Retry {
        /// The mode the next attempt launches in
        mode: LaunchMode,
        /// Whether to delete the stillborn session store first
        wipe_store: bool,
    },
    /// The operator interrupted the run; the unit resumes next invocation
    Interrupted,
    /// The unit is out of budget or options; the reason is kept on the unit
    Failed(String),
}

/// Decide what happens to a unit after an attempt ends
///
/// This is the pool's whole recovery policy in one pure function. A unit is
/// complete exactly when it is scoreable (a non-empty report plus the ended
/// attempt's own `usage:` line); a settled run missing only its report
/// retries report-only; every other failure retries fresh when the session
/// spent nothing across any attempt (wiping a stillborn store) and continues
/// the saved session when something was, until the budgets run out.
///
/// # Arguments
///
/// * `obs` - What the ended attempt looked like
/// * `budget` - The attempt counts and budgets to decide against
pub fn decide(obs: &ExitObs, budget: &Budget) -> Disposition {
    // a non-empty report plus the ended attempt's own usage line is
    // scoreable: the one success. Session-level usage is not enough here —
    // a stale line from an earlier attempt must never mark a later, failed
    // attempt complete (an audit finding); the report-only retry below
    // regenerates the report and prints fresh usage instead.
    if obs.report_nonempty && obs.attempt_usage_present {
        return Disposition::Complete;
    }
    // anything else ending during shutdown is the operator stopping the pool
    // (signalled children, adopted orphans, even already-failing exits), not
    // a failure: park the unit resumable and charge no budget
    if obs.shutting_down {
        return Disposition::Interrupted;
    }
    // a store that provably belongs to someone else is never touched
    if obs.store_exists && obs.ownership == StoreOwnership::Foreign {
        return Disposition::Failed(
            "a session store with this name exists but does not match this \
             unit's samples; change name_template or --run"
                .to_owned(),
        );
    }
    // a settled run missing only its report, and any failed report-only
    // attempt, retries the report alone while that budget lasts
    if obs.exit == Some(0) || obs.mode == LaunchMode::ReportOnly {
        // the session store must still exist for a report-only resume
        if obs.store_exists && budget.report_attempts < budget.report_retries {
            return Disposition::Retry {
                mode: LaunchMode::ReportOnly,
                wipe_store: false,
            };
        }
        // report regeneration keeps failing: an operator problem (the
        // content-filter class), not something more attempts will fix
        return Disposition::Failed(format!(
            "report missing after {} report-only attempts",
            budget.report_attempts
        ));
    }
    // everything else (1 failure, 75 expired pause, external 130, signal
    // death) retries while the run budget lasts
    if budget.run_attempts > budget.max_retries {
        return Disposition::Failed(format!(
            "exit {:?} after {} attempts",
            obs.exit, budget.run_attempts
        ));
    }
    // nothing saved on disk yet: start over from scratch
    if !obs.store_exists {
        return Disposition::Retry {
            mode: LaunchMode::Fresh,
            wipe_store: false,
        };
    }
    // a store that spent nothing is stillborn (the launch-401 class):
    // wipe it so the fresh relaunch can claim the session name again -- but
    // never wipe a store whose ownership could not be verified (an
    // anonymized store with its source sample unreadable): park it failed
    // with a restore hint so --retry-exhausted can pick it back up
    if !obs.usage_present {
        if obs.ownership == StoreOwnership::Unverifiable {
            return Disposition::Failed(
                "stillborn store cannot be wiped: ownership is unverifiable (the \
                 unit's primary sample or the store's meta.json is unreadable); \
                 restore the sample (or remove the store by hand), then pass \
                 --retry-exhausted"
                    .to_owned(),
            );
        }
        return Disposition::Retry {
            mode: LaunchMode::Fresh,
            wipe_store: true,
        };
    }
    // the session has real progress saved: continue it instead of paying for
    // the analysis again
    Disposition::Retry {
        mode: LaunchMode::Continue,
        wipe_store: false,
    }
}

/// One queued launch waiting for a pool slot
#[derive(Debug, Clone)]
struct QueueItem {
    /// The unit to launch
    unit: String,
    /// The mode to launch it in
    mode: LaunchMode,
    /// Whether to wipe the unit's stillborn store before launching
    wipe: bool,
}

/// The per-invocation re-arm offsets granted by `--retry-exhausted`
#[derive(Debug, Clone, Copy, Default)]
struct Rearm {
    /// How many prior run attempts are forgiven
    run: u32,
    /// How many prior report-only attempts are forgiven
    report: u32,
}

/// An append-only event log mirrored to stderr
#[derive(Debug)]
struct PoolLog {
    /// The pool.log path events append to
    path: PathBuf,
    /// Whether the stderr mirror is suppressed
    quiet: bool,
}

impl PoolLog {
    /// Log one pool event
    ///
    /// # Arguments
    ///
    /// * `msg` - The event to log
    fn log(&self, msg: &str) {
        // stamp the event so the log reads as a timeline
        let line = format!(
            "{} {msg}",
            Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
        );
        // the log is best effort: losing a line must never stop the pool
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            use std::io::Write;
            let _ = writeln!(file, "{line}");
        }
        // mirror the event for a watching operator
        if !self.quiet {
            eprintln!("{line}");
        }
    }
}

/// Everything the pool needs to launch and judge attempts
struct PoolCtx<'a> {
    /// The loaded batch manifest
    conf: &'a BatchConf,
    /// The run number being driven
    run: u32,
    /// The run directory holding state, logs, and reports
    run_dir: PathBuf,
    /// The sessions root child sessions live under
    sessions_root: PathBuf,
    /// The thorctl config path passed through to children
    config_path: PathBuf,
    /// The pool event log
    log: PoolLog,
    /// The per-invocation retry re-arms from --retry-exhausted
    rearms: HashMap<String, Rearm>,
}

impl PoolCtx<'_> {
    /// Build the report path for a session
    ///
    /// # Arguments
    ///
    /// * `session` - The session name to build the report path for
    fn report_path(&self, session: &str) -> PathBuf {
        self.run_dir.join(format!("{session}.md"))
    }

    /// Build the stdout log path for a session
    ///
    /// # Arguments
    ///
    /// * `session` - The session name to build the stdout path for
    fn out_path(&self, session: &str) -> PathBuf {
        self.run_dir.join(format!("{session}.out"))
    }

    /// Build the stderr log path for a session
    ///
    /// # Arguments
    ///
    /// * `session` - The session name to build the stderr path for
    fn err_path(&self, session: &str) -> PathBuf {
        self.run_dir.join(format!("{session}.err"))
    }

    /// Compute a unit's remaining-budget view, honoring any re-arm
    ///
    /// # Arguments
    ///
    /// * `name` - The unit's name
    /// * `unit` - The unit's state holding its attempt history
    fn budget_for(&self, name: &str, unit: &UnitState) -> Budget {
        // count the attempts in each budget class
        let run_attempts = unit
            .attempts
            .iter()
            .filter(|attempt| attempt.mode != LaunchMode::ReportOnly)
            .count() as u32;
        let report_attempts = unit.attempts.len() as u32 - run_attempts;
        // forgive the attempts a --retry-exhausted re-arm covered
        let rearm = self.rearms.get(name).copied().unwrap_or_default();
        Budget {
            run_attempts: run_attempts.saturating_sub(rearm.run),
            report_attempts: report_attempts.saturating_sub(rearm.report),
            max_retries: self.conf.max_retries,
            report_retries: self.conf.report_retries,
        }
    }

    /// Observe what an ended attempt left on disk
    ///
    /// # Arguments
    ///
    /// * `unit` - The unit's state
    /// * `mode` - The mode the ended attempt ran under
    /// * `exit` - The exit code the attempt ended with, if any
    /// * `shutting_down` - Whether the pool is shutting down
    fn observe(
        &self,
        unit: &UnitState,
        mode: LaunchMode,
        exit: Option<i32>,
        shutting_down: bool,
    ) -> ExitObs {
        // scoreability facts come from the run artifacts
        let report_nonempty = file_nonempty(&self.report_path(&unit.session));
        let err_path = self.err_path(&unit.session);
        let store = self.sessions_root.join(&unit.session);
        // session-level spend: any usage line across every attempt's bytes,
        // or a durable usage line in the store's transcript — the log summary
        // only flushes at attempt END, so a process killed mid-run prints
        // none and hours of real work would otherwise read as stillborn and
        // be wiped (this classifies stillborn-vs-continue, so paid stores
        // survive)
        let usage_present =
            scan::parse_usage_tail(&err_path).usage_lines > 0 || store_transcript_has_usage(&store);
        // attempt-level spend: usage lines at or past the ended attempt's
        // launch offset — a stale line from an earlier attempt must never
        // vouch for this one on the scoreable gate (an audit finding)
        let err_offset = unit
            .attempts
            .last()
            .map(|attempt| attempt.err_offset)
            .unwrap_or(0);
        let attempt_usage_present =
            scan::parse_usage_tail_from(&err_path, err_offset).usage_lines > 0;
        // store facts come from the sessions root
        let store_exists = store.exists();
        // an absent store leaves nothing to protect, so it counts as owned
        let ownership = if store_exists {
            store_ownership(&self.sessions_root, &unit.session, &unit.samples)
        } else {
            StoreOwnership::Owned
        };
        ExitObs {
            mode,
            exit,
            store_exists,
            ownership,
            usage_present,
            attempt_usage_present,
            report_nonempty,
            shutting_down,
        }
    }
}

/// Check whether a file exists with content
///
/// # Arguments
///
/// * `path` - The file to check
fn file_nonempty(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|meta| meta.len() > 0)
}

/// Check whether a session store's transcript records any model spend
///
/// The durable transcript gains a `usage` line per model request as the
/// session runs, so it survives a process kill — unlike the stderr usage
/// summary, which only flushes when an attempt ends. Reads line-wise with an
/// early exit so a long transcript costs one partial pass; a missing or
/// unreadable transcript is simply "no spend".
///
/// # Arguments
///
/// * `store` - The session store directory
fn store_transcript_has_usage(store: &Path) -> bool {
    use std::io::BufRead;
    // open the transcript, treating absence as no spend
    let Ok(file) = std::fs::File::open(store.join("transcript.jsonl")) else {
        return false;
    };
    // scan for the first usage line and stop there
    for line in std::io::BufReader::new(file).lines() {
        let Ok(line) = line else {
            return false;
        };
        if line.contains("\"kind\":\"usage\"") {
            return true;
        }
    }
    false
}

/// Judge how a session store relates to a unit's samples
///
/// The store's meta.json records the analyzed paths the session was built
/// from (`binary`, plus the same per extra sample). Staged samples are named
/// by their sha256, so matching the primary sample's file name against those
/// recorded paths is a strong ownership signal without depending on the
/// exact meta schema. An anonymized session built from an operator-named
/// sample records only the staged sha copy (the pre-staging path is never
/// persisted -- anonymize-at-rest), so when the file name misses, the
/// primary's content hash is matched against the recorded staged paths
/// instead. When neither proof is reachable -- the meta.json or the source
/// sample itself cannot be read -- the verdict is `Unverifiable`, NOT
/// foreign: a corpus file vanishing between invocations must park the unit
/// recoverably instead of condemning (or aborting over) a store the state
/// file says is ours.
///
/// # Arguments
///
/// * `sessions_root` - The sessions root the store lives under
/// * `session` - The session name to check
/// * `samples` - The unit's sample paths, primary first
fn store_ownership(sessions_root: &Path, session: &str, samples: &[PathBuf]) -> StoreOwnership {
    // the primary sample's file name is the strongest content-derived id
    let Some(primary) = samples.first() else {
        return StoreOwnership::Unverifiable;
    };
    let Some(name) = primary.file_name().and_then(|name| name.to_str()) else {
        return StoreOwnership::Unverifiable;
    };
    // an unreadable meta.json proves nothing either way
    let meta_path = sessions_root.join(session).join("meta.json");
    let Ok(meta) = std::fs::read_to_string(&meta_path) else {
        return StoreOwnership::Unverifiable;
    };
    // the recorded paths embed the staged file name when this unit built it
    if meta.contains(name) {
        return StoreOwnership::Owned;
    }
    // an anonymized session staged the sample under its sha256: hash the
    // unit's primary and match the digest against the recorded staged paths
    match file_sha256(primary) {
        Some(sha) if meta.to_ascii_lowercase().contains(&sha) => StoreOwnership::Owned,
        // a readable sample that matches nothing proves the store is foreign
        Some(_) => StoreOwnership::Foreign,
        // an unreadable sample proves nothing: never wipe, never abort
        None => StoreOwnership::Unverifiable,
    }
}

/// Compute a file's lowercase sha256 hex digest, streaming from disk
///
/// Returns `None` when the file cannot be read -- ownership checks treat an
/// unhashable sample as proving nothing rather than erroring.
///
/// # Arguments
///
/// * `path` - The file to hash
fn file_sha256(path: &Path) -> Option<String> {
    use sha2::{Digest, Sha256};
    // stream the file through the hasher so big samples stay cheap on memory
    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).ok()?;
    Some(format!("{:x}", hasher.finalize()))
}

/// Launch or continue a pooled evaluation run
///
/// # Arguments
///
/// * `args` - The base Thorctl arguments
/// * `cmd` - The run command to drive the pool with
pub async fn handle(args: &Args, cmd: &BatchRun) -> Result<(), Error> {
    // resolve the evaluation directory so every derived path is absolute
    let dir = std::fs::canonicalize(&cmd.dir).map_err(|err| {
        Error::new(format!(
            "failed to resolve evaluation dir {}: {err}",
            cmd.dir.display()
        ))
    })?;
    // load and validate the manifest before touching any state
    let conf = BatchConf::load(&dir)?;
    // fail before any staging or spawning if the thorctl config is unusable,
    // since every child session will load it
    let ctl = CtlConf::from_path(&args.config)?;
    if ctl.ai.is_none() {
        return Err(Error::new(format!(
            "{} has no ai block; child sessions could never start",
            args.config.display()
        )));
    }
    let config_path = std::fs::canonicalize(&args.config).map_err(|err| {
        Error::new(format!(
            "failed to resolve {}: {err}",
            args.config.display()
        ))
    })?;
    // resolve the run number to create or continue
    let latest = super::state::latest_run(&dir)?;
    let run = match (cmd.run, cmd.new_run) {
        (Some(run), _) => run,
        (None, true) => latest.map_or(1, |latest| latest + 1),
        (None, false) => latest.unwrap_or(1),
    };
    let run_dir = super::state::run_dir(&dir, run);
    // a dry run only reads, so it neither creates the run dir nor locks it
    let _lock = if cmd.dry_run {
        None
    } else {
        std::fs::create_dir_all(&run_dir)
            .map_err(|err| Error::new(format!("failed to create {}: {err}", run_dir.display())))?;
        Some(RunLock::acquire(&run_dir)?)
    };
    // load the run's state or start a fresh one
    let mut state = RunState::load(&run_dir)?.unwrap_or_else(|| RunState::new(run));
    // build the pool context everything downstream works from
    let sessions_root = thorpal::SessionStore::default_root()
        .map_err(|err| Error::new(format!("failed to resolve the sessions root: {err}")))?;
    let mut ctx = PoolCtx {
        conf: &conf,
        run,
        run_dir: run_dir.clone(),
        sessions_root,
        config_path,
        log: PoolLog {
            path: run_dir.join("pool.log"),
            quiet: args.quiet,
        },
        rearms: HashMap::new(),
    };
    // reconcile the manifest against the run's state and the filesystem
    let plan = reconcile(&mut ctx, &mut state, cmd)?;
    // a dry run prints the plan and stops before staging or spawning
    if cmd.dry_run {
        print_plan(&plan);
        return Ok(());
    }
    // stage every dataset sample the queued units still need
    stage_needed(&ctx, &mut state, &plan)?;
    // persist the reconciled state before the first spawn
    state.save(&run_dir)?;
    write_aliases(&run_dir, &state)?;
    // drive the pool to completion
    run_pool(&mut ctx, &mut state, cmd, plan).await
}

/// What reconciliation decided to do with one unit
#[derive(Debug, Clone)]
struct PlanRow {
    /// The unit's name
    unit: String,
    /// The unit's alias
    alias: String,
    /// The unit's session name
    session: String,
    /// What happens to the unit this invocation
    action: String,
    /// How many samples the unit analyzes
    members: usize,
}

/// The reconciled work this invocation will perform
#[derive(Debug, Default)]
struct Plan {
    /// The launches waiting for pool slots, in order
    queue: VecDeque<QueueItem>,
    /// Live sessions from a prior invocation to adopt, as (unit, pid)
    adopted: Vec<(String, u32)>,
    /// The dataset shas each queued unit still needs staged
    staging: Vec<(String, Vec<String>)>,
    /// One row per selected unit for the dry-run plan and the logs
    rows: Vec<PlanRow>,
    /// Units skipped with a warning this invocation
    skipped: Vec<String>,
}

/// Reconcile the manifest against the run state and the filesystem
///
/// New units are registered (with staged-path resolution and blind alias
/// assignment), finished units are skipped, failed and interrupted units are
/// re-queued on their remaining budgets, live orphans are adopted, and a
/// launched unit whose manifest samples changed is a hard error.
///
/// # Arguments
///
/// * `ctx` - The pool context
/// * `state` - The run state to reconcile into
/// * `cmd` - The run command flags
fn reconcile(ctx: &mut PoolCtx, state: &mut RunState, cmd: &BatchRun) -> Result<Plan, Error> {
    // narrow to the selected units, rejecting unknown names loudly
    let selected = select_units(ctx.conf, cmd)?;
    let mut plan = Plan::default();
    // split the selected units into known ones and brand-new ones
    let mut new_units = Vec::new();
    for unit in &selected {
        if state.units.contains_key(&unit.name) {
            reconcile_existing(ctx, state, unit, cmd.retry_exhausted, &mut plan)?;
        } else {
            new_units.push((*unit).clone());
        }
    }
    // register the new units: resolve samples, assign aliases, claim names
    register_new_units(ctx, state, new_units, &mut plan)?;
    // warn about state units the manifest no longer lists
    for name in state.units.keys() {
        if !ctx.conf.units.iter().any(|unit| unit.name == *name) {
            ctx.log.log(&format!(
                "warning: unit {name} is in state but not the manifest"
            ));
        }
    }
    Ok(plan)
}

/// Narrow the manifest's units to the ones this invocation acts on
///
/// # Arguments
///
/// * `conf` - The loaded manifest
/// * `cmd` - The run command flags
fn select_units<'a>(conf: &'a BatchConf, cmd: &BatchRun) -> Result<Vec<&'a UnitSpec>, Error> {
    // no filter selects every unit
    if cmd.units.is_empty() {
        return Ok(conf.units.iter().collect());
    }
    // every requested name must exist so typos cannot silently no-op
    let mut selected = Vec::with_capacity(cmd.units.len());
    for name in &cmd.units {
        match conf.units.iter().find(|unit| unit.name == *name) {
            Some(unit) => selected.push(unit),
            None => {
                return Err(Error::new(format!(
                    "--units names '{name}' but the manifest has no such unit"
                )));
            }
        }
    }
    Ok(selected)
}

/// Reconcile one unit that already has run state
///
/// # Arguments
///
/// * `ctx` - The pool context
/// * `state` - The run state holding the unit
/// * `spec` - The unit's manifest spec
/// * `retry_exhausted` - Whether failed units get their budgets re-armed
/// * `plan` - The plan being assembled
fn reconcile_existing(
    ctx: &mut PoolCtx,
    state: &mut RunState,
    spec: &UnitSpec,
    retry_exhausted: bool,
    plan: &mut Plan,
) -> Result<(), Error> {
    // a launched unit's sample set is immutable: changing it must be loud
    let resolved = resolve_sample_paths(ctx, spec);
    let unit = state.units.get_mut(&spec.name).expect("unit exists");
    if resolved != unit.samples {
        return Err(Error::new(format!(
            "unit '{}' already launched with a different sample set; a \
             session's samples are fixed at creation, so add a new unit instead",
            spec.name
        )));
    }
    // build this unit's plan row up front
    let mut row = PlanRow {
        unit: spec.name.clone(),
        alias: unit.alias.clone(),
        session: unit.session.clone(),
        action: String::new(),
        members: unit.samples.len(),
    };
    match unit.status {
        // a finished unit only re-queues if its report vanished
        UnitStatus::Complete => {
            if file_nonempty(&ctx.report_path(&unit.session)) {
                row.action = "skip (complete)".to_owned();
            } else {
                row.action = "report-only (report file lost)".to_owned();
                plan.queue.push_back(QueueItem {
                    unit: spec.name.clone(),
                    mode: LaunchMode::ReportOnly,
                    wipe: false,
                });
            }
        }
        // a failed unit retries only once its budget is re-armed
        UnitStatus::Failed => {
            if rearm(ctx, &spec.name, unit, retry_exhausted) {
                row.action = match infer_mode(ctx, unit)? {
                    Inferred::Launch { mode, wipe } => {
                        if queue_launch(ctx, plan, &spec.name, unit, mode, wipe) {
                            unit.status = UnitStatus::Pending;
                            unit.fail_reason = None;
                            format!("retry ({mode:?}, re-armed)")
                        } else {
                            "skipped (missing sample)".to_owned()
                        }
                    }
                    // an unverifiable store parks the unit, not the run
                    Inferred::Skip { reason } => {
                        plan.skipped.push(spec.name.clone());
                        reason
                    }
                };
            } else {
                row.action = format!(
                    "leave failed ({}); pass --retry-exhausted to retry",
                    unit.fail_reason.as_deref().unwrap_or("unknown")
                );
            }
        }
        // an interrupted or pending unit resumes from its artifacts
        UnitStatus::Interrupted | UnitStatus::Pending => {
            if recover_complete(ctx, unit) {
                // the artifacts are already scoreable: only the state was stale
                row.action = "complete (recovered from artifacts)".to_owned();
            } else {
                row.action = match infer_mode(ctx, unit)? {
                    Inferred::Launch { mode, wipe } => {
                        if queue_launch(ctx, plan, &spec.name, unit, mode, wipe) {
                            format!("launch ({mode:?})")
                        } else {
                            "skipped (missing sample)".to_owned()
                        }
                    }
                    // an unverifiable store parks the unit, not the run
                    Inferred::Skip { reason } => {
                        plan.skipped.push(spec.name.clone());
                        reason
                    }
                };
            }
        }
        // a running unit is either a live orphan to adopt or a stale crash
        UnitStatus::Running => match unit.pid.filter(|pid| pid_runs_session(*pid, &unit.session)) {
            Some(pid) => {
                row.action = format!("adopt live pid {pid}");
                plan.adopted.push((spec.name.clone(), pid));
            }
            None if recover_complete(ctx, unit) => {
                // the crashed invocation actually finished the unit first
                row.action = "complete (recovered from artifacts)".to_owned();
            }
            None => {
                unit.status = UnitStatus::Pending;
                unit.pid = None;
                row.action = match infer_mode(ctx, unit)? {
                    Inferred::Launch { mode, wipe } => {
                        if queue_launch(ctx, plan, &spec.name, unit, mode, wipe) {
                            format!("relaunch after crash ({mode:?})")
                        } else {
                            "skipped (missing sample)".to_owned()
                        }
                    }
                    // an unverifiable store parks the unit, not the run
                    Inferred::Skip { reason } => {
                        plan.skipped.push(spec.name.clone());
                        reason
                    }
                };
            }
        },
    }
    plan.rows.push(row);
    Ok(())
}

/// Queue a unit's launch, re-staging any dataset samples that vanished
///
/// A fresh launch needs every sample back on disk: missing files whose names
/// are dataset sha256s are queued for re-staging, while any other missing
/// sample skips the unit with a warning (the original pool's behavior). Resumed
/// modes run off the saved session, so they queue as they are.
///
/// # Arguments
///
/// * `ctx` - The pool context
/// * `plan` - The plan the launch and any staging joins
/// * `name` - The unit's name
/// * `unit` - The unit's state holding its resolved sample paths
/// * `mode` - The mode the launch runs in
/// * `wipe` - Whether the launch wipes a stillborn store first
fn queue_launch(
    ctx: &PoolCtx,
    plan: &mut Plan,
    name: &str,
    unit: &UnitState,
    mode: LaunchMode,
    wipe: bool,
) -> bool {
    // only a fresh launch reads the sample files themselves
    if mode == LaunchMode::Fresh {
        let mut shas = Vec::new();
        for path in &unit.samples {
            // samples still on disk need nothing
            if path.is_file() {
                continue;
            }
            // a content-addressed name can be re-staged from the dataset
            let sha = path
                .file_name()
                .and_then(|name| name.to_str())
                .filter(|name| name.len() == 64 && name.chars().all(|c| c.is_ascii_hexdigit()));
            match (sha, &ctx.conf.dataset) {
                (Some(sha), Some(_)) => shas.push(sha.to_owned()),
                // anything else is unrecoverable without the operator
                _ => {
                    ctx.log.log(&format!(
                        "warning: unit {name} skipped: missing sample {}",
                        path.display()
                    ));
                    plan.skipped.push(name.to_owned());
                    return false;
                }
            }
        }
        // queue the vanished shas for the staging pass
        if !shas.is_empty() {
            plan.staging.push((name.to_owned(), shas));
        }
    }
    plan.queue.push_back(QueueItem {
        unit: name.to_owned(),
        mode,
        wipe,
    });
    true
}

/// Re-arm a failed unit's budgets when --retry-exhausted asks for it
///
/// # Arguments
///
/// * `ctx` - The pool context recording the re-arms
/// * `name` - The unit's name
/// * `unit` - The unit's state holding its attempt history
/// * `retry_exhausted` - Whether this invocation asked for re-arms
fn rearm(ctx: &mut PoolCtx, name: &str, unit: &UnitState, retry_exhausted: bool) -> bool {
    // without the flag a failed unit stays failed
    if !retry_exhausted {
        return false;
    }
    // forgive every attempt so far, in both budget classes
    let run = unit
        .attempts
        .iter()
        .filter(|attempt| attempt.mode != LaunchMode::ReportOnly)
        .count() as u32;
    let report = unit.attempts.len() as u32 - run;
    ctx.rearms.insert(name.to_owned(), Rearm { run, report });
    true
}

/// Mark a unit complete when its artifacts are already scoreable
///
/// A crash or interrupt can land between a session finishing and the state
/// write; the report and usage line on disk are the truth, so the unit is
/// completed in place instead of paying for another launch.
///
/// # Arguments
///
/// * `ctx` - The pool context
/// * `unit` - The unit's state to recover
fn recover_complete(ctx: &PoolCtx, unit: &mut UnitState) -> bool {
    // scoreable means a non-empty report plus a usage tail recorded by the
    // LAST attempt — the same attempt-scoped gate the exit observer applies,
    // so a stale line from an earlier attempt cannot vouch through the
    // crash-recovery door either (an audit finding); a spent-but-unsettled
    // unit falls through to infer_mode, which resumes it for its report
    let err_offset = unit
        .attempts
        .last()
        .map(|attempt| attempt.err_offset)
        .unwrap_or(0);
    let tail = scan::parse_usage_tail_from(&ctx.err_path(&unit.session), err_offset);
    if !file_nonempty(&ctx.report_path(&unit.session)) || tail.usage_lines == 0 {
        return false;
    }
    // close any attempt the crash left open
    if let Some(attempt) = unit.attempts.last_mut()
        && attempt.ended.is_none()
    {
        attempt.ended = Some(Utc::now());
    }
    // adopt the artifacts as the unit's final accounting
    unit.status = UnitStatus::Complete;
    unit.scoreable = true;
    unit.usage = tail.usage;
    unit.cost = tail.cost;
    unit.pid = None;
    unit.fail_reason = None;
    true
}

/// What reconciliation inferred for one unit's next launch
#[derive(Debug, Clone, PartialEq, Eq)]
enum Inferred {
    /// Queue a launch in the given mode, optionally wiping the store first
    Launch {
        /// The mode the next attempt launches in
        mode: LaunchMode,
        /// Whether to delete the stillborn session store first
        wipe: bool,
    },
    /// Skip the unit this invocation, with the operator-facing reason
    Skip {
        /// Why the unit was skipped and how to recover it
        reason: String,
    },
}

/// Infer how to relaunch a unit from what its artifacts show
///
/// A provably foreign store is a loud error (a name collision the operator
/// must resolve). An UNVERIFIABLE store -- the source sample vanished, which
/// anonymize-at-rest makes the only proof for operator-named samples -- must
/// not abort the whole invocation: resumes that never read the source
/// proceed, and only a would-be wipe skips the unit with a restore hint.
///
/// # Arguments
///
/// * `ctx` - The pool context
/// * `unit` - The unit's state
fn infer_mode(ctx: &PoolCtx, unit: &UnitState) -> Result<Inferred, Error> {
    // observe the artifacts with no exit code to go on
    let obs = ctx.observe(unit, LaunchMode::Fresh, None, false);
    // a store that provably belongs to someone else is a loud stop
    if obs.store_exists && obs.ownership == StoreOwnership::Foreign {
        return Err(Error::new(format!(
            "session store '{}' exists but does not match unit samples; \
             change name_template or --run",
            unit.session
        )));
    }
    // a scoreable session only needs its report regenerated at most; neither
    // resume reads the source sample, so unverifiable ownership is fine here
    if obs.usage_present && obs.store_exists {
        if obs.report_nonempty {
            // complete sessions are filtered out before this is called, so a
            // scoreable store with a report just resumes for its report path
            return Ok(Inferred::Launch {
                mode: LaunchMode::ReportOnly,
                wipe: false,
            });
        }
        return Ok(Inferred::Launch {
            mode: LaunchMode::Continue,
            wipe: false,
        });
    }
    // a stillborn store is wiped for the fresh relaunch -- unless its
    // ownership could not be verified, in which case the unit skips with a
    // restore hint rather than risking someone else's data
    if obs.store_exists && obs.ownership == StoreOwnership::Unverifiable {
        return Ok(Inferred::Skip {
            reason: "skipped (store ownership unverifiable: restore the unit's \
                     sample file, or remove the store by hand)"
                .to_owned(),
        });
    }
    Ok(Inferred::Launch {
        mode: LaunchMode::Fresh,
        wipe: obs.store_exists,
    })
}

/// Check whether a pid is alive and running the given session
///
/// # Arguments
///
/// * `pid` - The pid to check
/// * `session` - The session name its command line must mention
#[cfg(unix)]
fn pid_runs_session(pid: u32, session: &str) -> bool {
    // the kernel's cmdline file joins argv with NUL bytes
    let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    let cmdline = String::from_utf8_lossy(&raw);
    // the session name only appears on our own chat invocations
    cmdline.contains(session)
}

/// Check whether a pid is alive and running the given session
///
/// # Arguments
///
/// * `pid` - The pid to check
/// * `session` - The session name its command line must mention
#[cfg(not(unix))]
fn pid_runs_session(_pid: u32, _session: &str) -> bool {
    // without procfs a stale pid cannot be verified, so never adopt
    false
}

/// Register brand-new manifest units into the run state
///
/// # Arguments
///
/// * `ctx` - The pool context
/// * `state` - The run state to register into
/// * `new_units` - The manifest units with no state yet
/// * `plan` - The plan being assembled
fn register_new_units(
    ctx: &mut PoolCtx,
    state: &mut RunState,
    new_units: Vec<UnitSpec>,
    plan: &mut Plan,
) -> Result<(), Error> {
    // the next free alias index continues past every alias already assigned
    let mut next_index = state
        .units
        .values()
        .filter_map(|unit| unit.alias.strip_prefix('s'))
        .filter_map(|digits| digits.parse::<usize>().ok())
        .max()
        .unwrap_or(0);
    // blind runs shuffle the new units so aliases leak no manifest order
    let ordered = if ctx.conf.blind {
        stage::shuffled(new_units)
    } else {
        new_units
    };
    for spec in ordered {
        // drop units whose on-disk samples are missing, like the original pool
        let missing: Vec<&PathBuf> = spec
            .samples
            .iter()
            .filter_map(|sample| match sample {
                SampleRef::Path(path) if !path.is_file() => Some(path),
                _ => None,
            })
            .collect();
        if let Some(path) = missing.first() {
            ctx.log.log(&format!(
                "warning: unit {} skipped: missing sample {}",
                spec.name,
                path.display()
            ));
            plan.skipped.push(spec.name.clone());
            continue;
        }
        // blind units get the next shuffled alias; others keep their name
        let alias = if ctx.conf.blind {
            stage::alias_for(next_index)
        } else {
            spec.name.clone()
        };
        next_index += 1;
        // render and claim the session name before anything launches
        let session = ctx.conf.render_name(ctx.run, &alias)?;
        if ctx.sessions_root.join(&session).exists() {
            return Err(Error::new(format!(
                "session '{session}' already exists under {}; change \
                 name_template or --run so batch never reuses foreign sessions",
                ctx.sessions_root.display()
            )));
        }
        // collect the dataset shas this unit still needs staged
        let needed: Vec<String> = spec
            .samples
            .iter()
            .filter_map(|sample| match sample {
                SampleRef::Sha256(sha) if !ctx.conf.dir.join("samples").join(sha).is_file() => {
                    Some(sha.clone())
                }
                _ => None,
            })
            .collect();
        if !needed.is_empty() {
            plan.staging.push((spec.name.clone(), needed));
        }
        // record the unit and queue its first launch
        let samples = resolve_sample_paths(ctx, &spec);
        plan.rows.push(PlanRow {
            unit: spec.name.clone(),
            alias: alias.clone(),
            session: session.clone(),
            action: "launch (Fresh)".to_owned(),
            members: samples.len(),
        });
        state.units.insert(
            spec.name.clone(),
            UnitState {
                session,
                alias,
                samples,
                status: UnitStatus::Pending,
                pid: None,
                attempts: Vec::new(),
                scoreable: false,
                usage: None,
                cost: None,
                fail_reason: None,
            },
        );
        plan.queue.push_back(QueueItem {
            unit: spec.name,
            mode: LaunchMode::Fresh,
            wipe: false,
        });
    }
    Ok(())
}

/// Resolve a unit's sample refs to the absolute paths a session analyzes
///
/// # Arguments
///
/// * `ctx` - The pool context
/// * `spec` - The unit's manifest spec
fn resolve_sample_paths(ctx: &PoolCtx, spec: &UnitSpec) -> Vec<PathBuf> {
    // sha refs live in the content-addressed samples dir; paths stand as is
    spec.samples
        .iter()
        .map(|sample| match sample {
            SampleRef::Sha256(sha) => ctx.conf.dir.join("samples").join(sha),
            SampleRef::Path(path) => path.clone(),
        })
        .collect()
}

/// Print the reconciled plan without acting on it
///
/// # Arguments
///
/// * `plan` - The plan to print
fn print_plan(plan: &Plan) {
    // one aligned row per selected unit
    println!("unit                 alias   session               members  action");
    for row in &plan.rows {
        println!(
            "{:<20} {:<7} {:<21} {:<8} {}",
            row.unit, row.alias, row.session, row.members, row.action
        );
    }
    // summarize the side work the real run would do
    let staged: usize = plan.staging.iter().map(|(_, shas)| shas.len()).sum();
    println!(
        "\n{} queued, {} adopted, {} skipped, {} samples to stage",
        plan.queue.len(),
        plan.adopted.len(),
        plan.skipped.len(),
        staged
    );
}

/// Stage every dataset sample the plan's units still need
///
/// # Arguments
///
/// * `ctx` - The pool context
/// * `state` - The run state (for logging context only)
/// * `plan` - The plan listing the needed shas per unit
fn stage_needed(ctx: &PoolCtx, state: &mut RunState, plan: &Plan) -> Result<(), Error> {
    // nothing to stage is the common continuation case
    if plan.staging.is_empty() {
        return Ok(());
    }
    // staging requires the manifest's dataset block (validated at load)
    let Some(dataset) = &ctx.conf.dataset else {
        return Err(Error::new(
            "units need staging but the manifest has no dataset block".to_owned(),
        ));
    };
    // load the dataset index and refresh it when any sha is unknown
    let mut index = DatasetIndex::load(&ctx.conf.dir)?;
    let unknown = plan
        .staging
        .iter()
        .flat_map(|(_, shas)| shas.iter())
        .any(|sha| index.lookup(sha).is_none());
    if unknown {
        ctx.log
            .log("indexing dataset zips for unresolved sha256 refs");
        let reindexed = index.refresh(dataset)?;
        index.save(&ctx.conf.dir)?;
        ctx.log.log(&format!(
            "dataset index refreshed: {reindexed} zips (re)indexed"
        ));
    }
    // stage each needed sha, dropping units whose shas stay unresolved
    for (unit, shas) in &plan.staging {
        for sha in shas {
            if index.lookup(sha).is_none() {
                // the unit cannot run without its sample: mark it failed loudly
                ctx.log.log(&format!(
                    "warning: unit {unit} skipped: sha {sha} is not in the dataset index"
                ));
                if let Some(unit_state) = state.units.get_mut(unit) {
                    unit_state.status = UnitStatus::Failed;
                    unit_state.fail_reason = Some(format!("sha {sha} not found in the dataset"));
                }
                break;
            }
            // extraction verifies the bytes against the requested sha
            stage::stage_sample(&ctx.conf.dir, dataset, &index, sha)?;
            ctx.log.log(&format!("staged {sha} for {unit}"));
        }
    }
    Ok(())
}

/// Rewrite the run's alias map for the evaluator
///
/// # Arguments
///
/// * `run_dir` - The run directory the map lives in
/// * `state` - The run state holding the alias assignments
fn write_aliases(run_dir: &Path, state: &RunState) -> Result<(), Error> {
    // one tab-separated row per unit, sorted by alias for easy scanning
    let mut rows: Vec<String> = state
        .units
        .iter()
        .map(|(name, unit)| format!("{}\t{name}\t{}", unit.alias, unit.session))
        .collect();
    rows.sort();
    let body = format!("{}\n", rows.join("\n"));
    let path = run_dir.join("aliases.tsv");
    std::fs::write(&path, body)
        .map_err(|err| Error::new(format!("failed to write {}: {err}", path.display())))
}

/// Drive the pool loop to completion
///
/// # Arguments
///
/// * `ctx` - The pool context
/// * `state` - The reconciled run state
/// * `cmd` - The run command flags
/// * `plan` - The reconciled work to perform
async fn run_pool(
    ctx: &mut PoolCtx<'_>,
    state: &mut RunState,
    cmd: &BatchRun,
    mut plan: Plan,
) -> Result<(), Error> {
    // the pool's live children, keyed by unit name
    let mut children: HashMap<String, Child> = HashMap::new();
    // adopted orphan pids polled for liveness, keyed by unit name
    let mut adopted: HashMap<String, u32> = plan.adopted.drain(..).collect();
    // the adaptive concurrency starts at the manifest's start bound; the cap
    // never drops below one or nothing could ever launch
    let cap = cmd
        .max_concurrency
        .unwrap_or(ctx.conf.concurrency.max)
        .clamp(1, ctx.conf.concurrency.max);
    let floor = ctx.conf.concurrency.floor.min(cap);
    let mut conc = ctx.conf.concurrency.start.clamp(floor, cap);
    // seed the throttle scanner so backoff reacts only to new markers
    let mut scanner = MarkerScanner::new();
    let mut prev_markers = scanner.seed(&ctx.run_dir)?;
    // launch pacing and the shutdown drain are tracked as instants
    let mut next_launch = Instant::now();
    let mut drain_deadline: Option<Instant> = None;
    let mut shutting_down = false;
    let mut seconds = 0u64;
    // one heartbeat drives launches, exits, markers, and adoption polling
    let mut heartbeat = tokio::time::interval(Duration::from_secs(1));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // register the signal streams once so no signal lands unheard
    let mut signals = SignalStreams::new();
    ctx.log.log(&format!(
        "pool start: run {} with {} queued, {} adopted, conc {conc} (floor {floor}, cap {cap})",
        ctx.run,
        plan.queue.len(),
        adopted.len()
    ));
    loop {
        // wait for the next heartbeat or an operator signal
        tokio::select! {
            _ = heartbeat.tick() => seconds += 1,
            _ = signals.recv() => {
                // the second signal abandons the drain and kills everything
                if shutting_down {
                    ctx.log.log("second signal: force-killing live sessions");
                    force_kill(ctx, state, &mut children, &run_cleanup_ids(&adopted)).await;
                    state.save(&ctx.run_dir)?;
                    epilogue(ctx, state);
                    std::process::exit(130);
                }
                // the first signal stops launches and drains gracefully
                shutting_down = true;
                drain_deadline = Some(Instant::now() + Duration::from_secs(DRAIN_SECS));
                plan.queue.clear();
                ctx.log.log("signal received: draining live sessions (sessions finish in <=60s)");
                for (unit, child) in &children {
                    if let Some(pid) = child.id() {
                        ctx.log.log(&format!("terminating {unit} (pid {pid})"));
                        signal_pid(pid, true);
                    }
                }
                // adopted orphans drain on the same graceful signal
                for (unit, pid) in &adopted {
                    ctx.log.log(&format!("terminating adopted {unit} (pid {pid})"));
                    signal_pid(*pid, true);
                }
            }
        }
        // reap any children that have exited, capturing each exit code once
        let mut exited: Vec<(String, Option<i32>)> = Vec::new();
        children.retain(|unit, child| match child.try_wait() {
            // the child exited: take its code and drop the handle
            Ok(Some(status)) => {
                exited.push((unit.clone(), status.code()));
                false
            }
            // the child is still running
            Ok(None) => true,
            // a wait error means the child is gone with no code to read
            Err(_) => {
                exited.push((unit.clone(), None));
                false
            }
        });
        for (unit, exit) in exited {
            handle_exit(ctx, state, &mut plan.queue, &unit, exit, shutting_down)?;
        }
        // poll adopted orphans and judge them off their artifacts once gone
        let gone: Vec<String> = adopted
            .iter()
            .filter(|(unit, pid)| match state.units.get(*unit) {
                // the orphan ended once its pid stops running the session
                Some(unit_state) => !pid_runs_session(**pid, &unit_state.session),
                // a unit missing from state cannot be tracked at all
                None => true,
            })
            .map(|(unit, _)| unit.clone())
            .collect();
        for unit in gone {
            adopted.remove(&unit);
            handle_exit(ctx, state, &mut plan.queue, &unit, None, shutting_down)?;
        }
        // adjust concurrency off the throttle-marker delta each poll window
        if !shutting_down && seconds.is_multiple_of(ctx.conf.poll_secs.max(1)) && seconds > 0 {
            let markers = scanner.scan(&ctx.run_dir)?;
            state.markers_seen = markers;
            if markers > prev_markers {
                prev_markers = markers;
                if conc > floor {
                    conc -= 1;
                }
                ctx.log
                    .log(&format!("backoff: markers now {markers}, conc {conc}"));
            } else if seconds.is_multiple_of(ctx.conf.poll_secs.max(1) * 2) && conc < cap {
                conc += 1;
                ctx.log.log(&format!("recover: conc {conc}"));
            }
        }
        // launch while there is capacity, pacing by stagger and disk space
        while !shutting_down
            && children.len() + adopted.len() < conc
            && !plan.queue.is_empty()
            && Instant::now() >= next_launch
        {
            // hold launches while the disk is too full to run safely
            let free = scan::disk_free_gib(&ctx.conf.disk_path)?;
            if free < ctx.conf.disk_free_min_gb {
                ctx.log.log(&format!(
                    "disk-hold: {free} GiB free under {} GiB floor; holding {DISK_HOLD_SECS}s",
                    ctx.conf.disk_free_min_gb
                ));
                next_launch = Instant::now() + Duration::from_secs(DISK_HOLD_SECS);
                break;
            }
            // launch the next queued item
            let item = plan.queue.pop_front().expect("queue is non-empty");
            // a unit that failed during staging never launches
            if state
                .units
                .get(&item.unit)
                .is_none_or(|unit| unit.status == UnitStatus::Failed)
            {
                ctx.log
                    .log(&format!("skipping {}: failed before launch", item.unit));
                continue;
            }
            match spawn_attempt(ctx, state, &item).await {
                Ok(child) => {
                    children.insert(item.unit.clone(), child);
                    next_launch = Instant::now() + Duration::from_secs(ctx.conf.stagger_secs);
                }
                Err(err) => {
                    // a spawn failure is an operator problem: fail the unit
                    ctx.log
                        .log(&format!("spawn failed for {}: {err}", item.unit));
                    if let Some(unit) = state.units.get_mut(&item.unit) {
                        unit.status = UnitStatus::Failed;
                        unit.fail_reason = Some(format!("spawn failed: {err}"));
                    }
                    state.save(&ctx.run_dir)?;
                }
            }
        }
        // a drain that outlives its deadline force-kills the stragglers
        if let Some(deadline) = drain_deadline
            && Instant::now() >= deadline
            && (!children.is_empty() || !adopted.is_empty())
        {
            ctx.log
                .log("drain deadline passed: force-killing stragglers");
            force_kill(ctx, state, &mut children, &run_cleanup_ids(&adopted)).await;
            adopted.clear();
        }
        // the pool is done when nothing is live and nothing is queued
        if children.is_empty() && adopted.is_empty() && (plan.queue.is_empty() || shutting_down) {
            break;
        }
    }
    // persist the final state and summarize the run
    state.save(&ctx.run_dir)?;
    write_aliases(&ctx.run_dir, state)?;
    epilogue(ctx, state);
    // an interrupted pool exits 130 like its children
    if shutting_down {
        std::process::exit(130);
    }
    // the run only succeeds when every unit it acted on completed; units
    // skipped before registration count as incomplete too
    let incomplete: Vec<String> = plan
        .rows
        .iter()
        .filter(|row| {
            state
                .units
                .get(&row.unit)
                .is_none_or(|unit| unit.status != UnitStatus::Complete)
        })
        .map(|row| row.unit.clone())
        .chain(plan.skipped.iter().map(|name| format!("{name} (skipped)")))
        .collect();
    if incomplete.is_empty() {
        Ok(())
    } else {
        Err(Error::new(format!(
            "{} unit(s) did not complete: {}",
            incomplete.len(),
            incomplete.join(", ")
        )))
    }
}

/// Collect the pids to clean up from the adopted map
///
/// # Arguments
///
/// * `adopted` - The adopted unit-to-pid map
fn run_cleanup_ids(adopted: &HashMap<String, u32>) -> Vec<u32> {
    adopted.values().copied().collect()
}

/// Force-kill every live child and adopted pid
///
/// # Arguments
///
/// * `ctx` - The pool context
/// * `state` - The run state to mark interrupted units in
/// * `children` - The live children to kill
/// * `adopted_pids` - The adopted pids to kill
async fn force_kill(
    ctx: &PoolCtx<'_>,
    state: &mut RunState,
    children: &mut HashMap<String, Child>,
    adopted_pids: &[u32],
) {
    // kill our own children through their handles
    for (unit, child) in children.drain() {
        // start_kill is SIGKILL; the graceful path already ran
        let mut child = child;
        let _ = child.start_kill();
        let _ = child.wait().await;
        mark_interrupted(state, &unit);
        ctx.log.log(&format!("killed {unit}"));
    }
    // adopted orphans are not our children, so signal them by pid
    for pid in adopted_pids {
        signal_pid(*pid, false);
    }
    // any adopted unit still marked running is now interrupted
    let running: Vec<String> = state
        .units
        .iter()
        .filter(|(_, unit)| unit.status == UnitStatus::Running)
        .map(|(name, _)| name.clone())
        .collect();
    for unit in running {
        mark_interrupted(state, &unit);
    }
}

/// Mark a unit interrupted and close its open attempt
///
/// # Arguments
///
/// * `state` - The run state holding the unit
/// * `unit` - The unit to mark
fn mark_interrupted(state: &mut RunState, unit: &str) {
    if let Some(unit) = state.units.get_mut(unit) {
        // close the open attempt with no exit code
        if let Some(attempt) = unit.attempts.last_mut()
            && attempt.ended.is_none()
        {
            attempt.ended = Some(Utc::now());
        }
        unit.status = UnitStatus::Interrupted;
        unit.pid = None;
    }
}

/// Send a pid a termination or kill signal
///
/// # Arguments
///
/// * `pid` - The pid to signal
/// * `graceful` - Whether to request a graceful finish (SIGTERM) or kill
#[cfg(unix)]
fn signal_pid(pid: u32, graceful: bool) {
    // children finish their sessions on SIGTERM and exit 130
    let signal = if graceful {
        libc::SIGTERM
    } else {
        libc::SIGKILL
    };
    // a dead pid is fine: the exit path will observe it
    unsafe {
        libc::kill(pid as i32, signal);
    }
}

/// Send a pid a termination or kill signal
///
/// # Arguments
///
/// * `pid` - The pid to signal
/// * `graceful` - Whether to request a graceful finish or kill
#[cfg(not(unix))]
fn signal_pid(_pid: u32, _graceful: bool) {
    // without unix signals only owned children can be killed, via start_kill
}

/// The operator signal streams the pool listens on
///
/// The streams are registered once, before the pool loop, so a signal landing
/// between polls is queued instead of missed.
struct SignalStreams {
    /// The SIGINT stream, when it could be registered
    #[cfg(unix)]
    interrupt: Option<tokio::signal::unix::Signal>,
    /// The SIGTERM stream, when it could be registered
    #[cfg(unix)]
    terminate: Option<tokio::signal::unix::Signal>,
}

impl SignalStreams {
    /// Register the pool's signal streams
    fn new() -> SignalStreams {
        // unix listens for both SIGINT and SIGTERM
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            SignalStreams {
                interrupt: signal(SignalKind::interrupt()).ok(),
                terminate: signal(SignalKind::terminate()).ok(),
            }
        }
        // other platforms only have ctrl-c
        #[cfg(not(unix))]
        SignalStreams {}
    }

    /// Wait for the next operator interrupt or terminate signal
    ///
    /// A closed stream (which practically cannot happen for these signals) is
    /// treated as a shutdown request so the pool drains instead of hanging.
    async fn recv(&mut self) {
        #[cfg(unix)]
        match (&mut self.interrupt, &mut self.terminate) {
            // both streams registered: whichever fires first wins
            (Some(interrupt), Some(terminate)) => {
                tokio::select! {
                    _ = interrupt.recv() => (),
                    _ = terminate.recv() => (),
                }
            }
            // half-registered fallbacks still drain on the stream that exists
            (Some(interrupt), None) => {
                interrupt.recv().await;
            }
            (None, Some(terminate)) => {
                terminate.recv().await;
            }
            // with no stream at all, fall back to tokio's portable ctrl-c
            (None, None) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

/// Judge an ended attempt and apply its disposition
///
/// # Arguments
///
/// * `ctx` - The pool context
/// * `state` - The run state holding the unit
/// * `queue` - The launch queue retries re-enter at the front of
/// * `unit_name` - The unit whose attempt ended
/// * `exit` - The exit code the attempt ended with, if any
/// * `shutting_down` - Whether the pool is shutting down
fn handle_exit(
    ctx: &PoolCtx,
    state: &mut RunState,
    queue: &mut VecDeque<QueueItem>,
    unit_name: &str,
    exit: Option<i32>,
    shutting_down: bool,
) -> Result<(), Error> {
    let Some(unit) = state.units.get_mut(unit_name) else {
        return Ok(());
    };
    // close the attempt that just ended
    let mode = unit
        .attempts
        .last()
        .map_or(LaunchMode::Fresh, |attempt| attempt.mode);
    if let Some(attempt) = unit.attempts.last_mut() {
        attempt.ended = Some(Utc::now());
        attempt.exit = exit;
    }
    unit.pid = None;
    // observe the artifacts and decide the unit's next step
    let obs = ctx.observe(unit, mode, exit, shutting_down);
    let budget = ctx.budget_for(unit_name, unit);
    let disposition = decide(&obs, &budget);
    ctx.log.log(&format!(
        "exit {}: code={exit:?} -> {disposition:?}",
        unit.session
    ));
    // apply the decision to the unit and the queue
    match disposition {
        Disposition::Complete => {
            // record the final usage and cost off the stderr tail
            let tail = scan::parse_usage_tail(&ctx.err_path(&unit.session));
            unit.status = UnitStatus::Complete;
            unit.scoreable = true;
            unit.usage = tail.usage;
            unit.cost = tail.cost;
            unit.fail_reason = None;
        }
        Disposition::Retry { mode, wipe_store } => {
            // wipe a stillborn store so the fresh launch can claim the name
            if wipe_store {
                let store = ctx.sessions_root.join(&unit.session);
                std::fs::remove_dir_all(&store).map_err(|err| {
                    Error::new(format!("failed to wipe {}: {err}", store.display()))
                })?;
                ctx.log
                    .log(&format!("wiped stillborn store {}", unit.session));
            }
            // retries beat fresh launches so a wounded unit is not starved
            unit.status = UnitStatus::Pending;
            queue.push_front(QueueItem {
                unit: unit_name.to_owned(),
                mode,
                wipe: false,
            });
        }
        Disposition::Interrupted => unit.status = UnitStatus::Interrupted,
        Disposition::Failed(reason) => {
            unit.status = UnitStatus::Failed;
            unit.fail_reason = Some(reason);
        }
    }
    // persist every transition so a crash never loses an attempt
    state.save(&ctx.run_dir)
}

/// Spawn one attempt as a `thorctl analyze chat` child process
///
/// # Arguments
///
/// * `ctx` - The pool context
/// * `state` - The run state holding the unit
/// * `item` - The queued launch to perform
async fn spawn_attempt(
    ctx: &PoolCtx<'_>,
    state: &mut RunState,
    item: &QueueItem,
) -> Result<Child, Error> {
    let Some(unit) = state.units.get_mut(&item.unit) else {
        return Err(Error::new(format!("unit {} has no state", item.unit)));
    };
    // wipe a stillborn store when the queue item asked for it
    if item.wipe {
        let store = ctx.sessions_root.join(&unit.session);
        if store.exists() {
            std::fs::remove_dir_all(&store)
                .map_err(|err| Error::new(format!("failed to wipe {}: {err}", store.display())))?;
            ctx.log
                .log(&format!("wiped stillborn store {}", unit.session));
        }
    }
    // spawn thorctl itself for the child session; tests may override the
    // spawn target with a stub that simulates the headless exit contract
    let exe = match std::env::var_os("THORCTL_BATCH_SPAWN") {
        Some(stub) => PathBuf::from(stub),
        None => std::env::current_exe()
            .map_err(|err| Error::new(format!("failed to resolve the thorctl binary: {err}")))?,
    };
    let mut command = Command::new(exe);
    // the child loads the same thorctl config this pool validated
    command.arg("--config").arg(&ctx.config_path);
    command.arg("analyze").arg("chat");
    // build the mode-specific arguments
    match item.mode {
        LaunchMode::Fresh => {
            // the samples ride as positionals, primary first
            for sample in &unit.samples {
                command.arg(sample);
            }
            command.arg("--name").arg(&unit.session);
            // the manifest's session toggles only apply to fresh launches
            if ctx.conf.fresh_project {
                command.arg("--fresh-project");
            }
            if ctx.conf.anonymize {
                command.arg("--anonymize");
            }
            if ctx.conf.disable_detectors {
                command.arg("--disable-detectors");
            }
            // the prompt comes inline or from the manifest's prompt file
            match (&ctx.conf.prompt, &ctx.conf.prompt_file) {
                (Some(prompt), _) => command.arg("--prompt").arg(prompt),
                (None, Some(prompt_file)) => command.arg("--prompt-file").arg(prompt_file),
                // the manifest validated exactly one prompt source exists
                (None, None) => unreachable!("manifest validation requires a prompt"),
            };
        }
        LaunchMode::Continue => {
            // reopen the saved session's loop on its open questions
            command.arg("--resume").arg(&unit.session).arg("--continue");
        }
        LaunchMode::ReportOnly => {
            // regenerate the report without running the agent loop
            command
                .arg("--resume")
                .arg(&unit.session)
                .arg("--report-only");
        }
    }
    // every mode writes the report and rides the same pause budget
    command.arg("--report").arg(ctx.report_path(&unit.session));
    command.arg("--pause-wait").arg(&ctx.conf.pause_wait);
    // forward the manifest's turn cap so children raise their main loop
    if let Some(turns) = ctx.conf.max_turns {
        command.arg("--max-turns").arg(turns.to_string());
    }
    // the deny-token list rides every launch mode: it is never persisted in
    // the store, so resumed/continued sessions need it re-supplied
    if let Some(tokens) = &ctx.conf.redact_tokens {
        command.arg("--redact-tokens").arg(tokens);
    }
    if let Some(template) = &ctx.conf.report_template {
        command.arg("--report-template").arg(template);
    }
    if ctx.conf.defang {
        command.arg("--defang");
    }
    // stdout is per-attempt; stderr appends so usage history accumulates
    let out = std::fs::File::create(ctx.out_path(&unit.session))
        .map_err(|err| Error::new(format!("failed to create the stdout log: {err}")))?;
    let err = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(ctx.err_path(&unit.session))
        .map_err(|err| Error::new(format!("failed to open the stderr log: {err}")))?;
    // snapshot the log length before the child writes so this attempt's usage
    // lines are separable from any earlier attempt's (scoreable gate, audit R-1)
    let err_offset = err.metadata().map(|meta| meta.len()).unwrap_or(0);
    command.stdin(Stdio::null());
    command.stdout(Stdio::from(out));
    command.stderr(Stdio::from(err));
    // children own their process group so terminal signals reach only the
    // pool, which forwards exactly one graceful signal per child
    #[cfg(unix)]
    command.process_group(0);
    // source fresh credentials for this spawn (the stale-creds lesson)
    if let Some(env_file) = &ctx.conf.env_file {
        for (key, value) in scan::parse_env_file(env_file)? {
            command.env(key, value);
        }
        // advertise the file itself so a PAUSED session's credential
        // refresher can re-read it in place: the env vars above are frozen
        // into the child at spawn, so an operator refresh of the file never
        // reaches a running session without this
        let advertised = std::path::absolute(env_file).unwrap_or_else(|_| env_file.clone());
        command.env("THORPAL_CREDS_ENV_FILE", advertised);
    }
    // the child outlives a dropped handle so a pool crash never kills runs
    command.kill_on_drop(false);
    // spawn and record the attempt
    let child = command
        .spawn()
        .map_err(|err| Error::new(format!("failed to spawn thorctl: {err}")))?;
    let pid = child.id().unwrap_or(0);
    unit.attempts.push(Attempt {
        mode: item.mode,
        started: Utc::now(),
        ended: None,
        exit: None,
        err_offset,
    });
    unit.status = UnitStatus::Running;
    unit.pid = Some(pid);
    ctx.log.log(&format!(
        "launched {} pid={pid} mode={:?} members={}",
        unit.session,
        item.mode,
        unit.samples.len()
    ));
    state.save(&ctx.run_dir)?;
    Ok(child)
}

/// Print the per-unit epilogue and totals
///
/// # Arguments
///
/// * `ctx` - The pool context
/// * `state` - The final run state
fn epilogue(ctx: &PoolCtx, state: &RunState) {
    // one line per unit in the original summary shape
    let mut complete = 0usize;
    let mut failed = 0usize;
    let mut other = 0usize;
    let mut tokens = 0u64;
    let mut cost = 0.0f64;
    for unit in state.units.values() {
        // re-derive the summary facts from the artifacts
        let report = file_nonempty(&ctx.report_path(&unit.session));
        let tail = scan::parse_usage_tail(&ctx.err_path(&unit.session));
        eprintln!(
            "{} status={:?} report={} usage={}",
            unit.session,
            unit.status,
            if report { "yes" } else { "no" },
            tail.usage_lines
        );
        // accumulate the totals
        match unit.status {
            UnitStatus::Complete => complete += 1,
            UnitStatus::Failed => failed += 1,
            _ => other += 1,
        }
        if let Some(usage) = &unit.usage {
            tokens += usage.total;
        }
        if let Some(unit_cost) = unit.cost {
            cost += unit_cost;
        }
    }
    eprintln!(
        "pool done: {complete} complete, {failed} failed, {other} pending/interrupted; \
         {tokens} tokens, ${cost:.2} estimated"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a default observation for one ended attempt
    ///
    /// # Arguments
    ///
    /// * `exit` - The exit code the attempt ended with
    fn obs(exit: Option<i32>) -> ExitObs {
        ExitObs {
            mode: LaunchMode::Fresh,
            exit,
            store_exists: true,
            ownership: StoreOwnership::Owned,
            usage_present: true,
            attempt_usage_present: true,
            report_nonempty: false,
            shutting_down: false,
        }
    }

    /// Build a budget with nothing spent yet
    fn budget() -> Budget {
        Budget {
            run_attempts: 1,
            report_attempts: 0,
            max_retries: 2,
            report_retries: 2,
        }
    }

    /// A scoreable unit completes regardless of its exit code
    #[test]
    fn scoreable_completes() {
        // a clean exit with a report and usage completes
        let mut done = obs(Some(0));
        done.report_nonempty = true;
        assert_eq!(decide(&done, &budget()), Disposition::Complete);
        // even a signal death completes once the artifacts are scoreable,
        // which is how adopted orphans are judged
        let mut adopted = obs(None);
        adopted.report_nonempty = true;
        assert_eq!(decide(&adopted, &budget()), Disposition::Complete);
    }

    /// A settled run missing its report retries report-only until the budget ends
    #[test]
    fn missing_report_retries_report_only() {
        // the first report retry is allowed
        let settled = obs(Some(0));
        assert_eq!(
            decide(&settled, &budget()),
            Disposition::Retry {
                mode: LaunchMode::ReportOnly,
                wipe_store: false
            }
        );
        // a failed report-only attempt retries itself
        let mut report_only = obs(Some(1));
        report_only.mode = LaunchMode::ReportOnly;
        let mut spent = budget();
        spent.report_attempts = 1;
        assert_eq!(
            decide(&report_only, &spent),
            Disposition::Retry {
                mode: LaunchMode::ReportOnly,
                wipe_store: false
            }
        );
        // an exhausted report budget fails the unit
        spent.report_attempts = 2;
        assert!(matches!(
            decide(&report_only, &spent),
            Disposition::Failed(_)
        ));
        // a vanished store cannot resume for its report at all
        let mut gone = obs(Some(0));
        gone.store_exists = false;
        assert!(matches!(decide(&gone, &budget()), Disposition::Failed(_)));
    }

    /// Shutdown interrupts park the unit without charging budget
    #[test]
    fn shutdown_interrupts_park() {
        // exit 130 while shutting down is the operator stopping the pool
        let mut interrupted = obs(Some(130));
        interrupted.shutting_down = true;
        assert_eq!(decide(&interrupted, &budget()), Disposition::Interrupted);
        // signal deaths and plain failures during the drain park the same way
        // (adopted orphans are only ever observed with no exit code)
        let mut signal_death = obs(None);
        signal_death.shutting_down = true;
        assert_eq!(decide(&signal_death, &budget()), Disposition::Interrupted);
        let mut failing = obs(Some(1));
        failing.shutting_down = true;
        assert_eq!(decide(&failing, &budget()), Disposition::Interrupted);
        // an external 130 with the pool still running retries like a failure
        let external = obs(Some(130));
        assert_eq!(
            decide(&external, &budget()),
            Disposition::Retry {
                mode: LaunchMode::Continue,
                wipe_store: false
            }
        );
    }

    /// Failures continue a spent session and restart a stillborn one
    #[test]
    fn failures_pick_continue_or_fresh() {
        // a spent session continues to keep its paid progress
        let spent = obs(Some(1));
        assert_eq!(
            decide(&spent, &budget()),
            Disposition::Retry {
                mode: LaunchMode::Continue,
                wipe_store: false
            }
        );
        // an expired pause (exit 75) rides the same continue path
        let paused = obs(Some(75));
        assert_eq!(
            decide(&paused, &budget()),
            Disposition::Retry {
                mode: LaunchMode::Continue,
                wipe_store: false
            }
        );
        // a stillborn store is wiped and relaunched fresh
        let mut stillborn = obs(Some(1));
        stillborn.usage_present = false;
        stillborn.attempt_usage_present = false;
        assert_eq!(
            decide(&stillborn, &budget()),
            Disposition::Retry {
                mode: LaunchMode::Fresh,
                wipe_store: true
            }
        );
        // no store at all just launches fresh
        let mut missing = obs(Some(1));
        missing.store_exists = false;
        missing.usage_present = false;
        missing.attempt_usage_present = false;
        assert_eq!(
            decide(&missing, &budget()),
            Disposition::Retry {
                mode: LaunchMode::Fresh,
                wipe_store: false
            }
        );
    }

    /// Attempt-scoped usage gates completion without reclassifying spend
    #[test]
    fn stale_usage_never_completes_but_spend_still_continues() {
        // a spent session whose ended attempt died before printing usage must
        // CONTINUE (not be wiped as stillborn): the paid progress is on disk
        let mut dead_continue = obs(Some(1));
        dead_continue.attempt_usage_present = false;
        assert_eq!(
            decide(&dead_continue, &budget()),
            Disposition::Retry {
                mode: LaunchMode::Continue,
                wipe_store: false
            }
        );
        // a report plus only a STALE usage line is not scoreable: the unit
        // retries report-only, which reprints usage, instead of completing
        let mut stale = obs(Some(0));
        stale.report_nonempty = true;
        stale.attempt_usage_present = false;
        assert_eq!(
            decide(&stale, &budget()),
            Disposition::Retry {
                mode: LaunchMode::ReportOnly,
                wipe_store: false
            }
        );
        // the ended attempt's own usage line still completes as before
        let mut fresh = obs(Some(0));
        fresh.report_nonempty = true;
        assert_eq!(decide(&fresh, &budget()), Disposition::Complete);
    }

    /// A foreign session store fails the unit instead of being touched
    #[test]
    fn foreign_store_is_never_touched() {
        // an unowned store must never be wiped or resumed
        let mut foreign = obs(Some(1));
        foreign.ownership = StoreOwnership::Foreign;
        assert!(matches!(
            decide(&foreign, &budget()),
            Disposition::Failed(_)
        ));
        // the report-only path must refuse a foreign store the same way
        let mut settled = obs(Some(0));
        settled.ownership = StoreOwnership::Foreign;
        assert!(matches!(
            decide(&settled, &budget()),
            Disposition::Failed(_)
        ));
    }

    /// Unverifiable ownership blocks wipes but keeps safe resumes working
    #[test]
    fn unverifiable_store_blocks_wipes_only() {
        // a settled session continues: the resume never reads the source file
        let mut settled = obs(Some(1));
        settled.ownership = StoreOwnership::Unverifiable;
        assert_eq!(
            decide(&settled, &budget()),
            Disposition::Retry {
                mode: LaunchMode::Continue,
                wipe_store: false
            }
        );
        // a report-only resume is equally safe
        let mut report_only = obs(Some(0));
        report_only.ownership = StoreOwnership::Unverifiable;
        assert_eq!(
            decide(&report_only, &budget()),
            Disposition::Retry {
                mode: LaunchMode::ReportOnly,
                wipe_store: false
            }
        );
        // a stillborn store would be wiped, which needs proof: park it failed
        // with the restore hint instead
        let mut stillborn = obs(Some(1));
        stillborn.ownership = StoreOwnership::Unverifiable;
        stillborn.usage_present = false;
        stillborn.attempt_usage_present = false;
        match decide(&stillborn, &budget()) {
            Disposition::Failed(reason) => {
                assert!(reason.contains("unverifiable"));
                assert!(reason.contains("--retry-exhausted"));
            }
            other => panic!("a stillborn unverifiable store must park, got {other:?}"),
        }
    }

    /// Ownership matches by file name, or by content sha for anonymized stores
    #[test]
    fn store_ownership_matches_name_or_content_sha() {
        let dir = scratch_dir("ownership");
        // the unit's primary sample on disk, named by the operator
        let sample = dir.join("evil.exe");
        std::fs::write(&sample, b"corny bytes").unwrap();
        let sha = file_sha256(&sample).expect("hashable");
        // a plain store records the operator-named path: name matches
        let named = dir.join("sessions-named");
        std::fs::create_dir_all(named.join("s1")).unwrap();
        std::fs::write(
            named.join("s1").join("meta.json"),
            r#"{"binary": "/corpus/evil.exe"}"#,
        )
        .unwrap();
        assert_eq!(
            store_ownership(&named, "s1", &[sample.clone()]),
            StoreOwnership::Owned
        );
        // an anonymized store records only the staged sha copy: the name
        // misses but the content hash still proves ownership
        let anon = dir.join("sessions-anon");
        std::fs::create_dir_all(anon.join("s2")).unwrap();
        std::fs::write(
            anon.join("s2").join("meta.json"),
            format!(r#"{{"binary": "/projects/{sha}/sample/{sha}"}}"#),
        )
        .unwrap();
        assert_eq!(
            store_ownership(&anon, "s2", &[sample.clone()]),
            StoreOwnership::Owned
        );
        // a store recording someone else's sample matches neither way
        std::fs::create_dir_all(anon.join("s3")).unwrap();
        std::fs::write(
            anon.join("s3").join("meta.json"),
            r#"{"binary": "/projects/other/sample/other"}"#,
        )
        .unwrap();
        assert_eq!(
            store_ownership(&anon, "s3", &[sample.clone()]),
            StoreOwnership::Foreign
        );
        // a vanished source sample makes ownership unverifiable, not foreign:
        // the unit must park recoverably instead of failing or aborting
        std::fs::remove_file(&sample).unwrap();
        assert_eq!(
            store_ownership(&anon, "s3", &[sample]),
            StoreOwnership::Unverifiable
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The run budget caps fresh and continue retries
    #[test]
    fn run_budget_caps_retries() {
        // the last allowed attempt still retries
        let mut spent = budget();
        spent.run_attempts = 2;
        assert!(matches!(
            decide(&obs(Some(1)), &spent),
            Disposition::Retry { .. }
        ));
        // one more attempt exhausts the budget
        spent.run_attempts = 3;
        assert!(matches!(
            decide(&obs(Some(1)), &spent),
            Disposition::Failed(_)
        ));
    }

    /// Build a unique scratch directory for one test
    ///
    /// # Arguments
    ///
    /// * `name` - A short name distinguishing the test
    fn scratch_dir(name: &str) -> PathBuf {
        // unique per process and call so parallel tests never collide
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let count = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "thorctl-batch-pool-{name}-{}-{count}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Write a batch.yml listing the given units and load it
    ///
    /// # Arguments
    ///
    /// * `dir` - The evaluation directory to write into
    /// * `blind` - Whether the manifest runs blind
    /// * `units` - The unit names to list, each with one path sample
    fn write_manifest(dir: &Path, blind: bool, units: &[&str]) -> BatchConf {
        // every unit gets one on-disk sample under samples/
        std::fs::create_dir_all(dir.join("samples")).unwrap();
        // pin the free-disk floor to zero: with the default 15 GiB floor a
        // nearly-full host parks every launch in the disk hold and the tests
        // hang forever instead of running their stubs
        let mut yaml = format!(
            "prompt: analyze\nblind: {blind}\nstagger_secs: 0\ndisk_free_min_gb: 0\nunits:\n"
        );
        for unit in units {
            std::fs::write(dir.join("samples").join(unit), b"bytes").unwrap();
            yaml.push_str(&format!(
                "  - name: {unit}\n    samples:\n      - samples/{unit}\n"
            ));
        }
        std::fs::write(dir.join("batch.yml"), yaml).unwrap();
        BatchConf::load(dir).unwrap()
    }

    /// Build a pool context over scratch run and session directories
    ///
    /// # Arguments
    ///
    /// * `conf` - The loaded manifest to drive
    /// * `dir` - The evaluation directory holding the run tree
    fn test_ctx<'a>(conf: &'a BatchConf, dir: &Path) -> PoolCtx<'a> {
        // keep the sessions root inside the scratch dir so tests stay hermetic
        let run_dir = super::super::state::run_dir(dir, 1);
        std::fs::create_dir_all(&run_dir).unwrap();
        let sessions_root = dir.join("sessions");
        std::fs::create_dir_all(&sessions_root).unwrap();
        PoolCtx {
            conf,
            run: 1,
            run_dir: run_dir.clone(),
            sessions_root,
            config_path: PathBuf::from("/dev/null"),
            log: PoolLog {
                path: run_dir.join("pool.log"),
                quiet: true,
            },
            rearms: HashMap::new(),
        }
    }

    /// Build a default run command for the tests
    fn run_cmd() -> BatchRun {
        BatchRun {
            dir: PathBuf::from("."),
            run: Some(1),
            new_run: false,
            units: Vec::new(),
            dry_run: false,
            retry_exhausted: false,
            max_concurrency: None,
        }
    }

    /// Reconciliation registers new units and extends a started run
    #[test]
    fn reconcile_registers_and_extends() {
        // start with two units and no state
        let dir = scratch_dir("extend");
        let conf = write_manifest(&dir, true, &["u1", "u2"]);
        let mut ctx = test_ctx(&conf, &dir);
        let mut state = RunState::new(1);
        let plan = reconcile(&mut ctx, &mut state, &run_cmd()).unwrap();
        // both units queue fresh under shuffled two-digit aliases
        assert_eq!(plan.queue.len(), 2);
        assert!(plan.queue.iter().all(|item| item.mode == LaunchMode::Fresh));
        let mut aliases: Vec<String> = state
            .units
            .values()
            .map(|unit| unit.alias.clone())
            .collect();
        aliases.sort();
        assert_eq!(aliases, vec!["s01".to_owned(), "s02".to_owned()]);
        // finish the first two units so the extension is the only new work
        for name in ["u1", "u2"] {
            let session = state.units[name].session.clone();
            state.units.get_mut(name).unwrap().status = UnitStatus::Complete;
            std::fs::write(ctx.report_path(&session), b"report").unwrap();
        }
        // extending the manifest only queues the brand-new unit
        let conf = write_manifest(&dir, true, &["u1", "u2", "u3"]);
        let mut ctx = test_ctx(&conf, &dir);
        let plan = reconcile(&mut ctx, &mut state, &run_cmd()).unwrap();
        assert_eq!(plan.queue.len(), 1);
        assert_eq!(plan.queue[0].unit, "u3");
        // the new unit continues the alias sequence past the taken ones
        assert_eq!(state.units["u3"].alias, "s03");
        assert_eq!(state.units["u3"].session, "batch1-s03");
    }

    /// An unblinded manifest aliases units by their own names
    #[test]
    fn reconcile_unblinded_uses_unit_names() {
        // one unit with blinding off
        let dir = scratch_dir("unblinded");
        let conf = write_manifest(&dir, false, &["u1"]);
        let mut ctx = test_ctx(&conf, &dir);
        let mut state = RunState::new(1);
        reconcile(&mut ctx, &mut state, &run_cmd()).unwrap();
        // the alias and session carry the unit name
        assert_eq!(state.units["u1"].alias, "u1");
        assert_eq!(state.units["u1"].session, "batch1-u1");
    }

    /// A complete unit is skipped until its report file vanishes
    #[test]
    fn reconcile_complete_skips_until_report_lost() {
        // register one unit and mark it complete with a report on disk
        let dir = scratch_dir("complete");
        let conf = write_manifest(&dir, true, &["u1"]);
        let mut ctx = test_ctx(&conf, &dir);
        let mut state = RunState::new(1);
        reconcile(&mut ctx, &mut state, &run_cmd()).unwrap();
        let session = state.units["u1"].session.clone();
        state.units.get_mut("u1").unwrap().status = UnitStatus::Complete;
        std::fs::write(ctx.report_path(&session), b"report").unwrap();
        // a complete unit with its report intact queues nothing
        let plan = reconcile(&mut ctx, &mut state, &run_cmd()).unwrap();
        assert!(plan.queue.is_empty());
        // losing the report queues a report-only recovery
        std::fs::remove_file(ctx.report_path(&session)).unwrap();
        let plan = reconcile(&mut ctx, &mut state, &run_cmd()).unwrap();
        assert_eq!(plan.queue.len(), 1);
        assert_eq!(plan.queue[0].mode, LaunchMode::ReportOnly);
    }

    /// Changing a launched unit's samples is a hard error
    #[test]
    fn reconcile_sample_mutation_errors() {
        // register the unit with its original sample
        let dir = scratch_dir("mutation");
        let conf = write_manifest(&dir, true, &["u1"]);
        let mut ctx = test_ctx(&conf, &dir);
        let mut state = RunState::new(1);
        reconcile(&mut ctx, &mut state, &run_cmd()).unwrap();
        // swap the unit's sample list in the manifest
        std::fs::write(dir.join("samples").join("other"), b"other").unwrap();
        let yaml = "prompt: analyze\nunits:\n  - name: u1\n    samples:\n      - samples/other\n";
        std::fs::write(dir.join("batch.yml"), yaml).unwrap();
        let conf = BatchConf::load(&dir).unwrap();
        let mut ctx = test_ctx(&conf, &dir);
        // the mutation is rejected with the add-a-new-unit hint
        let err = reconcile(&mut ctx, &mut state, &run_cmd()).unwrap_err();
        assert!(err.to_string().contains("add a new unit"), "err: {err}");
    }

    /// A failed unit stays failed until --retry-exhausted re-arms it
    #[test]
    fn reconcile_failed_needs_rearm() {
        // register one unit and fail it with a spent attempt
        let dir = scratch_dir("rearm");
        let conf = write_manifest(&dir, true, &["u1"]);
        let mut ctx = test_ctx(&conf, &dir);
        let mut state = RunState::new(1);
        reconcile(&mut ctx, &mut state, &run_cmd()).unwrap();
        let unit = state.units.get_mut("u1").unwrap();
        unit.status = UnitStatus::Failed;
        unit.fail_reason = Some("exit Some(1) after 3 attempts".to_owned());
        unit.attempts.push(Attempt {
            mode: LaunchMode::Fresh,
            started: Utc::now(),
            ended: Some(Utc::now()),
            exit: Some(1),
            err_offset: 0,
        });
        // without the flag the unit stays failed and queues nothing
        let plan = reconcile(&mut ctx, &mut state, &run_cmd()).unwrap();
        assert!(plan.queue.is_empty());
        assert_eq!(state.units["u1"].status, UnitStatus::Failed);
        // the flag re-arms the budget and queues a retry
        let mut cmd = run_cmd();
        cmd.retry_exhausted = true;
        let plan = reconcile(&mut ctx, &mut state, &cmd).unwrap();
        assert_eq!(plan.queue.len(), 1);
        assert_eq!(state.units["u1"].status, UnitStatus::Pending);
        // the re-arm forgives the spent attempt in the budget view
        let budget = ctx.budget_for("u1", &state.units["u1"]);
        assert_eq!(budget.run_attempts, 0);
    }

    /// Serialize the stub-spawn tests since the env override is process-wide
    #[cfg(unix)]
    static SPAWN_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Write an executable stub child script
    ///
    /// # Arguments
    ///
    /// * `dir` - The directory to write the stub into
    /// * `body` - The script body below the shebang
    #[cfg(unix)]
    fn write_stub(dir: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        // the stub parses --report/--report-only off its argv like thorctl
        let path = dir.join("stub.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// A shell fragment resolving the --report path into $report
    #[cfg(unix)]
    const PARSE_REPORT: &str = "report=''\nprev=''\nfor a in \"$@\"; do\n  \
        if [ \"$prev\" = '--report' ]; then report=\"$a\"; fi\n  prev=\"$a\"\ndone";

    /// A shell fragment printing a scoreable usage and cost tail
    #[cfg(unix)]
    const PRINT_USAGE: &str = "echo 'usage: input=10 output=20 cache_read=30 \
        cache_write=40 total=100 agents=2' >&2\n\
        echo 'estimated cost: $1.2345 (test-model)' >&2";

    /// Run one pool over a manifest with the spawn target stubbed
    ///
    /// # Arguments
    ///
    /// * `dir` - The evaluation directory
    /// * `conf` - The loaded manifest
    /// * `stub` - The stub child the pool spawns
    #[cfg(unix)]
    async fn run_stubbed(
        dir: &Path,
        conf: &BatchConf,
        stub: &Path,
    ) -> (Result<(), Error>, RunState) {
        // the env override is process-wide, so hold the lock across the run
        let _guard = SPAWN_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        // SAFETY: the lock serializes every test that touches this variable
        unsafe { std::env::set_var("THORCTL_BATCH_SPAWN", stub) };
        // reconcile and drive the pool exactly like handle() does
        let mut ctx = test_ctx(conf, dir);
        let mut state = RunState::new(1);
        let cmd = run_cmd();
        let plan = reconcile(&mut ctx, &mut state, &cmd).unwrap();
        let outcome = run_pool(&mut ctx, &mut state, &cmd, plan).await;
        // SAFETY: still serialized by the held lock
        unsafe { std::env::remove_var("THORCTL_BATCH_SPAWN") };
        (outcome, state)
    }

    /// A clean child completes its unit with usage and a report
    #[cfg(unix)]
    #[tokio::test]
    async fn pool_completes_a_clean_unit() {
        // the stub prints usage and writes the report like a settled run
        let dir = scratch_dir("clean");
        let conf = write_manifest(&dir, true, &["u1"]);
        let stub = write_stub(
            &dir,
            &format!("{PARSE_REPORT}\n{PRINT_USAGE}\nprintf 'report' > \"$report\"\nexit 0"),
        );
        let (outcome, state) = run_stubbed(&dir, &conf, &stub).await;
        // the pool succeeds with the unit complete and scoreable
        outcome.unwrap();
        let unit = &state.units["u1"];
        assert_eq!(unit.status, UnitStatus::Complete);
        assert!(unit.scoreable);
        assert_eq!(unit.attempts.len(), 1);
        assert_eq!(unit.attempts[0].mode, LaunchMode::Fresh);
        assert_eq!(unit.attempts[0].exit, Some(0));
        // the usage and cost came off the stderr tail
        assert_eq!(unit.usage.unwrap().total, 100);
        assert_eq!(unit.cost, Some(1.2345));
    }

    /// A stillborn first attempt relaunches fresh and then completes
    #[cfg(unix)]
    #[tokio::test]
    async fn pool_retries_stillborn_then_succeeds() {
        // the stub fails silently once, then behaves like a settled run
        let dir = scratch_dir("stillborn");
        let conf = write_manifest(&dir, true, &["u1"]);
        let flag = dir.join("tried");
        let stub = write_stub(
            &dir,
            &format!(
                "if [ ! -f {flag} ]; then\n  touch {flag}\n  exit 1\nfi\n\
                 {PARSE_REPORT}\n{PRINT_USAGE}\nprintf 'report' > \"$report\"\nexit 0",
                flag = flag.display()
            ),
        );
        let (outcome, state) = run_stubbed(&dir, &conf, &stub).await;
        // the retry completed the unit on its second fresh attempt
        outcome.unwrap();
        let unit = &state.units["u1"];
        assert_eq!(unit.status, UnitStatus::Complete);
        assert_eq!(unit.attempts.len(), 2);
        assert_eq!(unit.attempts[0].exit, Some(1));
        assert!(unit.attempts.iter().all(|a| a.mode == LaunchMode::Fresh));
    }

    /// A settled run missing its report recovers through report-only
    #[cfg(unix)]
    #[tokio::test]
    async fn pool_report_only_recovers_lost_report() {
        // the stub settles without a report, then writes it on --report-only
        let dir = scratch_dir("reportonly");
        let conf = write_manifest(&dir, true, &["u1"]);
        let stub = write_stub(
            &dir,
            &format!(
                "{PARSE_REPORT}\n{PRINT_USAGE}\n\
                 for a in \"$@\"; do\n  if [ \"$a\" = '--report-only' ]; then\n    \
                 printf 'report' > \"$report\"\n  fi\ndone\nexit 0"
            ),
        );
        // the report-only resume needs a session store this unit owns
        let mut ctx = test_ctx(&conf, &dir);
        let mut state = RunState::new(1);
        let cmd = run_cmd();
        let _guard = SPAWN_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        // SAFETY: the lock serializes every test that touches this variable
        unsafe { std::env::set_var("THORCTL_BATCH_SPAWN", &stub) };
        let plan = reconcile(&mut ctx, &mut state, &cmd).unwrap();
        let session = state.units["u1"].session.clone();
        let store = ctx.sessions_root.join(&session);
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(
            store.join("meta.json"),
            format!("{{\"binary\": \"samples/u1\"}}"),
        )
        .unwrap();
        let outcome = run_pool(&mut ctx, &mut state, &cmd, plan).await;
        // SAFETY: still serialized by the held lock
        unsafe { std::env::remove_var("THORCTL_BATCH_SPAWN") };
        // the second attempt regenerated the report headlessly
        outcome.unwrap();
        let unit = &state.units["u1"];
        assert_eq!(unit.status, UnitStatus::Complete);
        assert_eq!(unit.attempts.len(), 2);
        assert_eq!(unit.attempts[0].mode, LaunchMode::Fresh);
        assert_eq!(unit.attempts[1].mode, LaunchMode::ReportOnly);
    }

    /// A unit that keeps failing exhausts its budget and fails the run
    #[cfg(unix)]
    #[tokio::test]
    async fn pool_fails_after_budget() {
        // the stub always fails without producing anything
        let dir = scratch_dir("budget");
        let conf = write_manifest(&dir, true, &["u1"]);
        let stub = write_stub(&dir, "exit 1");
        let (outcome, state) = run_stubbed(&dir, &conf, &stub).await;
        // the pool surfaces the incomplete unit as its own failure
        let err = outcome.unwrap_err();
        assert!(err.to_string().contains("did not complete"), "err: {err}");
        let unit = &state.units["u1"];
        assert_eq!(unit.status, UnitStatus::Failed);
        // the budget allowed the first attempt plus max_retries relaunches
        assert_eq!(unit.attempts.len(), 3);
    }

    /// A killed session's durable transcript spend survives the missing
    /// stderr usage summary, so the store is never judged stillborn
    #[test]
    fn transcript_usage_counts_as_spend() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = dir.path();
        // no transcript at all: no spend
        assert!(!store_transcript_has_usage(store));
        // a transcript with tool calls but no usage line: still no spend
        std::fs::write(
            store.join("transcript.jsonl"),
            "{\"kind\":\"user\",\"text\":\"go\"}\n{\"kind\":\"tool_call\",\"tool\":\"read\"}\n",
        )
        .expect("write");
        assert!(!store_transcript_has_usage(store));
        // one durable usage line marks the store as paid work
        std::fs::write(
            store.join("transcript.jsonl"),
            "{\"kind\":\"user\",\"text\":\"go\"}\n{\"kind\":\"usage\",\"agent\":\"main\"}\n",
        )
        .expect("write");
        assert!(store_transcript_has_usage(store));
    }
}
