//! The checks a restore runs before its first write
//!
//! The preflight refuses a backup it can't trust and a target it would merge into by surprise:
//! the backup must be finished (or legacy, with `--legacy`), pass `backup verify`, and record no
//! failures (or the admin accepts them with `--accept-failures`), and the target's backed-up
//! tables must be empty (or the admin asks for `--merge`, or for `--wipe <namespace>`,
//! whose namespace checks, guards, and plan run in place of the empty-target check). Then the
//! admin confirms, seeing what will be replaced and what the backup doesn't hold, and for a wipe
//! typing the namespace on a terminal. Each refusal is a `preflight` record naming its check, and
//! nothing in the target changes.

use chrono::Utc;
use scylla::client::session::Session;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thorium::models::BackupParams;
use thorium::{Conf, CtlConf, Thorium};
use tracing::{info, warn};

use crate::Error;
use crate::args::{BackupComponents, RestoreBackup};
use crate::backup::coverage::{Coverage, Registry};
use crate::backup::failures::{
    FAILURES_FILE, FailureKind, FailureLog, FailureRecord, Operation, read_records,
};
use crate::backup::kept::{self, KeptUsers};
use crate::backup::manifest::{
    self, LOCK_FILE, Layout, MANIFEST_FILE, MARKER_FILE, Manifest, PRUNED_FILE,
};
use crate::backup::sink::SinkFactory;
use crate::backup::target::{Target, backup_components};
use crate::backup::utils::is_link;
use crate::backup::verify::{REPORT_FILE, Verifier, missing_object};
use crate::backup::version::{self, VersionMismatch};
use crate::backup::views::{VIEWS_FILE, ViewStatements};
use crate::backup::wipe::{self, WipeReport};

/// The component a restore records its preflight refusals under
pub const RESTORE_COMPONENT: &str = "restore";

/// How many user and group names the confirmation lists before summing up the rest
const NAMES_SHOWN: usize = 20;

/// What the preflight found, for the restore and its summary
#[derive(Debug, Clone)]
pub struct Cleared {
    /// The backup's manifest, or `None` for a legacy backup
    pub manifest: Option<Manifest>,
    /// Whether the backup was restored as legacy (`--legacy`)
    pub legacy: bool,
    /// The components the backup holds
    pub components: Vec<BackupComponents>,
    /// The objects the backup recorded as failed or skipped, which `--accept-failures` accepted
    pub missing: Vec<FailureRecord>,
    /// The broken links the backup accepted when it was taken, which the restore knows without
    /// `--accept-failures`
    pub broken_links: Vec<FailureRecord>,
    /// The backed-up tables and buckets the restore writes to
    pub target: Target,
    /// The tables and buckets that held data before a `--merge` restore
    pub merged_into: Vec<String>,
    /// The statements of the views the restore drops, saved to `views.cql`
    pub views: ViewStatements,
    /// The wipe plan of a `--wipe` restore whose target holds data, run before the first restore
    pub wipe: Option<WipeReport>,
    /// The API's version and thoradm's, when they differed and `--allow-version-mismatch` let
    /// the restore go ahead
    pub version_mismatch: Option<VersionMismatch>,
}

/// Whether the preflight cleared the restore or refused it
#[derive(Debug)]
pub enum Verdict {
    /// Every check passed; the restore may write
    Cleared(Box<Cleared>),
    /// A check refused the restore and recorded why
    Refused,
}

/// Records a refusal: a `preflight` record naming the check
///
/// # Arguments
///
/// * `log` - The restore's failure log
/// * `check` - The preflight check that refused
/// * `reason` - Why, naming the flag that overrides it if there is one
async fn refuse<R: std::fmt::Display>(
    log: &FailureLog,
    check: &str,
    reason: R,
) -> Result<Verdict, Error> {
    // the check is the record's object, so a script can tell refusals apart
    let record =
        FailureRecord::failed(RESTORE_COMPONENT, Operation::Preflight, reason).object(check);
    log.record(&record).await?;
    Ok(Verdict::Refused)
}

