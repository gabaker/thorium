//! Setup Scylla for Thorium

use scylla::client::session::Session;
use std::time::Duration as StdDuration;

use crate::models::schema::queries::ScyllaQueries;
use crate::models::schema::statements::prepare_all;
use crate::models::schema::{
    ApplyOptions, ApplyTarget, DEFAULT_VIEW_WAIT_SECS, apply, check_startup, connect,
};
use crate::models::schema::{RevisionSnapshot, SchemaCheckError, SchemaCheckPass, SnapshotError};
use crate::models::upgrades::Catalog;
use crate::utils::{ApiError, StartupOpts};
use crate::{Conf, setup};
use axum::http::StatusCode;

/// Loads a revision's committed snapshot
pub type SnapshotLoader = fn(&str) -> Option<Result<RevisionSnapshot, SnapshotError>>;

/// The scylla client and prepared statments
pub struct Scylla {
    /// The scylla session object
    pub session: Session,
    /// The generated statements, one queries struct per table and view
    pub queries: ScyllaQueries,
}

impl Scylla {
    /// Connect to Scylla, check the applied schema against this build's catalog, and prepare
    /// every statement
    ///
    /// Issues no DDL unless `opts` asks the API to apply the schema first. A schema this build
    /// can't run on is returned as an error immediately, while connection failures are retried.
    ///
    /// # Arguments
    ///
    /// * `config` - The Thorium config
    /// * `opts` - The startup options
    pub async fn start(config: &Conf, opts: StartupOpts) -> Result<Self, SchemaCheckError> {
        // judge the applied schema against the catalog this build ships
        let (scylla, pass) = Self::start_with(
            config,
            opts,
            Catalog::PRODUCTION,
            RevisionSnapshot::committed,
        )
        .await?;
        // a later additive schema starts with one warning naming its revision
        if let Some(warning) = pass.warning() {
            println!(
                "{}",
                serde_json::json!({
                    "timestamp": chrono::Utc::now(),
                    "level": "WARN",
                    "msg": warning
                })
            );
        }
        Ok(scylla)
    }

    /// Connect to Scylla and prepare every generated statement, with no schema check and no DDL
    ///
    /// For a writer that runs inside an upgrade, where the schema is already applied and the
    /// startup check isn't the writer's to make.
    ///
    /// # Arguments
    ///
    /// * `config` - The Thorium config
    pub async fn offline(config: &Conf) -> Result<Self, ApiError> {
        // connect with the shared session settings
        let session = connect(config).await.map_err(|err| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                Some(format!("Scylla can't be reached: {err}")),
            )
        })?;
        // prepare the generated statements against the namespace's keyspace
        let queries = ScyllaQueries::prepare(&session, &config.thorium.namespace)
            .await
            .map_err(|err| {
                ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Some(format!("the Scylla statements didn't prepare: {err}")),
                )
            })?;
        Ok(Scylla { session, queries })
    }

    /// Connect to Scylla, judge the applied schema against a catalog, and prepare every
    /// statement
    ///
    /// Each of the four attempts gets the configured setup time. An attempt that runs out of time
    /// is aborted before the next one starts, so attempts never apply the schema at the same time.
    ///
    /// # Arguments
    ///
    /// * `config` - The Thorium config
    /// * `opts` - The startup options
    /// * `catalog` - The revisions this build knows
    /// * `snapshots` - Loads a revision's committed snapshot
    ///
    /// # Panics
    ///
    /// Panics when Scylla can't be connected to and set up in four attempts.
    pub async fn start_with(
        config: &Conf,
        opts: StartupOpts,
        catalog: Catalog,
        snapshots: SnapshotLoader,
    ) -> Result<(Self, SchemaCheckPass), SchemaCheckError> {
        // give each attempt the configured setup time
        let limit = StdDuration::from_secs(u64::from(config.scylla.setup_time));
        // loop and try to complete this future
        for _ in 0..4 {
            // get a clone of our config and log
            let config_clone = config.clone();
            // run the setup on its own task so a timed-out attempt can be aborted
            let mut setup_task = tokio::spawn(async move {
                Box::pin(build(config_clone, opts, catalog, snapshots)).await
            });
            // wait for the setup to finish or run out of time
            match tokio::time::timeout(limit, &mut setup_task).await {
                // a refusal is final, but a failed read of the record is retried
                Ok(Ok(Err(err))) if err.is_transient() => {
                    setup!(
                        config.thorium.tracing.local.level,
                        format!("Scylla setup error {err}")
                    );
                }
                // a started API or a final refusal is returned as is
                Ok(Ok(started)) => return started,
                // the setup task panicked, so print its error and try again
                Ok(Err(err)) => {
                    setup!(
                        config.thorium.tracing.local.level,
                        format!("Scylla setup error {err:#?}")
                    );
                }
                // the attempt ran out of time, so log it and try again
                Err(_) => {
                    setup!(
                        config.thorium.tracing.local.level,
                        format!(
                            "Failed to connect to scylla in {} seconds",
                            config.scylla.setup_time
                        )
                    );
                    // abort the attempt and wait for it to stop, so the next attempt never
                    // applies DDL beside it; whatever the stopped attempt returns is discarded
                    setup_task.abort();
                    let _ = setup_task.await;
                }
            }
        }
        // panic if we fail to connect
        panic!("Failed to connect/setup Scylla");
    }
}

