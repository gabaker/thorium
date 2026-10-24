//! Tests of the derived Elastic rebuild in an upgrade: the search-streamer paused before the rebuild's
//! Job and kept paused until the rollout, the step reingesting after its Job, the `SearchIncomplete`
//! condition, the plan going on past the reingest, and the rebuild run again when the reingest was
//! lost, all against the stateful fake of the Kubernetes API

use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};
use thorium::models::elastic::ElasticSource;
use thorium::models::schema::RebuildProgress;
use thorium::models::upgrades::{
    Catalog, StepDef, StepKind, StepState, UpgradeComponent, UpgradeConditionType,
};

use super::{Scene, apply_deployment, check_report, edge, gated, run_summary};
use crate::k8s::clusters::ClusterMeta;
use crate::k8s::clusters::tests::meta_for;
use crate::upgrades::migration::NOW;
use crate::upgrades::pause::RESTORE_STEP;
use crate::upgrades::reingest::{self, PROGRESS_OVERRIDE};
use crate::upgrades::sim::Finish;

/// The id of the fixture step that runs the Elastic rebuild
const REBUILD: &str = "rebuild-search";

/// The components the fixture's data step pauses
const DATA_PAUSE: &[UpgradeComponent] = &[UpgradeComponent::Api, UpgradeComponent::Scaler];

/// Build the step that runs the Elastic rebuild, which pauses the search-streamer only
const fn rebuild_step() -> StepDef {
    // a data step with the rebuild's migration
    StepDef {
        id: REBUILD,
        kind: StepKind::Data,
        description: "A fixture Elastic rebuild",
        pause: &[UpgradeComponent::SearchStreamer],
        thoradm_migration: Some("elastic-rebuild"),
        manual_procedure: Some("Run `thoradm migrate run --id elastic-rebuild` by hand."),
        ..StepDef::BASE
    }
}

/// The rebuild, then a data step that runs after it
const REBUILD_THEN_CONTRACT: Catalog = Catalog {
    edges: &[
        edge(None, "2026-10-v01", &[]),
        edge(
            Some("2026-10-v01"),
            "2026-10-v02",
            &[rebuild_step(), gated("contract-v2", &[], &[])],
        ),
    ],
    floors: &[],
};

/// A data step that pauses the API, then the rebuild, then a data step that runs after both
const DATA_REBUILD_CONTRACT: Catalog = Catalog {
    edges: &[
        edge(None, "2026-10-v01", &[]),
        edge(
            Some("2026-10-v01"),
            "2026-10-v02",
            &[
                gated("users-v2", DATA_PAUSE, &[]),
                rebuild_step(),
                gated("contract-v2", &[], &[]),
            ],
        ),
    ],
    floors: &[],
};

/// Puts back what a test injected beside the catalog the scene resets
struct Reset;

impl Drop for Reset {
    /// Remove the progress and the time a test injected
    fn drop(&mut self) {
        // the thread may run another test
        PROGRESS_OVERRIDE.with(|progress| *progress.borrow_mut() = None);
        NOW.with(|now| *now.borrow_mut() = None);
    }
}

/// The approvals of the rebuild and the step after it
fn rebuild_approved() -> Value {
    // the approval an admin adds once a backup exists
    json!([
        {"step": REBUILD, "backup": "snap-1"},
        {"step": "contract-v2", "backup": "snap-1"}
    ])
}

/// The approvals of the data step, the rebuild, and the step after it
fn both_approved() -> Value {
    // each names the same backup
    json!([
        {"step": "users-v2", "backup": "snap-1"},
        {"step": REBUILD, "backup": "snap-1"},
        {"step": "contract-v2", "backup": "snap-1"}
    ])
}

/// What the check of the rebuild finds: two indexes to drop
fn rebuild_report() -> Value {
    // two of the four indexes are mapped differently
    check_report("elastic-rebuild", 2, &[], None)
}