/// Reads the manifest check's verdict: a finished backup's manifest, or a legacy backup under
/// `--legacy`, or the refusal
///
/// # Arguments
///
/// * `args` - The restore's arguments
/// * `log` - The restore's failure log
async fn check_manifest(
    args: &RestoreBackup,
    log: &FailureLog,
) -> Result<Result<Option<Manifest>, Verdict>, Error> {
    let root = &args.backup;
    let shown = root.display();
    // what the directory holds decides whether and how it can be restored
    let layout = match manifest::classify(root).await {
        Ok(layout) => layout,
        Err(error) => return refuse(log, "manifest", error).await.map(Err),
    };
    match layout {
        Layout::Finished if args.legacy => {
            let reason = format!(
                "{shown} holds a finished backup ({MANIFEST_FILE}); --legacy is only for a backup \
                 with neither {MANIFEST_FILE} nor {MARKER_FILE}, so drop it"
            );
            refuse(log, "manifest", reason).await.map(Err)
        }
        Layout::Finished => match Manifest::read(root).await {
            Ok(Some(manifest)) => Ok(Ok(Some(manifest))),
            Ok(None) => refuse(
                log,
                "manifest",
                format!("{shown}/{MANIFEST_FILE} disappeared"),
            )
            .await
            .map(Err),
            Err(error) => refuse(log, "manifest", error).await.map(Err),
        },
        Layout::Interrupted(marker) => {
            let legacy = if args.legacy {
                "; --legacy doesn't apply to an interrupted backup"
            } else {
                ""
            };
            let reason = format!(
                "{shown} holds a backup that was interrupted ({MARKER_FILE} from a run started at \
                 {}) and is incomplete; run the same backup new into it again to finish it{legacy}",
                marker.started
            );
            refuse(log, "manifest", reason).await.map(Err)
        }
        Layout::Legacy if !args.legacy => {
            let reason = format!(
                "{shown} has no {MANIFEST_FILE}, so its completeness can't be checked; if it is a \
                 backup taken by a thoradm older than manifests, pass --legacy to restore it"
            );
            refuse(log, "manifest", reason).await.map(Err)
        }
        Layout::Legacy => {
            // the lock file stays in every backup a thoradm writing manifests took, so a
            // directory holding it lost its manifest, and its archives' format with it
            if tokio::fs::try_exists(root.join(LOCK_FILE)).await? {
                let reason = format!(
                    "{shown} has no {MANIFEST_FILE} but holds {LOCK_FILE}, which only a thoradm \
                     that writes manifests leaves, so its manifest was removed; its archives' \
                     format can't be known without it, so it isn't restored as a legacy backup"
                );
                return refuse(log, "manifest", reason).await.map(Err);
            }
            warn!(
                "Restoring {shown} as a legacy backup (--legacy): it has no {MANIFEST_FILE}, so \
                 its completeness can't be checked; only its archives are verified"
            );
            Ok(Ok(None))
        }
        Layout::Empty => refuse(log, "manifest", format!("{shown} holds no backup"))
            .await
            .map(Err),
    }
}

/// Checks the API runs thoradm's own version when the restore loads `redis.json` or wipes,
/// returning the mismatch `--allow-version-mismatch` accepted, or the refusal
///
/// # Arguments
///
/// * `ctl_conf` - The Thorctl config of the API the restore talks to
/// * `args` - The restore's arguments
/// * `log` - The restore's failure log
async fn check_version(
    ctl_conf: &CtlConf,
    args: &RestoreBackup,
    log: &FailureLog,
) -> Result<Result<Option<VersionMismatch>, Verdict>, Error> {
    // only a restore that sends the API the Redis state, or wipes, asks for its version
    let redis = tokio::fs::try_exists(args.backup.join("redis.json")).await?;
    if !redis && args.wipe.is_none() {
        return Ok(Ok(None));
    }
    match version::enforce(ctl_conf, args.allow_version_mismatch, "restore").await {
        Ok(mismatch) => Ok(Ok(mismatch)),
        Err(reason) => refuse(log, "version", reason).await.map(Err),
    }
}

/// Runs `backup verify` on the backup, writing its report beside the restore's failures
///
/// # Arguments
///
/// * `args` - The restore's arguments
/// * `manifest` - The backup's manifest, or `None` for a legacy backup
/// * `log` - The restore's failure log
/// * `workers` - How many archives or objects are checked at once
/// * `reports` - The restore's report directory
/// * `sinks` - The file-system seam to write the report through
async fn check_backup(
    args: &RestoreBackup,
    manifest: Option<&Manifest>,
    log: &Arc<FailureLog>,
    workers: usize,
    reports: &Path,
    sinks: &dyn SinkFactory,
) -> Result<Option<Verdict>, Error> {
    // verify everything a manifest lists, or only the archives of a legacy backup
    let started = Utc::now();
    info!("Verifying {} before restoring it", args.backup.display());
    let verifier = Verifier::new(log.clone(), workers);
    match manifest {
        Some(manifest) => {
            verifier
                .manifest_checks(&args.backup, manifest, false)
                .await?;
        }
        None => verifier.archive_checks(&args.backup).await?,
    }
    let report = verifier.report(
        &args.backup,
        manifest.map(|manifest| manifest.format_version),
        manifest.is_some(),
        started,
    );
    manifest::write_json_atomic(reports, REPORT_FILE, &report, sinks).await?;
    // any problem refuses the restore; each is recorded above the refusal
    if report.failed() == 0 {
        return Ok(None);
    }
    let reason = format!(
        "backup verify found {} problems in the backup, each recorded in {}; restoring it could \
         lose or corrupt data, and no flag overrides this",
        report.failed(),
        log.path().display()
    );
    refuse(log, "verify", reason).await.map(Some)
}

