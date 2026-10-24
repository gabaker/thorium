//! Following a derived rebuild's reingest after the rollout: the `SearchIncomplete` condition, the
//! step's `Done` entry once the search-streamer finished, and the rebuild run again when the
//! reingest was lost
//!
//! A rebuild's Job drops Elastic's indexes and the init session keys while the search-streamer is
//! paused, and the streamer stays paused until the new release's Deployments are applied. The step
//! then stays `Reingesting` while the streamer, running the new image, recreates the indexes and
//! reingests from Scylla. Nothing in the plan waits for that. Each reconcile after the rollout reads
//! Elastic and Redis directly: when no init session is left and every index has its declared
//! mappings the step is done, and when the sessions are gone but an index is missing or mapped
//! differently (Elastic auto-created it on a write, or the session was lost) the rebuild runs again
//! under the step's approval, pausing only the search-streamer.

use chrono::{DateTime, Duration, Utc};
use redis::AsyncConnectionConfig;
use std::time::Duration as StdDuration;
use thorium::Error;
use thorium::models::elastic::ElasticSource;
use thorium::models::schema::{RebuildProgress, elastic_client, probe_elastic};
use thorium::models::upgrades::{
    AppliedStep, PlannedStep, StepDef, StepState, UpgradeComponent, UpgradeConditionType,
};

use super::migration::{self, Advance};
use super::state::{self, StoredState};
use super::{OPERATOR_VERSION, Pending, catalog, pause};
use crate::k8s::clusters::ClusterMeta;
use crate::k8s::crds;

/// The key in the state's schema markers holding when a lost reingest was first seen
const LOST_MARKER: &str = "reingest_lost";

/// How long in seconds a reingest must look lost before the rebuild runs again
///
/// Right after the rollout the streamer hasn't started its init yet, so every index is missing and
/// no session exists; only a streamer that had this long to create them is judged to have lost the
/// reingest.
const LOST_AFTER_SECS: i64 = 300;

/// How long in seconds to wait before reading the reingest again
const READ_REQUEUE_SECS: u64 = 60;

/// How long in seconds to wait before checking on a rebuild Job that runs again
const RUN_REQUEUE_SECS: u64 = 15;

/// How long in seconds to wait before trying again after a read or a run failed
const RETRY_SECS: u64 = 120;

/// How long a read of Redis or Elastic may take
const READ_TIMEOUT: StdDuration = StdDuration::from_secs(10);

#[cfg(test)]
thread_local! {
    /// The progress tests give the check on their thread instead of what Elastic and Redis hold
    pub(crate) static PROGRESS_OVERRIDE: std::cell::RefCell<Option<Vec<RebuildProgress>>> =
        const { std::cell::RefCell::new(None) };
}

/// Read each source's progress from Elastic and Redis
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
pub(super) async fn read_live(meta: &ClusterMeta) -> Result<Vec<RebuildProgress>, Error> {
    // the mappings of every index that exists, as the API's own probe reads them
    let conf = &meta.conf.elastic;
    let client = elastic_client(conf)
        .await
        .map_err(|error| Error::new(format!("Elastic can't be reached: {error}")))?;
    let observed = probe_elastic(&client, conf)
        .await
        .map_err(|error| Error::new(format!("Elastic's mappings can't be read: {error}")))?;
    // a connection to Redis for the init session keys
    let redis = redis::Client::open(crate::app::checks::redis_info(meta)?)
        .map_err(|error| Error::new(format!("Redis can't be configured: {error}")))?;
    let config = AsyncConnectionConfig::new()
        .set_connection_timeout(READ_TIMEOUT)
        .set_response_timeout(READ_TIMEOUT);
    let mut connection = redis
        .get_multiplexed_async_connection_with_config(&config)
        .await
        .map_err(|error| Error::new(format!("Redis can't be reached: {error}")))?;
    // each source's session exists while either of its keys does
    let mut progress = Vec::new();
    for source in ElasticSource::ALL {
        let keys = source.init_keys(&meta.conf.thorium.namespace);
        let present: i64 = redis::cmd("EXISTS")
            .arg(&keys[..])
            .query_async(&mut connection)
            .await
            .map_err(|error| Error::new(format!("Redis can't be read: {error}")))?;
        progress.push(RebuildProgress::observe(source, present > 0, &observed));
    }
    Ok(progress)
}

/// Read each source's progress: what a test injected, or what Elastic and Redis hold
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
async fn read(meta: &ClusterMeta) -> Result<Vec<RebuildProgress>, Error> {
    // a test's injected progress stands in for the backends
    #[cfg(test)]
    if let Some(progress) = PROGRESS_OVERRIDE.with(|progress| progress.borrow().clone()) {
        return Ok(progress);
    }
    read_live(meta).await
}