/// Build the progress the post-rollout check observes
///
/// # Arguments
///
/// * `results_keys` - Whether the results source's init session keys exist
/// * `tags_keys` - Whether the tags source's init session keys exist
/// * `mismatched` - The indexes that are missing or mapped differently, by source
fn progress(
    results_keys: bool,
    tags_keys: bool,
    mismatched: &[(ElasticSource, &str)],
) -> Vec<RebuildProgress> {
    // one entry per source
    [
        (ElasticSource::Results, results_keys),
        (ElasticSource::Tags, tags_keys),
    ]
    .into_iter()
    .map(|(source, init_keys_present)| RebuildProgress {
        source,
        init_keys_present,
        mismatched: mismatched
            .iter()
            .filter(|(of, _)| *of == source)
            .map(|(_, index)| (*index).to_owned())
            .collect(),
    })
    .collect()
}

/// Make the post-rollout check observe this progress
///
/// # Arguments
///
/// * `observed` - What it observes
fn observe(observed: Vec<RebuildProgress>) {
    // the next check reads it instead of Elastic and Redis
    PROGRESS_OVERRIDE.with(|progress| *progress.borrow_mut() = Some(observed));
}

/// Build the cluster's metadata as the reconcile after the upgrade sees it
///
/// # Arguments
///
/// * `scene` - The scene
fn meta(scene: &Scene) -> ClusterMeta {
    // the cluster with the status the last reconcile left
    let mut cluster = scene.cluster.clone();
    cluster.status = Some(scene.sim.status());
    meta_for(cluster, &scene.sim.fake().client())
}

/// Run reconciles of the upgrade, ending each check Job with the rebuild's report and each
/// migration Job as done
///
/// # Arguments
///
/// * `scene` - The scene
/// * `rounds` - How many reconciles
async fn drive(scene: &Scene, rounds: usize) {
    // a reconcile creates or reads a Job, and the test ends the Jobs between reconciles
    let report = rebuild_report();
    for _ in 0..rounds {
        scene.reconcile().await;
        scene.finish_checks(&report);
        for job in scene.sim.active("migration") {
            scene
                .sim
                .finish(&job, &Finish::Complete(run_summary("done", 2, &[])));
        }
    }
    scene.reconcile().await;
}

/// Apply every Deployment the upgrade paused, as the rollout does
///
/// # Arguments
///
/// * `scene` - The scene
/// * `names` - The Deployments to apply
async fn roll_out(scene: &Scene, names: &[&str]) {
    // each Deployment is applied with the new release's template
    let meta = meta(scene);
    for name in names {
        apply_deployment(&meta, name).await;
    }
}

/// Get the conditions the status reports for an upgrade
///
/// # Arguments
///
/// * `scene` - The scene
/// * `kind` - The condition
fn condition(scene: &Scene, kind: UpgradeConditionType) -> Option<(String, String)> {
    // the status the operator last wrote
    scene
        .sim
        .status()
        .upgrade
        .and_then(|upgrade| {
            upgrade
                .conditions
                .into_iter()
                .find(|condition| condition.kind == kind)
        })
        .map(|condition| (condition.status, condition.message))
}