/// What a backup's own records say it lacks or keeps as the cluster held it
#[derive(Debug, Clone, Default)]
struct Recorded {
    /// The objects the backup recorded as failed or skipped, which `--accept-failures` accepted
    missing: Vec<FailureRecord>,
    /// The broken links the backup accepted, which are known without `--accept-failures`
    broken_links: Vec<FailureRecord>,
}

/// Checks the objects the backup itself recorded as failed or skipped, and the references it
/// pruned from the Redis state, returning the failed objects when `--accept-failures` accepts
/// them, and the broken links it accepted, which need no flag
///
/// # Arguments
///
/// * `args` - The restore's arguments
/// * `manifest` - The backup's manifest
/// * `log` - The restore's failure log
async fn check_recorded_failures(
    args: &RestoreBackup,
    manifest: &Manifest,
    log: &FailureLog,
) -> Result<Result<Recorded, Verdict>, Error> {
    // a backup that recorded, accepted, and pruned nothing is missing nothing
    let recorded = manifest.failures + manifest.skipped;
    let broken = manifest.broken_links;
    let pruned = manifest.pruned.as_ref().map_or(0, |entry| entry.count);
    if recorded == 0 && pruned == 0 && broken == 0 {
        return Ok(Ok(Recorded::default()));
    }
    // only a backup that recorded something lists it, and a linked list isn't followed, since
    // its records would come from outside the backup
    let (originals, links) = if recorded > 0 || broken > 0 {
        let path = args.backup.join(FAILURES_FILE);
        if is_link(&path).await? {
            let reason = format!(
                "{} is a symbolic link; a backup holds only real files, so it isn't followed",
                path.display()
            );
            return refuse(log, "failures", reason).await.map(Err);
        }
        // the accepted broken links are told apart from the objects the backup lacks
        read_records(&path)?
            .into_iter()
            .partition::<Vec<FailureRecord>, _>(|record| record.kind != FailureKind::BrokenLink)
    } else {
        (Vec::new(), Vec::new())
    };
    // a broken link is accepted by the backup itself, so the restore only says which it knows
    if broken > 0 {
        warn!(
            "Restoring a backup that accepted {broken} broken links, references to objects the \
             cluster it was taken from lacked; they are kept as they were, and the restore \
             summary lists them"
        );
    }
    // nothing but broken links is recorded or pruned, so there is nothing to accept
    if recorded == 0 && pruned == 0 {
        return Ok(Ok(Recorded {
            missing: Vec::new(),
            broken_links: links,
        }));
    }
    // accepted, the restore goes on and its summary lists what the backup lacks
    if args.accept_failures {
        if recorded > 0 {
            warn!(
                "Restoring a backup that recorded {recorded} failed or skipped objects, which it \
                 doesn't hold (--accept-failures); the restore summary lists them"
            );
        }
        if pruned > 0 {
            warn!(
                "Restoring a backup whose Redis state had {pruned} dangling user and group \
                 references pruned, listed in its {PRUNED_FILE} (--accept-failures)"
            );
        }
        return Ok(Ok(Recorded {
            missing: originals,
            broken_links: links,
        }));
    }
    // otherwise each missing object and the pruned references are listed, and the restore refused
    for original in &originals {
        let mut record = missing_object(original);
        record.operation = Operation::Preflight;
        log.record(&record).await?;
    }
    if pruned > 0 {
        let record = FailureRecord::failed(
            BackupComponents::Redis.to_string(),
            Operation::Preflight,
            format!(
                "the backup's Redis state had {pruned} dangling user and group references pruned \
                 when it was taken (--prune-dangling), each listed in its {PRUNED_FILE}"
            ),
        )
        .key(PRUNED_FILE);
        log.record(&record).await?;
    }
    // the refusal names what the backup lacks and the flag that accepts it
    let mut lacks = Vec::new();
    if recorded > 0 {
        lacks.push(format!(
            "recorded {recorded} failed or skipped objects, listed in this failures file, which it \
             doesn't hold"
        ));
    }
    if pruned > 0 {
        lacks.push(format!(
            "pruned {pruned} dangling user and group references, listed in its {PRUNED_FILE}"
        ));
    }
    let reason = format!(
        "the backup {}; pass --accept-failures to restore it without them",
        lacks.join(", and ")
    );
    refuse(log, "recorded-failures", reason).await.map(Err)
}

