//! Setup Scylla for Thorium

use chrono::Duration;
use chrono::prelude::*;
use futures::{poll, task::Poll};
use scylla::client::session::Session;
use std::time::Duration as StdDuration;

use crate::models::schema::queries::ScyllaQueries;
use crate::models::schema::statements::prepare_all;
use crate::models::schema::{ApplyOptions, ApplyTarget, apply, check_startup, connect};
use crate::models::schema::{RevisionSnapshot, SchemaCheckError, SchemaCheckPass, SnapshotError};
use crate::models::upgrades::Catalog;
use crate::utils::StartupOpts;
use crate::{Conf, setup};

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

    /// Connect to Scylla, judge the applied schema against a catalog, and prepare every
    /// statement
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
        // loop and try to complete this future
        for _ in 0..4 {
            // get the correct timeout for scylla
            let timeout = Utc::now() + Duration::seconds(i64::from(config.scylla.setup_time));
            // get a clone of our config and log
            let config_clone = config.clone();
            // build the future for our setup
            let mut future = tokio::spawn(async move {
                Box::pin(build(config_clone, opts, catalog, snapshots)).await
            });
            // timeout appears to just hang so were going to check it manually
            loop {
                // check if this future has completed yet
                if let Poll::Ready(join_result) = poll!(&mut future) {
                    // if this future has errored out then panic with that error
                    match join_result {
                        // a refusal is final, but a failed read of the record is retried
                        Ok(Err(err)) if err.is_transient() => {
                            setup!(
                                config.thorium.tracing.local.level,
                                format!("Scylla setup error {err}")
                            );
                            break;
                        }
                        Ok(started) => return started,
                        // there was an error so print it and try again
                        Err(err) => {
                            // print our error and try again
                            setup!(
                                config.thorium.tracing.local.level,
                                format!("Scylla setup error {:#?}", err)
                            );
                            // try to connect to scylla again
                            break;
                        }
                    }
                }
                // check if we are past our timeout yet
                if Utc::now() > timeout {
                    setup!(
                        config.thorium.tracing.local.level,
                        format!(
                            "Failed to connect to scylla in {} seconds",
                            config.scylla.setup_time
                        )
                    );
                    break;
                }
                // sleep for 3 seconds
                tokio::time::sleep(StdDuration::from_millis(100)).await;
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
    // converge to the declared schema and record it, the way `thoradm schema apply` does
    let options = ApplyOptions {
        target: ApplyTarget::Declared,
        dry_run: false,
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
