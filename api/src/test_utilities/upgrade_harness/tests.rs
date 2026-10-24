//! Unit tests of the scenario builder and the variant generator

use async_trait::async_trait;
use std::collections::BTreeSet;

use super::{
    ActionRecord, BackupRef, Check, DriverKind, FieldShape, FixtureSeed, FixtureVerifier,
    HarnessError, Injection, Interrupt, Layout, RestartTarget, Scenario, ScenarioAction,
    ScenarioDriver, VariantSet, Variation,
};
use crate::models::upgrades::{Family, FieldPath};

/// A driver that runs nothing and records each action it was asked to run
struct LogDriver {
    /// The kind of driver this one pretends to be
    kind: DriverKind,
    /// How many actions it ran
    ran: usize,
}

#[async_trait]
impl ScenarioDriver for LogDriver {
    /// The kind this driver pretends to be
    fn kind(&self) -> DriverKind {
        // as constructed
        self.kind
    }

    /// Record the action as run
    ///
    /// # Arguments
    ///
    /// * `action` - The action to run
    /// * `scenario` - The scenario it belongs to
    async fn run(
        &mut self,
        action: &ScenarioAction,
        scenario: &Scenario,
    ) -> Result<ActionRecord, HarnessError> {
        // count the action and log it rendered
        let _ = scenario;
        self.ran += 1;
        Ok(ActionRecord {
            action: action.render(),
            outcome: "ran".to_owned(),
        })
    }
}

/// A driver of a different implementation than [`LogDriver`]: it keeps the action it was handed in
/// a list of its own and answers each with an outcome of its own
struct RecordingDriver {
    /// The kind of driver this one pretends to be
    kind: DriverKind,
    /// The actions it was handed, rendered, in order
    seen: Vec<String>,
}

#[async_trait]
impl ScenarioDriver for RecordingDriver {
    /// The kind this driver pretends to be
    fn kind(&self) -> DriverKind {
        // as constructed
        self.kind
    }

    /// Keep the action's rendering and answer it
    ///
    /// # Arguments
    ///
    /// * `action` - The action to run
    /// * `scenario` - The scenario it belongs to
    async fn run(
        &mut self,
        action: &ScenarioAction,
        scenario: &Scenario,
    ) -> Result<ActionRecord, HarnessError> {
        // keep what it was handed and answer in its own words
        let _ = scenario;
        let action = action.render();
        self.seen.push(action.clone());
        Ok(ActionRecord {
            action,
            outcome: "recorded".to_owned(),
        })
    }
}

/// The family the scenarios seed
const USERS: Family = Family::Redis("users");

/// A scenario of actions both drivers can run
fn shared() -> Scenario {
    // seed, move, gate, approve, check, and set a setting
    Scenario::new("shared")
        .seed(FixtureSeed {
            family: USERS,
            layout: Layout(1),
            count: 5,
            variants: None,
        })
        .target("2026-10-v03")
        .expect_blocked("users-v2")
        .check(Check::Fixtures(FixtureVerifier::items_in(USERS, Layout(1))))
        .approve("users-v2", BackupRef::new("snap-1"))
        .set_dev_lossy(true)
        .expect_revision("2026-10-v03")
}