/// Reads the operator users the target holds, which the Redis restore keeps in place of the
/// backup's
///
/// A backup without a Redis state restores no user, so it needs none. A target whose Redis state
/// the API can't back up, such as a namespace that was never initialized, keeps none, which the
/// log and the restore's summary say.
///
/// # Arguments
///
/// * `ctl_conf` - The Thorctl config of the API the Redis restore goes through
/// * `args` - The restore's arguments
pub async fn read_kept_users(ctl_conf: &CtlConf, args: &RestoreBackup) -> Result<KeptUsers, Error> {
    // without a Redis state no user is restored, so none needs keeping
    if !tokio::fs::try_exists(args.backup.join("redis.json")).await? {
        return Ok(KeptUsers::default());
    }
    // a target whose users can't be read keeps none, and says so
    match target_operator_users(ctl_conf).await {
        Ok(kept) => Ok(kept),
        Err(error) => {
            let reason = format!(
                "the target's operator users ({}) couldn't be read, so the Redis restore replaces \
                 them with the backup's: {error}",
                kept::OPERATOR_USERS.join(", ")
            );
            warn!("{reason}");
            Ok(KeptUsers {
                unread: Some(reason),
                ..KeptUsers::default()
            })
        }
    }
}

/// Reads the operator users a target holds, with their password hashes and tokens, through the
/// API's backup of its Redis state
///
/// A state whose records don't fit together is read again with its dangling references pruned,
/// which changes nothing in Redis and leaves the users whole.
///
/// # Arguments
///
/// * `ctl_conf` - The Thorctl config of the API the Redis restore goes through
async fn target_operator_users(ctl_conf: &CtlConf) -> Result<KeptUsers, Error> {
    // the API backs up every user record whole
    let client = Thorium::from_ctl_conf(ctl_conf.clone()).await?;
    let state = match client.system.backup().await {
        Ok(state) => state,
        Err(_) => {
            client
                .system
                .backup_with(&BackupParams::default().prune_dangling())
                .await?
        }
    };
    Ok(KeptUsers::of(state))
}

/// Lists the names the API holds of users and groups, which the Redis restore replaces
///
/// # Arguments
///
/// * `ctl_conf` - The Thorctl config of the API the restore talks to
async fn replaced_names(ctl_conf: &CtlConf) -> Result<(Vec<String>, Vec<String>), Error> {
    // ask the API the Redis restore goes through
    let client = Thorium::from_ctl_conf(ctl_conf.clone()).await?;
    let users = client.users.list().await?;
    let groups = client.groups.list().exec().await?.names;
    Ok((users, groups))
}

/// Shows a list of names, summing up those past the first few
///
/// # Arguments
///
/// * `names` - The names
fn show_names(names: &[String]) -> String {
    // the first names, then how many more
    let shown: Vec<&str> = names.iter().take(NAMES_SHOWN).map(String::as_str).collect();
    let rest = names.len().saturating_sub(NAMES_SHOWN);
    if rest == 0 {
        format!("{}: {}", names.len(), shown.join(", "))
    } else {
        format!("{}: {}, and {rest} more", names.len(), shown.join(", "))
    }
}