/// The search-streamer is scaled to zero and its pods are gone before the rebuild's Job is created,
/// the other components are left running, and when the Job ends the streamer is still at zero: it is
/// scaled back to its recorded count only when its Deployment from the new release is applied
#[tokio::test]
async fn the_streamer_is_paused_before_the_job_and_back_after_the_rollout() {
    // a streamer whose pods are slow to stop
    let _reset = Reset;
    let scene = Scene::new(
        REBUILD_THEN_CONTRACT,
        &rebuild_approved(),
        &[("api", 3), ("search-streamer", 2)],
    );
    scene.sim.stick("search-streamer");
    drive(&scene, 3).await;
    // the streamer was scaled down, the api was left alone, and no Job runs while pods are up
    assert_eq!(scene.sim.replicas("search-streamer"), Some(0));
    assert_eq!(scene.sim.replicas("api"), Some(3));
    assert_eq!(scene.sim.created_of("migration").len(), 0);
    // once its pods are gone the Job is created
    scene.sim.unstick("search-streamer");
    scene.rounds(2).await;
    let jobs = scene.sim.created_of("migration");
    assert_eq!(jobs.len(), 1);
    assert_eq!(
        &Scene::args(&jobs[0])[..4],
        ["migrate", "run", "--id", "elastic-rebuild"]
    );
    // the count to restore is recorded on the step's `Running` entry
    let running = scene
        .history(REBUILD)
        .into_iter()
        .find(|entry| entry.outcome == StepState::Running)
        .expect("a running entry");
    assert_eq!(running.paused.get("search-streamer"), Some(&2));
    // the Job ends, the plan goes on, and the streamer is still at zero
    drive(&scene, 3).await;
    assert_eq!(scene.sim.replicas("search-streamer"), Some(0));
    let ended = scene.history(REBUILD).pop().expect("an entry");
    assert_eq!(ended.outcome, StepState::Reingesting);
    // applying the streamer's new Deployment patches it at zero and then scales it to its count
    let before = scene.sim.scaled().len();
    roll_out(&scene, &["search-streamer"]).await;
    assert_eq!(
        scene.sim.scaled()[before..].to_vec(),
        [
            ("search-streamer".to_owned(), 0),
            ("search-streamer".to_owned(), 2)
        ]
    );
    assert_eq!(scene.sim.replicas("search-streamer"), Some(2));
}

/// The step is `Reingesting` when its Job ends and `Done` only when no init session keys are left
/// and the probe sees the expected mappings; the `SearchIncomplete` condition names the sources
/// still reingesting until then and is cleared with it
#[tokio::test]
async fn the_step_is_done_only_when_the_reingest_finished() {
    // an upgrade whose rebuild Job ended and whose Deployments were applied
    let _reset = Reset;
    let scene = Scene::new(
        REBUILD_THEN_CONTRACT,
        &rebuild_approved(),
        &[("api", 3), ("search-streamer", 1)],
    );
    drive(&scene, 6).await;
    roll_out(&scene, &["search-streamer"]).await;
    assert_eq!(
        scene.history(REBUILD).pop().expect("entry").outcome,
        StepState::Reingesting
    );
    // the results source is still reingesting: its session is in Redis
    observe(progress(true, false, &[]));
    let meta = meta(&scene);
    reingest::track(&meta).await;
    let (status, message) =
        condition(&scene, UpgradeConditionType::SearchIncomplete).expect("a condition");
    assert_eq!(status, "True");
    assert!(message.contains("Results"), "{message}");
    assert!(!message.contains("Tags"), "{message}");
    assert_eq!(
        scene.history(REBUILD).pop().expect("entry").outcome,
        StepState::Reingesting
    );
    // both sources reingesting are named
    observe(progress(true, true, &[]));
    reingest::track(&meta).await;
    let (_, message) =
        condition(&scene, UpgradeConditionType::SearchIncomplete).expect("a condition");
    assert!(
        message.contains("Results") && message.contains("Tags"),
        "{message}"
    );
    // the sessions are gone and every index is as declared: the step is done and the condition clears
    observe(progress(false, false, &[]));
    reingest::track(&meta).await;
    let ended = scene.history(REBUILD).pop().expect("entry");
    assert_eq!(ended.outcome, StepState::Done);
    let (status, _) = condition(&scene, UpgradeConditionType::SearchIncomplete).expect("cleared");
    assert_eq!(status, "False");
    // nothing more is recorded once it is done
    let recorded = scene.sim.state().expect("state").applied.len();
    reingest::track(&meta).await;
    assert_eq!(scene.sim.state().expect("state").applied.len(), recorded);
}

