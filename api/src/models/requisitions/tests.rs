//! Tests for requisitions and the worker records built from them

use chrono::Utc;
use std::collections::{BTreeMap, HashMap, HashSet};
use uuid::Uuid;

use super::*;
use crate::models::{
    GroupStats, ImageScaler, StageStats, SystemStats, WorkerRegistration, WorkerStatus,
    WorkloadRuntimes,
};

/// The tool instance most requisitions in these tests run
const CORN: Uuid = Uuid::from_u128(0xc0ff_ee00);

/// The run most per-run requisitions in these tests serve
const RUN: Uuid = Uuid::from_u128(0x7275_6e00);

/// Build a deadline for a job of `alice` in `corn`
///
/// # Arguments
///
/// * `tool_instance` - The tool instance the job runs
/// * `run` - The run the job belongs to, if any
fn deadline(tool_instance: Uuid, run: Option<Uuid>) -> Deadline {
    Deadline {
        group: "corn".to_owned(),
        pipeline: "harvest".to_owned(),
        stage: "husk".to_owned(),
        creator: "alice".to_owned(),
        job_id: Uuid::new_v4(),
        reaction: Uuid::new_v4(),
        deadline: Utc::now(),
        tool_instance,
        run,
    }
}

/// Build a worker serving `alice` in `corn`
///
/// # Arguments
///
/// * `run` - The run the worker is limited to, if any
fn worker(run: Option<Uuid>) -> Worker {
    Worker {
        cluster: "cluster0".to_owned(),
        node: "node0".to_owned(),
        scaler: ImageScaler::K8s,
        name: "corn-worker".to_owned(),
        user: "alice".to_owned(),
        group: "corn".to_owned(),
        tool_instance: CORN,
        run,
        status: WorkerStatus::Running,
        spawned: Utc::now(),
        heart_beat: None,
        resources: Resources::default(),
        pool: Pools::Deadline,
        active: None,
    }
}

/// Serialize a value to JSON and back
///
/// # Arguments
///
/// * `value` - The value to round trip
fn round_trip<T: Serialize + for<'de> Deserialize<'de>>(value: &T) -> T {
    // serialize and deserialize the value
    let raw = serde_json::to_string(value).unwrap();
    serde_json::from_str(&raw).unwrap()
}

/// Requisitions, workers, worker registrations, and workload runtimes round trip through JSON
/// with and without a run, and workers and registrations built from a worker keep its tool
/// instance and run
#[test]
fn scheduling_types_round_trip() {
    for run in [None, Some(RUN)] {
        // requisitions and scoped requisitions keep their tool instance and run
        let req = Requisition::new("alice", "corn", CORN, run);
        assert_eq!(round_trip(&req), req);
        let scoped = req.clone().to_scoped("node1");
        assert_eq!(round_trip(&scoped), scoped);
        assert_eq!(scoped.req, req);
        // workers and worker registrations keep their tool instance and run
        let worker = worker(run);
        let read = round_trip(&worker);
        assert_eq!(read.tool_instance, CORN);
        assert_eq!(read.run, run);
        let registration = WorkerRegistration::from(worker.clone());
        assert_eq!(registration.tool_instance, CORN);
        assert_eq!(registration.run, run);
        let read = round_trip(&registration);
        assert_eq!(read.tool_instance, CORN);
        assert_eq!(read.run, run);
        // a spawned update rebuilds the worker's requisition
        let update = SpawnedUpdate::from(worker);
        assert_eq!(update.req, req);
    }
    // a worker or registration stored without a run reads as a shared worker
    let mut raw = serde_json::to_value(worker(Some(RUN))).unwrap();
    raw.as_object_mut().unwrap().remove("run");
    let read: Worker = serde_json::from_value(raw).unwrap();
    assert_eq!(read.run, None);
    let mut raw = serde_json::to_value(WorkerRegistration::from(worker(Some(RUN)))).unwrap();
    raw.as_object_mut().unwrap().remove("run");
    let read: WorkerRegistration = serde_json::from_value(raw).unwrap();
    assert_eq!(read.run, None);
    // workload runtimes keep each tool instance's runtime
    let mut runtimes = WorkloadRuntimes::default();
    runtimes.runtimes.insert(CORN, 42.5);
    assert_eq!(round_trip(&runtimes).runtimes, runtimes.runtimes);
}