/// Describes everything the restore is about to do, for the confirmation or the log
///
/// # Arguments
///
/// * `config` - The config of the cluster about to be written
/// * `ctl_conf` - The Thorctl config of the API the Redis restore goes through
/// * `args` - The restore's arguments
/// * `cleared` - What the preflight found
async fn describe(
    config: &Conf,
    ctl_conf: &CtlConf,
    args: &RestoreBackup,
    cleared: &Cleared,
) -> Vec<String> {
    let thorium = &config.thorium;
    let mut lines = vec![format!(
        "Restoring {} into namespace {} (Redis {}:{}, namespace {}; Scylla {}; buckets files={}, \
         repos={}, attachments={}, results={})",
        args.backup.display(),
        thorium.namespace,
        config.redis.host,
        config.redis.port,
        thorium.namespace,
        config.scylla.nodes.join(", "),
        thorium.files.bucket,
        thorium.repos.bucket,
        thorium.attachments.bucket,
        thorium.results.bucket,
    )];
    // the Redis restore replaces the namespace's users and groups
    match replaced_names(ctl_conf).await {
        Ok((users, groups)) => lines.push(format!(
            "The Redis restore replaces the namespace's users ({}) and groups ({}) with the \
             backup's",
            show_names(&users),
            show_names(&groups)
        )),
        Err(error) => lines.push(format!(
            "The Redis restore replaces the namespace's users and groups with the backup's (the \
             API couldn't list them: {error})"
        )),
    }
    // except what the target's operator needs, which the restore keeps
    lines.push(format!(
        "The restore keeps the target's operator users ({}) and nodes in place of the backup's",
        kept::OPERATOR_USERS.join(", ")
    ));
    // the Redis restore deletes every key of the namespace, or of the database, first
    if tokio::fs::try_exists(args.backup.join("redis.json"))
        .await
        .unwrap_or(true)
    {
        lines.push(redis_wipe_warning(config, args.redis_flush_all));
    }
    // what a wipe deletes first
    if let Some(plan) = &cleared.wipe {
        lines.extend(plan.describe());
    }
    // what the flags accepted
    if let Some(mismatch) = &cleared.version_mismatch {
        lines.push(version::accepted(mismatch));
    }
    if !cleared.merged_into.is_empty() {
        lines.push(format!(
            "Merging into data the target already holds (--merge): {}; on a key both hold, the \
             backup's values win",
            cleared.merged_into.join(", ")
        ));
    }
    if cleared.legacy {
        lines.push(
            "Restoring a legacy backup (--legacy): its completeness couldn't be checked".to_owned(),
        );
    }
    if !cleared.missing.is_empty() {
        lines.push(format!(
            "The backup is missing {} objects it recorded as failed (--accept-failures)",
            cleared.missing.len()
        ));
    }
    if !cleared.broken_links.is_empty() {
        lines.push(format!(
            "The backup accepted {} broken links, references to objects the cluster it was taken \
             from lacked, listed in its {FAILURES_FILE}; they are kept as they were",
            cleared.broken_links.len()
        ));
    }
    if let Some(pruned) = cleared
        .manifest
        .as_ref()
        .and_then(|manifest| manifest.pruned.as_ref())
    {
        lines.push(format!(
            "The backup's Redis state had {} dangling user and group references pruned, listed \
             in its {PRUNED_FILE} (--accept-failures)",
            pruned.count
        ));
    }
    // what the backup doesn't hold, which a restore deletes from Redis or leaves stale
    lines.push(
        "The backup doesn't hold these; the Redis restore deletes what of them is in Redis, and \
         what is in Scylla or S3 is left as it is:"
            .to_owned(),
    );
    for entry in Registry::excluded() {
        if let Coverage::Excluded { reason } = entry.coverage {
            lines.push(format!(
                "  - {} {} ({}): {reason}",
                entry.store.label(),
                entry.name,
                entry.category.label()
            ));
        }
    }
    lines
}

/// Says that the Redis restore deletes every key of the namespace, or with a flush every key of
/// the database, before loading the backup
///
/// # Arguments
///
/// * `config` - The config of the cluster being restored
/// * `flush_all` - Whether the API flushes its whole Redis database instead
#[must_use]
pub fn redis_wipe_warning(config: &Conf, flush_all: bool) -> String {
    // a flush deletes every key of the database, whichever namespace it belongs to
    if flush_all {
        return format!(
            "The Redis restore deletes every key in the Redis database at {}:{} with FLUSHDB \
             ASYNC (--redis-flush-all) before loading the backup: namespace {ns}'s keys, its job \
             queues, reaction state, and census streams included (restore rebuilds the census), \
             and other namespaces' keys included; Redis's other databases are left alone",
            config.redis.host,
            config.redis.port,
            ns = config.thorium.namespace
        );
    }
    // the API's restore deletes the namespace's keys by prefix before it loads the backup
    format!(
        "The Redis restore deletes every key of namespace {ns} in the Redis database at {}:{} \
         before loading the backup, its job queues, reaction state, and census streams included \
         (restore rebuilds the census); keys of other namespaces are left alone",
        config.redis.host,
        config.redis.port,
        ns = config.thorium.namespace
    )
}