/// In a plan with a data step pausing the API, then a rebuild, then a later step, the later step runs,
/// the revision is recorded, and the API is scaled up after the rollout while the rebuild is still
/// reingesting
#[tokio::test]
async fn a_later_step_and_the_apis_return_do_not_wait_for_the_reingest() {
    // a data step pausing the api and the scaler, the rebuild, and a contract step
    let _reset = Reset;
    let scene = Scene::new(
        DATA_REBUILD_CONTRACT,
        &both_approved(),
        &[("api", 3), ("scaler", 1), ("search-streamer", 1)],
    );
    drive(&scene, 12).await;
    // every step ran, and the revision is recorded
    let state = scene.sim.state().expect("state");
    assert_eq!(state.revision.to_string(), "2026-10-v02");
    assert!(
        state
            .applied
            .iter()
            .any(|entry| entry.step == "contract-v2" && entry.outcome == StepState::Done)
    );
    // the rebuild is reingesting and holds nothing
    assert_eq!(
        scene.history(REBUILD).pop().expect("entry").outcome,
        StepState::Reingesting
    );
    // the paused components are still at zero until their Deployments are applied
    assert_eq!(scene.sim.replicas("api"), Some(0));
    assert_eq!(scene.sim.replicas("search-streamer"), Some(0));
    // the rollout restores the api while the init session is still running
    observe(progress(true, false, &[]));
    roll_out(&scene, &["api"]).await;
    assert_eq!(scene.sim.replicas("api"), Some(3));
    let meta = meta(&scene);
    reingest::track(&meta).await;
    assert_eq!(
        scene.history(REBUILD).pop().expect("entry").outcome,
        StepState::Reingesting
    );
    assert_eq!(scene.sim.replicas("search-streamer"), Some(0));
    // the streamer comes back with its own Deployment, and the api is unchanged by it
    roll_out(&scene, &["search-streamer"]).await;
    assert_eq!(scene.sim.replicas("search-streamer"), Some(1));
    assert_eq!(scene.sim.replicas("api"), Some(3));
    let (status, _) =
        condition(&scene, UpgradeConditionType::SearchIncomplete).expect("a condition");
    assert_eq!(status, "True");
}