/// Report whether search results are incomplete, and why
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `holds` - Whether results are incomplete
/// * `message` - What an admin should know
async fn report(meta: &ClusterMeta, holds: bool, message: String) {
    // the status carries it as the `SearchIncomplete` condition
    crds::set_upgrade_condition(
        &meta.client,
        &meta.status,
        UpgradeConditionType::SearchIncomplete,
        holds,
        message,
    )
    .await;
}

/// Follow the reingest of every rebuild step that is still reingesting
///
/// A failure is logged and tried again after a while, and never holds the reconcile. Returns how soon
/// in seconds to look again, or nothing when no step is reingesting.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
pub async fn track(meta: &ClusterMeta) -> Option<u64> {
    // the reingest is advice and work beside the reconcile: whatever goes wrong here never fails it
    match try_track(meta).await {
        Ok(next) => next,
        Err(error) => {
            println!(
                "Warning: following the reingest of {} failed: {error}",
                meta.name
            );
            Some(RETRY_SECS)
        }
    }
}

/// Mark the steps whose reingest finished as done and clear the condition
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `stored` - The recorded upgrade state, which gains the entries
/// * `steps` - The steps whose reingest finished, as `(revision, step)`
/// * `observed` - What the check observed for each source
async fn finish(
    meta: &ClusterMeta,
    stored: &mut StoredState,
    steps: &[(String, String)],
    observed: &[RebuildProgress],
) -> Result<(), Error> {
    // each step gets an entry saying its reingest finished, for every source it covers
    let sources = observed
        .iter()
        .map(|progress| progress.source.name())
        .collect::<Vec<_>>()
        .join(", ");
    for (revision, step) in steps {
        stored.state.record(AppliedStep {
            revision: revision.clone(),
            step: step.clone(),
            outcome: StepState::Done,
            message: Some(format!(
                "The search-streamer finished reingesting {sources}: no init session is left and \
                 every index has its declared mappings"
            )),
            at: Some(migration::now().to_rfc3339()),
            by: Some(format!("thorium-operator {OPERATOR_VERSION}")),
            ..AppliedStep::default()
        });
    }
    // a reingest that finished leaves nothing to be judged lost
    stored.state.schema.remove(LOST_MARKER);
    state::save(&meta.client, &meta.namespace, stored).await?;
    report(meta, false, "Search results are complete".to_owned()).await;
    Ok(())
}

/// Record when a reingest was first seen lost, or that it no longer looks lost
///
/// Returns whether the state changed, and how long the reingest has looked lost.
///
/// # Arguments
///
/// * `stored` - The recorded upgrade state
/// * `lost` - Whether the reingest looks lost now
fn note_lost(stored: &mut StoredState, lost: bool) -> (bool, Option<Duration>) {
    // a reingest that doesn't look lost forgets when it did
    if !lost {
        return (stored.state.schema.remove(LOST_MARKER).is_some(), None);
    }
    // the first sighting is remembered, and later ones measure from it
    let now = migration::now();
    let since = stored
        .state
        .schema
        .get(LOST_MARKER)
        .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
        .map(|at| at.with_timezone(&Utc));
    match since {
        Some(since) => (false, Some(now - since)),
        None => {
            stored
                .state
                .schema
                .insert(LOST_MARKER.to_owned(), now.to_rfc3339());
            (true, Some(Duration::zero()))
        }
    }
}