/// Asks the admin to confirm the restore, or takes `--confirm` as the confirmation
///
/// # Arguments
///
/// * `lines` - What the restore is about to do
/// * `confirm` - Whether `--confirm` confirmed the restore up front
fn confirm(lines: &[String], confirm: bool) -> Result<(), String> {
    // --confirm confirms without asking, and everything is still logged
    if confirm {
        for line in lines {
            warn!("{line}");
        }
        warn!("Confirmed by --confirm");
        return Ok(());
    }
    // a prompt needs a terminal to answer it
    if !(std::io::stdin().is_terminal() && std::io::stderr().is_terminal()) {
        return Err(
            "refusing to restore: no terminal is attached to confirm it; pass --confirm to restore \
             without asking"
                .to_owned(),
        );
    }
    // ask on the terminal
    let prompt = format!(
        "{}\nDo you want to restore data to the above databases?",
        lines.join("\n")
    );
    let confirmed = dialoguer::Confirm::new()
        .with_prompt(prompt)
        .interact()
        .map_err(|error| format!("failed to ask for confirmation: {error}"))?;
    if confirmed {
        Ok(())
    } else {
        Err("the restore was not confirmed".to_owned())
    }
}

/// Asks the admin to confirm a wipe by typing the namespace on a terminal, or, without one, takes
/// `--confirm` as the confirmation
///
/// # Arguments
///
/// * `lines` - What the restore is about to do, the wipe plan among it
/// * `namespace` - The namespace being wiped
/// * `confirm` - Whether `--confirm` was given
fn confirm_wipe(lines: &[String], namespace: &str, confirm: bool) -> Result<(), String> {
    // on a terminal a wipe is always typed, even with --confirm
    if std::io::stdin().is_terminal() && std::io::stderr().is_terminal() {
        let prompt = format!(
            "{}\nType the namespace ({namespace}) to delete its backed-up data and restore",
            lines.join("\n")
        );
        let typed: String = dialoguer::Input::new()
            .with_prompt(prompt)
            .allow_empty(true)
            .interact_text()
            .map_err(|error| format!("failed to ask for confirmation: {error}"))?;
        if typed.trim() == namespace {
            return Ok(());
        }
        return Err(format!(
            "the wipe was not confirmed: {:?} was typed, not the namespace {namespace}",
            typed.trim()
        ));
    }
    // without one, --confirm is the unattended confirmation, and the plan goes to the log
    if confirm {
        for line in lines {
            warn!("{line}");
        }
        warn!("Wipe of namespace {namespace} confirmed by --wipe {namespace} --confirm");
        return Ok(());
    }
    Err(format!(
        "refusing to wipe: no terminal is attached to type the namespace; pass --wipe \
         {namespace} --confirm to wipe and restore without asking"
    ))
}

/// Runs the wipe's checks in place of the empty-target check: the namespace given to `--wipe`,
/// the API's namespace, and the guards, then builds the plan, or `None` when the target is empty
///
/// # Arguments
///
/// * `config` - The config of the cluster to restore
/// * `ctl_conf` - The Thorctl config of the API the Redis restore goes through
/// * `scylla` - A session to the cluster's Scylla
/// * `given` - The namespace given to `--wipe`
/// * `target` - The backed-up tables and buckets the restore writes to, of which the empty-target
///   check examines the tables
/// * `args` - The restore's arguments
/// * `reports` - The restore's report directory
async fn check_wipe(
    config: &Conf,
    ctl_conf: &CtlConf,
    scylla: &Session,
    given: &str,
    target: &Target,
    args: &RestoreBackup,
    reports: &Path,
) -> Result<Result<Option<WipeReport>, String>, Error> {
    // the namespace given matches thoradm's config, before the API is asked anything
    if let Err(reason) = wipe::check_given(given, config) {
        return Ok(Err(reason));
    }
    // the API serves the same namespace
    let info = match Thorium::from_ctl_conf(ctl_conf.clone()).await {
        Ok(client) => client
            .system
            .get_info(None)
            .await
            .map_err(|error| error.to_string()),
        Err(error) => Err(error.to_string()),
    };
    let api_namespace = match wipe::check_api(config, info) {
        Ok(served) => served,
        Err(reason) => return Ok(Err(reason)),
    };
    // the keyspace is one the wipe may truncate in
    if let Err(reason) = wipe::guards(config, scylla).await {
        return Ok(Err(reason));
    }
    // an empty target needs no wipe
    let holding = match target.holding_data(scylla).await {
        Ok(holding) => holding,
        Err(error) => {
            return Ok(Err(format!(
                "couldn't check what the target holds: {error}"
            )));
        }
    };
    if holding.is_empty() {
        info!(
            "The target's backed-up tables are already empty, so --wipe {given} has nothing to \
             truncate"
        );
        return Ok(Ok(None));
    }
    // the plan, resuming an earlier wipe that didn't finish
    let resumed_from =
        wipe::find_unfinished(&args.backup, reports, &config.thorium.namespace).await;
    if let Some(earlier) = &resumed_from {
        warn!(
            "Resuming the unfinished wipe of namespace {given} recorded in {}",
            earlier.display()
        );
    }
    let snapshots = wipe::ScyllaSnapshots { scylla };
    match wipe::plan(
        scylla,
        &snapshots,
        target,
        api_namespace,
        &args.backup,
        resumed_from,
    )
    .await
    {
        Ok(plan) => Ok(Ok(Some(plan))),
        Err(error) => Ok(Err(format!("couldn't build the wipe plan: {error}"))),
    }
}

