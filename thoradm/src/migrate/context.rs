//! What every `thoradm migrate` command that runs the runner needs: the backends, the pass's
//! rows, the writer lease, and SIGTERM

use scylla::client::session::Session;
use std::sync::Arc;
use thorium::Conf;
use thorium::models::schema::scylla::{SchemaMigrationIssuesTable, SchemaMigrationsTable};
use thorium::models::schema::{
    LeaseError, MigrateError, PassSpec, PassStore, RunBackends, RunContext, RunLimits, RunMode,
    ScyllaObject, WriterLease,
};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use super::registry::MigrationDef;
use crate::Error;
use crate::args::Args;

/// The environment variable the operator passes a Job's name in
pub const JOB_NAME_ENV: &str = "THORIUM_JOB_NAME";

/// The environment variable the operator passes a Job's UID in, which keys its result row
pub use crate::shared::termination::JOB_UID_ENV;

/// Everything a runner command works with
pub struct Env {
    /// The cluster's config
    pub conf: Conf,
    /// The Scylla, Redis, and S3 clients and the namespace
    pub backends: RunBackends,
    /// The prepared statements on a pass's rows
    pub store: PassStore,
}

/// Turn the runner's error into thoradm's, keeping what a Job's retry may get past apart from
/// what it can't
///
/// # Arguments
///
/// * `err` - The runner's error
#[must_use]
#[expect(
    clippy::needless_pass_by_value,
    reason = "it is passed to `map_err`, which hands over the error"
)]
pub fn migrate_error(err: MigrateError) -> Error {
    // a backend that failed past the retries resumes from its checkpoint when the Job retries
    match err {
        MigrateError::Backend(_) | MigrateError::Cancelled | MigrateError::LeaseLost => {
            Error::Transient(err.to_string())
        }
        MigrateError::NotImplemented(_) => Error::NotImplemented(err.to_string()),
        MigrateError::Rejected(_) | MigrateError::Systematic(_) => {
            Error::Systematic(err.to_string())
        }
    }
}

/// Load the cluster's config, as a usage error when it can't be read
///
/// # Arguments
///
/// * `args` - The arguments thoradm was started with
pub fn load_conf(args: &Args) -> Result<Conf, Error> {
    // a config that can't be loaded is the caller's to fix, never worth retrying
    Conf::new(&args.cluster_conf).map_err(|err| {
        Error::Usage(format!(
            "the config at {} can't be loaded: {err}",
            args.cluster_conf.display()
        ))
    })
}

/// Build the S3 client the cluster's config describes
///
/// # Arguments
///
/// * `conf` - The cluster's config
#[must_use]
pub fn s3_client(conf: &Conf) -> aws_sdk_s3::Client {
    // the same client a backup builds
    crate::backup::s3::s3_client(conf, conf.thorium.s3.use_path_style)
}

/// The partition size the config gives each data type
///
/// # Arguments
///
/// * `conf` - The cluster's config
#[must_use]
pub fn partition_sizes(conf: &Conf) -> std::collections::BTreeMap<String, u16> {
    // one entry per data type, with tags by tag type
    let thorium = &conf.thorium;
    [
        ("files", thorium.files.partition_size),
        ("repos", thorium.repos.partition_size),
        ("results", thorium.results.partition_size),
        ("entities", thorium.entities.partition_size),
        ("associations", thorium.associations.partition_size),
        ("tags.files", thorium.tags.files.partition_size),
        ("tags.repos", thorium.tags.repos.partition_size),
        ("tags.entities", thorium.tags.entities.partition_size),
    ]
    .into_iter()
    .map(|(name, size)| (name.to_owned(), size))
    .collect()
}

/// Connect to every backend and prepare the pass statements
///
/// # Arguments
///
/// * `args` - The arguments thoradm was started with
pub async fn connect(args: &Args) -> Result<Env, Error> {
    let conf = load_conf(args)?;
    // an unreachable backend is worth retrying
    let scylla = crate::shared::scylla::get_client(&conf)
        .await
        .map_err(|err| Error::Transient(format!("Scylla can't be reached: {err}")))?;
    let redis = crate::shared::redis::get_client(&conf)
        .await
        .map_err(|err| Error::Transient(format!("Redis can't be reached: {err}")))?;
    let s3 = s3_client(&conf);
    let namespace = conf.thorium.namespace.clone();
    let scylla = Arc::new(scylla);
    let partition_sizes = partition_sizes(&conf);
    // a pass keeps its state in the record tables, which a converted pre-Helm deployment lacks in
    // part until its 2026-10-v03 schema apply (its seeded record made only schema_migrations)
    create_record_tables(&scylla, &namespace).await?;
    let store = PassStore::prepare(scylla.clone(), &namespace)
        .await
        .map_err(migrate_error)?;
    // migrations read the rest of their settings from the same config
    let shared_conf = Arc::new(conf.clone());
    Ok(Env {
        conf,
        backends: RunBackends {
            partition_sizes,
            namespace,
            scylla,
            redis,
            s3,
            conf: shared_conf,
        },
        store,
    })
}