/// After the rollout, with the sessions gone and an index missing or mapped differently, the check
/// pauses only the search-streamer, runs the rebuild again under the same approval, and scales the
/// streamer back up; the API's replicas don't change
#[tokio::test]
async fn a_lost_reingest_runs_the_rebuild_again_pausing_only_the_streamer() {
    // an upgrade whose rebuild finished and whose rollout is done
    let _reset = Reset;
    let scene = Scene::new(
        DATA_REBUILD_CONTRACT,
        &both_approved(),
        &[("api", 3), ("scaler", 1), ("search-streamer", 2)],
    );
    drive(&scene, 12).await;
    roll_out(&scene, &["api", "scaler", "search-streamer"]).await;
    assert_eq!(scene.sim.replicas("search-streamer"), Some(2));
    let first_jobs = scene.sim.jobs("migration").len();
    // the sessions are gone but an index is missing: Elastic auto-created it on a write, or it was
    // never created
    observe(progress(
        false,
        false,
        &[(ElasticSource::Results, "SampleResults")],
    ));
    let start: DateTime<Utc> = "2026-10-23T12:00:00Z".parse().expect("a time");
    NOW.with(|now| *now.borrow_mut() = Some(start));
    let meta = meta(&scene);
    reingest::track(&meta).await;
    // a mismatch seen once may be the streamer not having started its init yet, so nothing runs
    assert_eq!(scene.sim.jobs("migration").len(), first_jobs);
    let (_, message) =
        condition(&scene, UpgradeConditionType::SearchIncomplete).expect("a condition");
    assert!(message.contains("SampleResults"), "{message}");
    // seen again after the streamer had time to start, it is lost: the streamer is paused
    NOW.with(|now| *now.borrow_mut() = Some(start + Duration::minutes(30)));
    let api_before = scene.sim.replicas("api");
    reingest::track(&meta).await;
    assert_eq!(scene.sim.replicas("search-streamer"), Some(0));
    assert_eq!(scene.sim.replicas("api"), api_before);
    assert_eq!(scene.sim.replicas("scaler"), Some(1));
    // and the Job runs under the same approval, with only the streamer's count recorded
    reingest::track(&meta).await;
    let jobs = scene.sim.created_of("migration");
    assert_eq!(jobs.len(), first_jobs + 1, "{jobs:#?}");
    let job = jobs.last().expect("a job");
    assert_eq!(
        &Scene::args(job)[..4],
        ["migrate", "run", "--id", "elastic-rebuild"]
    );
    assert_eq!(Scene::flag(job, "--backup").as_deref(), Some("snap-1"));
    let running = scene
        .history(REBUILD)
        .into_iter()
        .rfind(|entry| entry.outcome == StepState::Running)
        .expect("a running entry");
    assert_eq!(running.paused.len(), 1);
    assert_eq!(running.paused.get("search-streamer"), Some(&2));
    // the Job ends: the streamer is scaled back up and the step goes on reingesting
    for job in scene.sim.active("migration") {
        scene
            .sim
            .finish(&job, &Finish::Complete(run_summary("done", 2, &[])));
    }
    reingest::track(&meta).await;
    assert_eq!(scene.sim.replicas("search-streamer"), Some(2));
    assert_eq!(scene.sim.replicas("api"), api_before);
    let state = scene.sim.state().expect("state");
    assert!(state.applied.iter().any(|entry| entry.step == RESTORE_STEP
        && entry.paused.contains_key("search-streamer")));
    assert_eq!(
        scene.history(REBUILD).pop().expect("entry").outcome,
        StepState::Reingesting
    );
    // the new reingest finishes and the step is done
    observe(progress(false, false, &[]));
    reingest::track(&meta).await;
    assert_eq!(
        scene.history(REBUILD).pop().expect("entry").outcome,
        StepState::Done
    );
}

/// While the search-streamer is still paused waiting for the rollout, a missing index is expected, so
/// the check never runs the rebuild again
#[tokio::test]
async fn nothing_runs_again_while_the_streamer_waits_for_the_rollout() {
    // a rebuild that finished with its Deployments not applied yet
    let _reset = Reset;
    let scene = Scene::new(
        REBUILD_THEN_CONTRACT,
        &rebuild_approved(),
        &[("search-streamer", 1)],
    );
    drive(&scene, 6).await;
    assert_eq!(scene.sim.replicas("search-streamer"), Some(0));
    let jobs = scene.sim.jobs("migration").len();
    // every index is missing and no session exists, however long it has been so
    observe(progress(
        false,
        false,
        &[
            (ElasticSource::Results, "SampleResults"),
            (ElasticSource::Tags, "SampleTags"),
        ],
    ));
    let start: DateTime<Utc> = "2026-10-23T12:00:00Z".parse().expect("a time");
    let meta = meta(&scene);
    for minutes in [0, 30, 60] {
        NOW.with(|now| *now.borrow_mut() = Some(start + Duration::minutes(minutes)));
        reingest::track(&meta).await;
    }
    assert_eq!(scene.sim.jobs("migration").len(), jobs);
    assert_eq!(scene.sim.replicas("search-streamer"), Some(0));
    // search is reported incomplete, and why
    let (status, message) =
        condition(&scene, UpgradeConditionType::SearchIncomplete).expect("a condition");
    assert_eq!(status, "True");
    assert!(message.contains("paused"), "{message}");
}