/// Runs every preflight check, in order, before the restore's first write
///
/// # Arguments
///
/// * `config` - The config of the cluster to restore
/// * `ctl_conf` - The Thorctl config of the API the Redis restore goes through
/// * `scylla` - A session to the cluster's Scylla
/// * `workers` - How many archives or objects verify checks at once
/// * `args` - The restore's arguments
/// * `log` - The restore's failure log
/// * `reports` - The restore's report directory
/// * `sinks` - The file-system seam the restore's files are written through
#[allow(clippy::too_many_arguments)]
pub async fn run(
    config: &Conf,
    ctl_conf: &CtlConf,
    scylla: &Session,
    workers: usize,
    args: &RestoreBackup,
    log: &Arc<FailureLog>,
    reports: &Path,
    sinks: &dyn SinkFactory,
) -> Result<Verdict, Error> {
    // the backup is finished, or legacy and restored on request
    let manifest = match check_manifest(args, log).await? {
        Ok(manifest) => manifest,
        Err(refused) => return Ok(refused),
    };
    // a restore that sends the API the Redis state, or wipes, needs an API of thoradm's own
    // version
    let version_mismatch = match check_version(ctl_conf, args, log).await? {
        Ok(mismatch) => mismatch,
        Err(refused) => return Ok(refused),
    };
    // every file the backup holds checks out
    if let Some(refused) =
        check_backup(args, manifest.as_ref(), log, workers, reports, sinks).await?
    {
        return Ok(refused);
    }
    // the backup recorded no failures, or the admin accepts what it lacks
    let Recorded {
        missing,
        broken_links,
    } = match &manifest {
        Some(manifest) => match check_recorded_failures(args, manifest, log).await? {
            Ok(recorded) => recorded,
            Err(refused) => return Ok(refused),
        },
        None => Recorded::default(),
    };
    // the target is empty, or the admin asks to merge into it or to wipe it first
    let components = backup_components(&args.backup, manifest.as_ref()).await?;
    let target = Target::new(config, &components);
    let mut wipe_plan = None;
    let merged_into = if let Some(given) = &args.wipe {
        match check_wipe(config, ctl_conf, scylla, given, &target, args, reports).await? {
            Ok(plan) => wipe_plan = plan,
            Err(reason) => return refuse(log, "wipe", reason).await,
        }
        Vec::new()
    } else {
        match target.holding_data(scylla).await {
            Ok(holding) if holding.is_empty() => Vec::new(),
            Ok(holding) if args.merge => holding,
            Ok(holding) => {
                let reason = format!(
                    "the target already holds data in {}; pass --merge to restore into it and \
                     keep what it holds (on a key both hold, the backup's values win), or --wipe \
                     <namespace> to delete the target's backed-up data first",
                    holding.join(", ")
                );
                return refuse(log, "empty-target", reason).await;
            }
            Err(error) => {
                let reason = format!("couldn't check that the target is empty: {error}");
                return refuse(log, "empty-target", reason).await;
            }
        }
    };
    // every view the restore drops can be recreated: from Scylla, or for a view an interrupted
    // restore left dropped, from an earlier views.cql
    let (views, unfound) = ViewStatements::capture(
        scylla,
        &config.thorium.namespace,
        &target.tables,
        &args.backup,
        reports,
    )
    .await?;
    if !unfound.is_empty() {
        let reason = format!(
            "{} {} missing, as an interrupted restore leaves views, and no earlier {VIEWS_FILE} \
             under {}/reports/restore-*/ or beside the report directory holds their statements; \
             replay an earlier {VIEWS_FILE} with cqlsh, or restart one API pod, whose schema setup \
             creates missing views, then run the restore again",
            unfound.join(", "),
            if unfound.len() == 1 { "is" } else { "are" },
            args.backup.display()
        );
        return refuse(log, "views", reason).await;
    }
    views.save(reports, sinks).await?;
    let cleared = Cleared {
        legacy: manifest.is_none(),
        manifest,
        components,
        missing,
        broken_links,
        target,
        merged_into,
        views,
        wipe: wipe_plan,
        version_mismatch,
    };
    // the admin confirms, seeing what will be replaced and what the backup doesn't hold, and
    // types the namespace for a wipe
    let lines = describe(config, ctl_conf, args, &cleared).await;
    let confirmed = match &cleared.wipe {
        Some(plan) => confirm_wipe(&lines, &plan.namespace, args.confirm),
        None => confirm(&lines, args.confirm),
    };
    if let Err(reason) = confirmed {
        return refuse(log, "confirmation", reason).await;
    }
    Ok(Verdict::Cleared(Box::new(cleared)))
}