/// The builder rejects cluster-only actions under the direct driver with a clear error, rejects
/// `step` under the cluster driver, and hands both drivers the scenario's actions in the same order
#[tokio::test]
async fn t_tc_15_cluster_only_actions_are_rejected_under_the_direct_driver() {
    // the same scenario runs under a direct driver and a differently built cluster driver, which
    // are handed the same actions in the same order
    let scenario = shared();
    let mut direct = LogDriver {
        kind: DriverKind::Direct,
        ran: 0,
    };
    let mut cluster = RecordingDriver {
        kind: DriverKind::Cluster,
        seen: Vec::new(),
    };
    let direct_log = scenario.run_with(&mut direct).await.expect("direct runs");
    let cluster_log = scenario.run_with(&mut cluster).await.expect("cluster runs");
    let direct_actions: Vec<&str> = direct_log.iter().map(|log| log.action.as_str()).collect();
    let cluster_actions: Vec<&str> = cluster_log.iter().map(|log| log.action.as_str()).collect();
    assert_eq!(direct_actions, cluster_actions);
    // the cluster driver itself saw every action of the scenario, in order
    assert_eq!(cluster.seen, direct_actions);
    assert_eq!(direct.ran, scenario.actions().len());
    assert_eq!(direct_log.len(), scenario.actions().len());
    assert_eq!(direct_log[2].action, "expect users-v2 blocked: approval");
    // an action only a cluster has fails the direct run before any action runs
    let mut direct = LogDriver {
        kind: DriverKind::Direct,
        ran: 0,
    };
    for scenario in [
        shared().deploy("thorium:next"),
        shared().restart(RestartTarget::JobPod),
        shared().inject(Injection::InterruptAtBatch {
            batch: 1,
            signal: Interrupt::Kill,
        }),
    ] {
        let error = scenario
            .run_with(&mut direct)
            .await
            .expect_err("a cluster-only action is rejected");
        assert!(
            matches!(error, HarnessError::ClusterOnly { .. }),
            "{error:?}"
        );
        let text = error.to_string();
        assert!(text.contains("cluster-only"), "{text}");
        assert!(text.contains("direct driver"), "{text}");
        assert!(text.contains("scenario shared"), "{text}");
    }
    assert_eq!(direct.ran, 0, "nothing ran before the rejection");
    // the same scenario runs under the cluster driver
    let mut cluster = LogDriver {
        kind: DriverKind::Cluster,
        ran: 0,
    };
    let with_deploy = shared().deploy("thorium:next");
    with_deploy
        .run_with(&mut cluster)
        .await
        .expect("a cluster runs its own actions");
    // `step` runs under the direct driver only
    let stepping = Scenario::new("stepping").target("2026-10-v02").step();
    let error = stepping
        .run_with(&mut cluster)
        .await
        .expect_err("the cluster has no stop-before-step setting");
    assert!(
        matches!(error, HarnessError::DirectOnly { .. }),
        "{error:?}"
    );
    assert!(error.to_string().contains("direct driver only"));
    let mut direct = LogDriver {
        kind: DriverKind::Direct,
        ran: 0,
    };
    stepping.run_with(&mut direct).await.expect("direct steps");
}

/// The fields a variant set varies
fn fields(count: usize) -> Vec<FieldPath> {
    // distinct paths
    ["a", "b", "c", "d", "e"]
        .into_iter()
        .take(count)
        .map(FieldPath)
        .collect()
}

/// Every shape a field takes
const ALL_SHAPES: [FieldShape; 6] = [
    FieldShape::Present,
    FieldShape::Absent,
    FieldShape::Null,
    FieldShape::WrongType,
    FieldShape::Empty,
    FieldShape::UnknownVariant,
];

/// Whether a set of variations covers every pair of shapes of every two fields
fn pairwise_covered(set: &VariantSet, variations: &[Variation]) -> bool {
    // look at each pair of fields and each pair of their shapes
    for (first, one) in set.fields.iter().enumerate() {
        for two in &set.fields[first + 1..] {
            for a in &set.shapes {
                for b in &set.shapes {
                    let seen = variations
                        .iter()
                        .any(|variation| variation.shape(one) == *a && variation.shape(two) == *b);
                    if !seen {
                        return false;
                    }
                }
            }
        }
    }
    true
}

/// Up to three fields get every combination of shapes, and more get pairwise coverage
#[test]
fn t_tc_30_variant_sets_cover_combinations_and_pairs() {
    // one to three fields: the full product, with no repeats
    for (count, shapes) in [(1, 6), (2, 6), (3, 3), (3, 6)] {
        let set = VariantSet {
            fields: fields(count),
            shapes: ALL_SHAPES[..shapes].to_vec(),
        };
        let variations = set.variations();
        assert_eq!(variations.len(), shapes.pow(u32::try_from(count).unwrap()));
        let distinct: BTreeSet<_> = variations.iter().map(|v| v.shapes.clone()).collect();
        assert_eq!(distinct.len(), variations.len(), "no repeated variation");
        // every variation names a shape for every field
        assert!(
            variations
                .iter()
                .all(|variation| variation.shapes.len() == count)
        );
    }
    // four and five fields: every pair of two fields' shapes appears, in far fewer objects
    for (count, shapes) in [(4, 3), (4, 6), (5, 6)] {
        let set = VariantSet {
            fields: fields(count),
            shapes: ALL_SHAPES[..shapes].to_vec(),
        };
        let variations = set.variations();
        assert!(pairwise_covered(&set, &variations), "{count} fields");
        assert!(variations.len() >= shapes * shapes, "a pair needs a row");
        assert!(
            variations.len() < shapes.pow(u32::try_from(count).unwrap()) / 4,
            "{} variations of {count} fields",
            variations.len()
        );
    }
    // generation is deterministic
    let set = VariantSet {
        fields: fields(5),
        shapes: ALL_SHAPES.to_vec(),
    };
    assert_eq!(set.variations(), set.variations());
}