/// Requisitions are built from deadlines and pod labels with their tool instance and run, print
/// and parse as colon joined strings, and match only their own label maps
#[test]
fn requisition_conversions() {
    // a requisition from a deadline copies its tool instance and run
    let shared = Requisition::from(deadline(CORN, None));
    assert_eq!(shared, Requisition::new("alice", "corn", CORN, None));
    let per_run = Requisition::from(&deadline(CORN, Some(RUN)));
    assert_eq!(per_run, Requisition::new("alice", "corn", CORN, Some(RUN)));
    // the requisition rebuilt from its own labels is the same requisition
    assert_eq!(Requisition::from_labels(&shared.labels()).unwrap(), shared);
    assert_eq!(
        Requisition::from_labels(&per_run.labels()).unwrap(),
        per_run
    );
    // a shared requisition has no run label
    assert!(!shared.labels().contains_key(RUN_LABEL));
    assert_eq!(per_run.labels()[RUN_LABEL], RUN.to_string());
    assert_eq!(shared.labels()[TOOL_INSTANCE_LABEL], CORN.to_string());
    // the colon joined strings name the run only for per-run requisitions
    assert_eq!(shared.to_string(), format!("alice:corn:{CORN}"));
    assert_eq!(per_run.to_string(), format!("alice:corn:{CORN}:run:{RUN}"));
    let scoped_shared = shared.clone().to_scoped("node1");
    let scoped_run = per_run.clone().to_scoped("node1");
    assert_eq!(
        scoped_shared.to_string(),
        format!("node1:alice:corn:{CORN}")
    );
    assert_eq!(
        scoped_run.to_string(),
        format!("node1:alice:corn:{CORN}:run:{RUN}")
    );
    // all four strings parse back
    assert_eq!(Requisition::try_from(&shared.to_string()).unwrap(), shared);
    assert_eq!(
        Requisition::try_from(&per_run.to_string()).unwrap(),
        per_run
    );
    assert_eq!(
        ScopedRequisition::try_from(&scoped_shared.to_string()).unwrap(),
        scoped_shared
    );
    assert_eq!(
        ScopedRequisition::try_from(&scoped_run.to_string()).unwrap(),
        scoped_run
    );
    // malformed strings are errors
    for raw in [
        "alice:corn:not-a-uuid".to_owned(),
        "alice:corn".to_owned(),
        format!("alice:corn:{CORN}:run"),
        format!("alice:corn:{CORN}:run:not-a-uuid"),
        format!("alice:corn:{CORN}:walk:{RUN}"),
    ] {
        assert!(Requisition::try_from(&raw).is_err(), "{raw} must not parse");
    }
    for raw in [
        "node1:alice:corn:not-a-uuid".to_owned(),
        format!("node1:alice:corn:{CORN}:run:"),
        "node1".to_owned(),
    ] {
        assert!(
            ScopedRequisition::try_from(&raw).is_err(),
            "{raw} must not parse"
        );
    }
    // a requisition matches its own label map
    assert_eq!(shared, shared.labels());
    assert_eq!(per_run, per_run.labels());
    // a missing, extra, or different run label doesn't match
    assert_ne!(per_run, shared.labels());
    assert_ne!(shared, per_run.labels());
    let mut other_run = per_run.labels();
    other_run.insert(RUN_LABEL.to_owned(), Uuid::new_v4().to_string());
    assert_ne!(per_run, other_run);
    // labels with a bad tool instance or run don't parse
    let mut bad = shared.labels();
    bad.insert(TOOL_INSTANCE_LABEL.to_owned(), "nope".to_owned());
    assert!(Requisition::from_labels(&bad).is_err());
    let mut bad = shared.labels();
    bad.insert(RUN_LABEL.to_owned(), "nope".to_owned());
    assert!(Requisition::from_labels(&bad).is_err());
    let mut missing = shared.labels();
    missing.remove(TOOL_INSTANCE_LABEL);
    assert!(Requisition::from_labels(&missing).is_err());
}

/// Two container tool calls of one tool instance in one run are one requisition, while two runs
/// of one tool instance by one user are two, and the run survives every conversion
#[test]
fn per_run_requisitions() {
    // two calls of one tool instance in one run share a requisition
    let calls: HashSet<Requisition> = [deadline(CORN, Some(RUN)), deadline(CORN, Some(RUN))]
        .iter()
        .map(Requisition::from)
        .collect();
    assert_eq!(calls.len(), 1);
    // two runs of one tool instance by one user are two requisitions
    let runs: HashSet<Requisition> = [
        deadline(CORN, Some(RUN)),
        deadline(CORN, Some(Uuid::new_v4())),
    ]
    .iter()
    .map(Requisition::from)
    .collect();
    assert_eq!(runs.len(), 2);
    // a pipeline job's requisition is neither of them
    assert!(!runs.contains(&Requisition::from(deadline(CORN, None))));
    // the run survives labels, strings, and worker records
    let req = Requisition::from(deadline(CORN, Some(RUN)));
    let labels: BTreeMap<String, String> = req.labels();
    assert_eq!(Requisition::try_from(labels).unwrap().run, Some(RUN));
    assert_eq!(
        Requisition::try_from(&req.to_string()).unwrap().run,
        Some(RUN)
    );
    assert_eq!(SpawnedUpdate::from(worker(Some(RUN))).req, req);
}