/// Returns the report directory a restore writes into
///
/// # Arguments
///
/// * `args` - The restore's arguments
pub async fn report_dir(args: &RestoreBackup) -> Result<PathBuf, Error> {
    // the named directory, or a fresh one under the backup
    crate::backup::verify::open_report_dir(&args.backup, args.report_dir.as_deref(), "restore")
        .await
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{RestoreBackup, Verdict, check_recorded_failures, confirm, confirm_wipe};
    use crate::backup::failures::{
        FAILURES_FILE, FailureLog, FailurePolicy, FailureRecord, Operation, read_records,
    };
    use crate::backup::manifest::Manifest;
    use crate::backup::sink::TokioFs;

    /// Without a terminal and without `--confirm`, a restore and a wipe refuse, naming the flags
    #[test]
    fn refusals_name_the_confirm_flag() {
        use std::io::IsTerminal;
        // a terminal would prompt instead of refusing
        if std::io::stdin().is_terminal() {
            return;
        }
        let lines = vec!["plan".to_owned()];
        // the plain restore names --confirm
        let error = confirm(&lines, false).expect_err("no terminal and no --confirm refuses");
        assert!(
            error.contains("--confirm") && !error.contains("--yes"),
            "{error}"
        );
        // the wipe names both --wipe and --confirm
        let error = confirm_wipe(&lines, "thorium", false).expect_err("a wipe refuses too");
        assert!(
            error.contains("--wipe thorium --confirm") && !error.contains("--yes"),
            "{error}"
        );
        // --confirm lets both proceed
        assert!(confirm(&lines, true).is_ok());
        assert!(confirm_wipe(&lines, "thorium", true).is_ok());
    }

    /// A linked `failures.jsonl` refuses the restore instead of being read, even under
    /// `--accept-failures`
    #[tokio::test]
    async fn a_linked_failures_file_refuses_the_restore() {
        // a backup whose failures.jsonl links to a list of records outside it
        let dir = std::env::temp_dir().join(format!("thoradm-preflight-{}", uuid::Uuid::new_v4()));
        let backup = dir.join("backup");
        let reports = dir.join("reports");
        std::fs::create_dir_all(&backup).unwrap();
        std::fs::create_dir_all(&reports).unwrap();
        let outside = dir.join("elsewhere.jsonl");
        let record =
            FailureRecord::failed("samples", Operation::Download, "planted").key("planted");
        std::fs::write(
            &outside,
            format!("{}\n", serde_json::to_string(&record).unwrap()),
        )
        .unwrap();
        std::os::unix::fs::symlink(&outside, backup.join(FAILURES_FILE)).unwrap();
        // a manifest that recorded one failure, restored with --accept-failures
        let manifest: Manifest = serde_json::from_value(serde_json::json!({
            "format_version": 2,
            "thoradm_version": "0.0.0",
            "namespace": "thorium",
            "partition_sizes": {},
            "buckets": {},
            "started": "2026-01-01T00:00:00Z",
            "finished": "2026-01-01T00:00:00Z",
            "components": [],
            "failures": 1,
            "skipped": 0,
            "tables": {},
            "s3": {},
        }))
        .unwrap();
        let args = RestoreBackup::parse_from([
            "restore".as_ref(),
            "-b".as_ref(),
            backup.as_os_str(),
            "--accept-failures".as_ref(),
        ]);
        let log = FailureLog::open(&reports, FailurePolicy::Continue, &TokioFs)
            .await
            .unwrap();
        // the preflight refuses the link and records why, accepting nothing from outside
        let verdict = check_recorded_failures(&args, &manifest, &log)
            .await
            .unwrap();
        let records = read_records(log.path()).unwrap_or_default();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            matches!(verdict, Err(Verdict::Refused)),
            "the preflight followed the linked {FAILURES_FILE}: {verdict:?}"
        );
        assert!(
            records
                .iter()
                .any(|record| record.error.contains("symbolic link")),
            "the preflight didn't record the link: {records:?}"
        );
    }
}