/// Run the rebuild again, or follow its Job: pause only the search-streamer, run the step's
/// migration under its approval, and scale the streamer back up once the Job ends
///
/// Returns how soon in seconds to look again.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `stored` - The recorded upgrade state
/// * `revision` - The revision of the rebuild step
/// * `def` - The rebuild step
async fn rerun(
    meta: &ClusterMeta,
    stored: StoredState,
    revision: &str,
    def: &'static StepDef,
) -> Result<Option<u64>, Error> {
    // the upgrade the step ran in is long over, so a one step upgrade stands in for it
    let step = PlannedStep {
        revision: revision.to_owned(),
        step: def.id.to_owned(),
        kind: def.kind,
        description: def.description.to_owned(),
        pause: def.pause.to_vec(),
        state: StepState::Running,
        message: None,
        phase: def.phase,
        prerelease: false,
        reverse: false,
        pass: None,
        blocked: None,
        loss: None,
    };
    let mut pending = Pending {
        target: stored.state.revision.clone(),
        stored,
        steps: vec![step],
        approvals: meta.cluster.spec.upgrade.approvals.clone(),
        requeue_hint: None,
    };
    // run it again without a check, which pauses the components the step names and creates its Job
    match migration::advance_with(meta, &mut pending, 0, def, false).await? {
        // the Job ended: record it, and let the streamer come back on the image that runs now
        Advance::Finished(entry) => {
            pending.stored.state.record(*entry);
            pending.stored.state.schema.remove(LOST_MARKER);
            state::save(&meta.client, &meta.namespace, &mut pending.stored).await?;
            for component in def.pause {
                pause::restore(meta, component.deployment_name()).await?;
            }
            report(
                meta,
                true,
                "Search results are incomplete: the rebuild of the Elastic indexes ran again and \
                 the search-streamer is reingesting"
                    .to_owned(),
            )
            .await;
            Ok(Some(READ_REQUEUE_SECS))
        }
        // the Job or the pause is in progress, or the step is held on something
        Advance::Held { state, message } => {
            report(
                meta,
                true,
                format!(
                    "Search results are incomplete: the rebuild of the Elastic indexes is running \
                     again ({state:?}): {message}"
                ),
            )
            .await;
            Ok(Some(RUN_REQUEUE_SECS))
        }
        // a run again has no check to say there was nothing to change
        Advance::Skipped(reason) => {
            report(
                meta,
                true,
                format!("Search results may be incomplete: {reason}"),
            )
            .await;
            Ok(Some(READ_REQUEUE_SECS))
        }
    }
}

/// Follow the reingest of every rebuild step that is still reingesting
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
async fn try_track(meta: &ClusterMeta) -> Result<Option<u64>, Error> {
    // without a recorded state there is no upgrade whose reingest could be followed
    let Some(mut stored) = state::load(&meta.client, &meta.namespace).await? else {
        return Ok(None);
    };
    // the steps still reingesting, as `(revision, step)`
    let catalog = catalog();
    let steps = catalog
        .reingesting(&stored.state)
        .into_iter()
        .map(|entry| (entry.revision.clone(), entry.step.clone()))
        .collect::<Vec<_>>();
    // nothing is reingesting, so results are complete
    let Some((revision, id)) = steps.first().cloned() else {
        report(meta, false, "Search results are complete".to_owned()).await;
        return Ok(None);
    };
    let Some((_, def)) = catalog.step(&id) else {
        return Err(Error::new(format!("Step {id} is not in the catalog")));
    };
    // a rebuild that runs again is followed until its Job ends
    if migration::open_run(&stored.state, &id).is_some() {
        return rerun(meta, stored, &revision, def).await;
    }
    // while the search-streamer waits for the rollout every index is missing, which is expected
    let streamer = UpgradeComponent::SearchStreamer.deployment_name();
    if pause::paused(&stored.state).contains_key(streamer) {
        let message = "Search results are incomplete: the search-streamer stays paused until the \
                       new release's Deployments are applied, and then reingests the dropped indexes"
            .to_owned();
        report(meta, true, message).await;
        return Ok(Some(READ_REQUEUE_SECS));
    }
    // read what Elastic and Redis hold now
    let observed = read(meta).await?;
    if observed.iter().all(RebuildProgress::finished) {
        finish(meta, &mut stored, &steps, &observed).await?;
        return Ok(None);
    }
    // a reingest with no session and a missing or differently mapped index may have been lost
    let lost = observed.iter().any(RebuildProgress::lost);
    let (changed, for_how_long) = note_lost(&mut stored, lost);
    if changed {
        state::save(&meta.client, &meta.namespace, &mut stored).await?;
    }
    // name each source that isn't finished
    let pending = observed
        .iter()
        .filter(|progress| !progress.finished())
        .map(RebuildProgress::describe)
        .collect::<Vec<_>>()
        .join("; ");
    // long enough lost, and approved, runs the rebuild again
    let lost_for_good = for_how_long.is_some_and(|waited| waited.num_seconds() >= LOST_AFTER_SECS);
    if lost_for_good {
        if migration::usable(def, &meta.cluster.spec.upgrade.approvals).is_some() {
            return rerun(meta, stored, &revision, def).await;
        }
        let message = format!(
            "Search results are incomplete: {pending}. The reingest looks lost; approve step {id} \
             with a backup in spec.upgrade.approvals to run the rebuild again"
        );
        report(meta, true, message).await;
        return Ok(Some(READ_REQUEUE_SECS));
    }
    report(
        meta,
        true,
        format!("Search results are incomplete: the search-streamer is reingesting {pending}"),
    )
    .await;
    Ok(Some(READ_REQUEUE_SECS))
}