/// Build the stats of one user's shared workload jobs
///
/// # Arguments
///
/// * `created` - The number of created jobs
/// * `running` - The number of running jobs
fn stage_stats(created: u64, running: u64) -> StageStats {
    StageStats {
        created,
        running,
        completed: 1,
        failed: 0,
        sleeping: 0,
        total: created + running + 1,
    }
}

/// The stats of each group's tool instances by user
type GroupFixture<'a> = Vec<(&'a str, Vec<(Uuid, Vec<(&'a str, StageStats)>)>)>;

/// Build system stats from per group, tool instance, and user stats
///
/// # Arguments
///
/// * `groups` - The stats of each group's tool instances by user
fn system_stats(groups: GroupFixture<'_>) -> SystemStats {
    // build each group's workload stats
    let groups = groups
        .into_iter()
        .map(|(group, workloads)| {
            let workloads = workloads
                .into_iter()
                .map(|(tool_instance, users)| {
                    let users = users
                        .into_iter()
                        .map(|(user, stats)| (user.to_owned(), stats))
                        .collect::<HashMap<String, StageStats>>();
                    (tool_instance, users)
                })
                .collect();
            (group.to_owned(), GroupStats { workloads })
        })
        .collect();
    SystemStats {
        deadlines: 0,
        running: 0,
        users: 2,
        k8s: crate::models::ScalerStats::default(),
        baremetal: crate::models::ScalerStats::default(),
        external: crate::models::ScalerStats::default(),
        windows: crate::models::ScalerStats::default(),
        kvm: crate::models::ScalerStats::default(),
        groups,
    }
}

/// `users_jobs` yields a shared requisition per user and tool instance, ranked by running jobs
/// and weighted by created jobs
#[test]
fn users_jobs_over_workloads() {
    // two users with jobs on two tool instances
    let other = Uuid::from_u128(0xbeef);
    let stats = system_stats(vec![(
        "corn",
        vec![
            (
                CORN,
                vec![("alice", stage_stats(3, 0)), ("bob", stage_stats(2, 1))],
            ),
            (other, vec![("alice", stage_stats(1, 2))]),
        ],
    )]);
    let map = stats.users_jobs();
    // alice has one requisition with no running jobs and one with two
    let alice = &map[&"alice".to_owned()];
    assert_eq!(
        alice[&0],
        vec![(Requisition::new("alice", "corn", CORN, None), 3)]
    );
    assert_eq!(
        alice[&2],
        vec![(Requisition::new("alice", "corn", other, None), 1)]
    );
    // bob has one requisition with one running job
    let bob = &map[&"bob".to_owned()];
    assert_eq!(bob.len(), 1);
    assert_eq!(
        bob[&1],
        vec![(Requisition::new("bob", "corn", CORN, None), 2)]
    );
    // no requisition names a run
    assert!(
        map.values()
            .flat_map(|ranks| ranks.values().flatten())
            .all(|(req, _)| req.run.is_none())
    );
}

/// The system and group totals count the users with shared workload jobs for each tool instance
/// in every group
#[test]
fn totals_over_workloads() {
    // two groups, three tool instances, and two users
    let (first, second, third) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
    let stats = system_stats(vec![
        (
            "corn",
            vec![
                (
                    first,
                    vec![("alice", stage_stats(1, 0)), ("bob", stage_stats(1, 0))],
                ),
                (second, vec![("alice", stage_stats(0, 1))]),
            ],
        ),
        (
            "wheat",
            vec![
                (second, vec![("bob", stage_stats(2, 0))]),
                (
                    third,
                    vec![("alice", stage_stats(1, 0)), ("bob", stage_stats(0, 3))],
                ),
            ],
        ),
    ]);
    // every group, tool instance, and user with jobs is counted once
    let entries = 6;
    assert_eq!(stats.total(), entries);
    assert_eq!(stats.unique().len(), entries);
    assert_eq!(stats.groups["corn"].total(), 3);
    assert_eq!(stats.groups["wheat"].total(), 3);
    assert!(stats.unique().contains(&format!("wheat:{third}:bob")));
}