/// Create a new session to scylla
///
/// # Arguments
///
/// * `config` - The thorium config
pub async fn new_session(config: &Conf) -> Session {
    // connecting to scylla
    setup!(
        config.thorium.tracing.local.level,
        format!("Connecting to scylla at {}", config.scylla.nodes.join(", "))
    );
    // log the user we authenticate as, if any
    if let Some(creds) = &config.scylla.auth {
        setup!(
            config.thorium.tracing.local.level,
            format!("Authenticating to Scylla as {}", creds.username)
        );
    }
    // connect with the shared session settings
    connect(config)
        .await
        .expect("Failed to build scylla session")
}

/// Apply the declared schema to the keyspace, for the API's dev-only apply flag
///
/// # Arguments
///
/// * `session` - The scylla session to apply with
/// * `config` - The Thorium config
async fn apply_declared_schema(session: &Session, config: &Conf) -> Result<(), SchemaCheckError> {
    // converge to the declared schema and record it, the way `thoradm schema apply` does, with
    // its default limit on the wait for views to build
    let options = ApplyOptions {
        target: ApplyTarget::Declared,
        dry_run: false,
        view_wait: Some(StdDuration::from_secs(DEFAULT_VIEW_WAIT_SECS)),
    };
    let progress = |line: &str| {
        setup!(config.thorium.tracing.local.level, line.to_owned());
    };
    // a refused data-shaping setting is final, and any other failure is retried
    apply(session, config, &options, "api --apply-schema", &progress)
        .await
        .map(|_| ())
        .map_err(|err| {
            SchemaCheckError::from_apply(&err).unwrap_or_else(|| {
                SchemaCheckError::Unreadable(format!("failed to apply the schema: {err}"))
            })
        })
}

/// Build a session, check the applied schema, and prepare the statements
///
/// # Arguments
///
/// * `config` - The Thorium config
/// * `opts` - The startup options
/// * `catalog` - The revisions this build knows
/// * `snapshots` - Loads a revision's committed snapshot
async fn build(
    config: Conf,
    opts: StartupOpts,
    catalog: Catalog,
    snapshots: SnapshotLoader,
) -> Result<(Scylla, SchemaCheckPass), SchemaCheckError> {
    // Create a new session for scylla
    let session = new_session(&config).await;
    // apply the declared schema first when asked to, which is the only DDL the API ever issues
    if opts.apply_schema {
        apply_declared_schema(&session, &config).await?;
    }
    // refuse to go on when the applied schema isn't one this build can run on
    let pass = check_startup(&session, &config, &catalog, &snapshots).await?;
    // fail before serving when any registered statement doesn't prepare, naming each one
    if let Err(err) = prepare_all(&session, &config.thorium.namespace).await {
        panic!("{err}");
    }
    // prepare the generated statements
    let queries = ScyllaQueries::prepare(&session, &config.thorium.namespace)
        .await
        .expect("Failed to prepare the generated scylla statements");
    // build our scylla client
    Ok((Scylla { session, queries }, pass))
}