/// Create the record tables a pass reads and writes when the keyspace lacks them, with the
/// definitions a schema apply creates them with
///
/// # Arguments
///
/// * `scylla` - The Scylla session
/// * `namespace` - The keyspace
async fn create_record_tables(scylla: &Session, namespace: &str) -> Result<(), Error> {
    // each table's own CREATE TABLE IF NOT EXISTS, which leaves an existing table as it is
    for def in [
        <SchemaMigrationsTable as ScyllaObject>::DEF,
        <SchemaMigrationIssuesTable as ScyllaObject>::DEF,
    ] {
        scylla
            .query_unpaged(def.create_cql(namespace, None), ())
            .await
            .map_err(|err| {
                Error::Transient(format!(
                    "the record table {} can't be created: {err}",
                    def.name()
                ))
            })?;
    }
    Ok(())
}

/// Who a manual run or a Job is, for the writer lease and the pass row
#[must_use]
pub fn holder() -> String {
    // a Job is named by its Job, so a retried pod takes its own lease again; a manual run by its
    // host, OS user, and process, so a second run of the same user is still refused
    if let Ok(job) = std::env::var(JOB_NAME_ENV)
        && !job.is_empty()
    {
        return job;
    }
    let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown-host".to_owned());
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown-user".to_owned());
    format!("{host}/{user}/{}", std::process::id())
}

/// Take the namespace's writer lease
///
/// # Arguments
///
/// * `env` - The backends
/// * `force` - Whether to break a lease another writer holds
/// * `offer_force` - Whether the command has a flag to break the lease, which a refusal names
pub async fn take_lease(env: &Env, force: bool, offer_force: bool) -> Result<WriterLease, Error> {
    // the namespace's lease in the connected Redis
    lease_in(
        &env.backends.redis,
        &env.backends.namespace,
        force,
        offer_force,
    )
    .await
}

/// Take a namespace's writer lease in a Redis pool
///
/// # Arguments
///
/// * `redis` - The Redis pool the lease lives in
/// * `namespace` - The namespace
/// * `force` - Whether to break a lease another writer holds
/// * `offer_force` - Whether the command has a flag to break the lease, which a refusal names
pub async fn lease_in(
    redis: &bb8_redis::bb8::Pool<bb8_redis::RedisConnectionManager>,
    namespace: &str,
    force: bool,
    offer_force: bool,
) -> Result<WriterLease, Error> {
    // a held lease is a refusal naming the holder, and the flag when the command has one; Redis
    // failing is worth retrying
    WriterLease::acquire(redis, namespace, &holder(), force)
        .await
        .map_err(|err| match err {
            LeaseError::Held { .. } if offer_force => {
                Error::new(format!("{err}, or take it over with --force-break-lease"))
            }
            LeaseError::Held { .. } => Error::new(err.to_string()),
            LeaseError::Backend(_) => Error::Transient(err.to_string()),
        })
}

/// Cancel a token when SIGTERM or SIGINT arrives, so the workers finish their batch, write their
/// checkpoints, and stop
#[must_use]
pub fn cancel_on_signal() -> CancellationToken {
    // a task waits for either signal and cancels the token once
    let cancel = CancellationToken::new();
    let token = cancel.clone();
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let (Ok(mut term), Ok(mut int)) = (
                signal(SignalKind::terminate()),
                signal(SignalKind::interrupt()),
            ) else {
                warn!("the signal handlers couldn't be installed");
                return;
            };
            tokio::select! {
                _ = term.recv() => {}
                _ = int.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        warn!("signal received; finishing the batch and writing the checkpoint");
        token.cancel();
    });
    cancel
}

/// Build a pass's spec from a registered migration, taking the group limit from the catalog step
/// that performs it when there is one
///
/// # Arguments
///
/// * `def` - The registered migration
/// * `accept` - The reason codes an approval accepted
/// * `backup` - The approval's backup reference, if any
pub fn spec_for(
    def: &MigrationDef,
    accept: impl IntoIterator<Item = String>,
    backup: Option<String>,
) -> PassSpec {
    // the catalog step's limit wins over the registry's default
    let group_max = super::step_for(def.id).map_or(def.group_max, |step| step.group_max);
    PassSpec {
        key: def.pass.to_owned(),
        migration: def.id.to_owned(),
        transform: def.chain.first().copied().unwrap_or(def.id).to_owned(),
        revision: super::revision_of(def.id),
        chain: def.chain.iter().map(|id| (*id).to_owned()).collect(),
        group_max,
        accept: accept.into_iter().collect(),
        job: std::env::var(JOB_NAME_ENV)
            .ok()
            .filter(|job| !job.is_empty()),
        backup,
        families: def
            .families
            .iter()
            .map(|family| (*family).to_owned())
            .collect(),
        has_views: def.has_views,
    }
}

/// Build the context of a run
///
/// # Arguments
///
/// * `env` - The backends
/// * `mode` - `Run` or `Check`
/// * `cancel` - Cancelled by SIGTERM
/// * `limits` - The run's bounds
#[must_use]
pub fn context(
    env: &Env,
    mode: RunMode,
    cancel: CancellationToken,
    limits: RunLimits,
) -> RunContext {
    // the backends are shared with the store
    let mut ctx = RunContext::new(mode, env.backends.clone(), cancel);
    ctx.limits = limits;
    ctx
}