/// Read the progress from a real Elasticsearch and Redis: an index with the declared mappings and a
/// source's init keys are seen, a deleted index is a mismatch, and a source whose keys are gone is
/// not reingesting
#[tokio::test]
#[ignore = "creates and deletes Elasticsearch indexes named with a run stamp and sets Redis keys: \
            set THORIUM__ELASTIC__NODE, THORIUM__REDIS__HOST and THORIUM__REDIS__PORT to a \
            throwaway Elasticsearch (no security) and Redis, then run `cargo test -p \
            thorium-operator reingest_tests::live -- --ignored`"]
async fn live_progress_reads_elastic_and_redis() {
    // the namespace and the indexes are named with the run's stamp
    let stamp = format!(
        "rg-{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("a time")
            .as_nanos()
    );
    let cluster = crate::upgrades::tests::chart_cluster(json!({}));
    let sim = crate::upgrades::sim::Sim::new(&cluster);
    let mut meta = meta_for(cluster, &sim.fake().client());
    // point the config at the throwaway backends
    meta.conf.elastic.node = std::env::var("THORIUM__ELASTIC__NODE").expect("an Elastic node");
    meta.conf.redis.host = std::env::var("THORIUM__REDIS__HOST").expect("a Redis host");
    meta.conf.redis.port = std::env::var("THORIUM__REDIS__PORT")
        .expect("a Redis port")
        .parse()
        .expect("a port");
    meta.conf.redis.password = None;
    meta.conf.redis.username = None;
    meta.conf.elastic.insecure_certificates = true;
    meta.conf.thorium.namespace = stamp.clone();
    meta.conf.elastic.results.samples = format!("{stamp}-sample-results");
    meta.conf.elastic.results.repos = format!("{stamp}-repo-results");
    meta.conf.elastic.tags.samples = format!("{stamp}-sample-tags");
    meta.conf.elastic.tags.repos = format!("{stamp}-repo-tags");
    let conf = meta.conf.elastic.clone();
    let client = thorium::models::schema::elastic_client(&conf)
        .await
        .expect("Elastic is reachable");
    // every index with the mappings Thorium declares
    for index in thorium::models::elastic::ElasticIndex::ALL {
        let response = client
            .indices()
            .create(elasticsearch::indices::IndicesCreateParts::Index(
                index.full_name(&conf),
            ))
            .body(index.create_body(&conf))
            .send()
            .await
            .expect("the index creates");
        assert!(response.status_code().is_success());
    }
    // the results source has a session
    let redis = redis::Client::open(format!(
        "redis://{}:{}/",
        meta.conf.redis.host, meta.conf.redis.port
    ))
    .expect("a client");
    let mut connection = redis
        .get_multiplexed_async_connection()
        .await
        .expect("Redis is reachable");
    for key in ElasticSource::Results.init_keys(&stamp) {
        let _: () = redis::cmd("SET")
            .arg(key)
            .arg("session")
            .query_async(&mut connection)
            .await
            .expect("a key sets");
    }
    let found = reingest::read_live(&meta).await.expect("progress reads");
    assert_eq!(found, progress(true, false, &[]));
    // a deleted index is a mismatch of its source, and a source without keys isn't reingesting
    let _ = client
        .indices()
        .delete(elasticsearch::indices::IndicesDeleteParts::Index(&[
            thorium::models::elastic::ElasticIndex::SampleTags.full_name(&conf),
        ]))
        .send()
        .await;
    for key in ElasticSource::Results.init_keys(&stamp) {
        let _: i64 = redis::cmd("DEL")
            .arg(key)
            .query_async(&mut connection)
            .await
            .expect("a key deletes");
    }
    let found = reingest::read_live(&meta).await.expect("progress reads");
    assert_eq!(
        found,
        progress(false, false, &[(ElasticSource::Tags, "SampleTags")])
    );
    // clean up the indexes the test made
    for index in thorium::models::elastic::ElasticIndex::ALL {
        let _ = client
            .indices()
            .delete(elasticsearch::indices::IndicesDeleteParts::Index(&[
                index.full_name(&conf)
            ]))
            .send()
            .await;
    }
}
