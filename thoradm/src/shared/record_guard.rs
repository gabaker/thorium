//! Refuses writes from a thoradm older than what the keyspace records
//!
//! The API refuses a schema fingerprint its catalog doesn't name when it starts, so an older image
//! never writes objects in a layout the newer revision's migrations have already converted. The
//! writing commands refuse the same way: `schema apply` won't record its own fingerprint over a
//! revision its catalog doesn't know, and `migrate run` runs only steps the planner would plan from
//! the recorded revision. A downgrade goes through the Helm chart and the operator along the
//! catalog's reverse edges, which first moves the record to the older revision, or is a restore.

use scylla::client::session::Session;
use thorium::Conf;
use thorium::models::schema::{Fingerprint, RecordError, RecordedState, RevisionSnapshot};
use thorium::models::upgrades::{Catalog, RevisionId};

use crate::Error;

/// What every refusal tells an admin to do instead
const WAY_FORWARD: &str = "Roll forward to a Thorium release that knows the recorded revision, \
                           downgrade through the Helm chart and the operator along the catalog's \
                           reverse edges, or restore a backup";

/// Read what the record holds before a write
///
/// # Arguments
///
/// * `session` - The Scylla session to read with
/// * `conf` - The Thorium config, for the keyspace
pub async fn read(session: &Session, conf: &Conf) -> Result<RecordedState, Error> {
    // a failed read may succeed on a retry, and a record that can't be read is refused
    thorium::models::schema::read_recorded(session, conf, env!("CARGO_PKG_VERSION"))
        .await
        .map_err(|err| match err {
            RecordError::Query { .. } => Error::Transient(err.to_string()),
            other => Error::Generic(format!(
                "the schema record can't be judged: {other}. {WAY_FORWARD}"
            )),
        })
}

/// Get the deployment fingerprint this build declares
///
/// # Arguments
///
/// * `catalog` - The catalog this build plans over
fn declared(catalog: &Catalog) -> Fingerprint {
    // the declared schema is the one the API's startup check judges against
    RevisionSnapshot::declared(catalog.latest()).deployment
}

/// Describe what the keyspace records and what this build knows, for a refusal
///
/// # Arguments
///
/// * `catalog` - The catalog this build plans over
/// * `recorded` - What the record holds
fn describe(catalog: &Catalog, recorded: &RecordedState) -> String {
    // name both revisions and both fingerprints
    let revision = recorded.state.as_ref().map_or_else(
        || "no revision".to_owned(),
        |state| format!("revision {}", state.revision),
    );
    let fingerprint = recorded
        .fingerprint
        .as_ref()
        .map_or("none", Fingerprint::as_str);
    format!(
        "the keyspace records {revision} with schema fingerprint {fingerprint}; this build's \
         catalog ends at revision {} with schema fingerprint {}",
        catalog.latest(),
        declared(catalog).as_str()
    )
}

/// Refuse when the recorded revision is one this build's catalog doesn't know
///
/// A revision the catalog knows is one this build can plan from, whatever fingerprint is recorded
/// with it: a downgrade along reverse edges moves the record to the older revision while the newer
/// build's fingerprint stays recorded until the older build applies its own.
///
/// # Arguments
///
/// * `catalog` - The catalog this build plans over
/// * `recorded` - What the record holds
/// * `action` - What was refused, such as "apply the schema"
fn check_revision(catalog: &Catalog, recorded: &RecordedState, action: &str) -> Result<(), Error> {
    // nothing recorded, or a revision this build plans from, is no refusal
    let Some(state) = &recorded.state else {
        return Ok(());
    };
    if catalog.knows(&state.revision) {
        return Ok(());
    }
    // say whether a newer build recorded it, or one on another branch
    let why = if state.revision > catalog.latest() {
        format!("revision {} is later than this build knows", state.revision)
    } else {
        format!("revision {} is unknown to this build", state.revision)
    };
    Err(Error::Generic(format!(
        "This thoradm can't {action}: {why} ({}). {WAY_FORWARD}",
        describe(catalog, recorded)
    )))
}

/// Refuse to apply this build's schema over a record a newer build wrote
///
/// The apply is refused when the recorded revision is one this build's catalog doesn't know, or
/// when no revision is recorded and the recorded fingerprint is neither this build's nor one its
/// catalog names.
///
/// # Arguments
///
/// * `catalog` - The catalog this build plans over
/// * `recorded` - What the record holds
pub fn check_apply(catalog: &Catalog, recorded: &RecordedState) -> Result<(), Error> {
    // a revision this build doesn't know refuses the apply whatever the fingerprint
    check_revision(catalog, recorded, "apply the schema")?;
    // a recorded revision this build knows vouches for the fingerprint beside it
    if recorded.state.is_some() {
        return Ok(());
    }
    // without one, the fingerprint must be this build's or one its catalog names
    let Some(applied) = &recorded.fingerprint else {
        return Ok(());
    };
    let known = *applied == declared(catalog)
        || catalog
            .edges
            .iter()
            .any(|edge| edge.schema == Some(applied.as_str()));
    if known {
        return Ok(());
    }
    Err(Error::Generic(format!(
        "This thoradm can't apply the schema: schema fingerprint {} is unknown to this build ({}). \
         {WAY_FORWARD}",
        applied.as_str(),
        describe(catalog, recorded)
    )))
}

/// Refuse to run a step the planner wouldn't plan from the recorded revision
///
/// The run is refused when the recorded revision is one this build's catalog doesn't know, or
/// when the step names a revision it doesn't know. A catalog step must be in the plan from the
/// recorded revision to the revision of its edge, so a step the record already moved past or
/// shows finished is refused. A step the catalog doesn't hold, such as a re-bucket or the
/// resubmission, a re-run of a pass's issue groups or objects, and a rebuild whose reingest the
/// record still shows in progress are not checked against the plan.
///
/// # Arguments
///
/// * `catalog` - The catalog this build plans over
/// * `recorded` - What the record holds
/// * `step` - The step's revision and id, when the run names a step
/// * `reprocess` - Whether the run only re-processes named objects or one issue group
pub fn check_step(
    catalog: &Catalog,
    recorded: &RecordedState,
    step: Option<(&str, &str)>,
    reprocess: bool,
) -> Result<(), Error> {
    // a revision this build doesn't know refuses every run
    check_revision(catalog, recorded, "run a migration")?;
    let Some((revision, id)) = step else {
        return Ok(());
    };
    // the step's own revision must be one this build knows
    let refuse = |why: String| {
        Error::Generic(format!(
            "This thoradm can't run step {revision}/{id}: {why} ({}). {WAY_FORWARD}",
            describe(catalog, recorded)
        ))
    };
    let target = revision
        .parse::<RevisionId>()
        .ok()
        .filter(|target| catalog.knows(target))
        .ok_or_else(|| refuse(format!("revision {revision} is unknown to this build")))?;
    // only a catalog step, on a deployment with a recorded revision, is checked against the plan
    let held = catalog
        .edges
        .iter()
        .any(|edge| edge.id == revision && edge.steps.iter().any(|def| def.id == id));
    let Some(state) = recorded.state.as_ref().filter(|_| held && !reprocess) else {
        return Ok(());
    };
    // a rebuild whose reingest was lost runs again, which the operator does at the recorded revision
    let reingesting = catalog
        .reingesting(state)
        .iter()
        .any(|entry| entry.revision == revision && entry.step == id);
    if reingesting {
        return Ok(());
    }
    // the step must be in the plan from the recorded revision to its edge's revision
    let planned = catalog
        .plan(state, &target)
        .map_err(|err| refuse(format!("the planner refuses the path to it: {err}")))?;
    if planned
        .iter()
        .any(|planned| planned.revision == revision && planned.step == id)
    {
        return Ok(());
    }
    let finished = if catalog.finished(state, revision, id) {
        "the record shows it finished, and "
    } else {
        ""
    };
    Err(refuse(format!(
        "{finished}the plan from revision {} doesn't include it; re-run a finished step's issue \
         groups with `--reason <code>` or `--objects <keys>`",
        state.revision
    )))
}

#[cfg(test)]
mod tests {
    use thorium::models::schema::{Fingerprint, RecordedState};
    use thorium::models::upgrades::{AppliedStep, Catalog, StepState, UpgradeState};

    use super::{check_apply, check_step, declared};

    /// A fingerprint from its text
    ///
    /// # Arguments
    ///
    /// * `text` - The fingerprint's hex
    fn print(text: &str) -> Fingerprint {
        // a fingerprint is a transparent string
        serde_json::from_value(serde_json::json!(text)).expect("a fingerprint")
    }

    /// A record at a revision with a fingerprint and a history
    ///
    /// # Arguments
    ///
    /// * `revision` - The recorded revision
    /// * `fingerprint` - The recorded fingerprint
    /// * `history` - The history entries
    fn at(revision: &str, fingerprint: Fingerprint, history: Vec<AppliedStep>) -> RecordedState {
        // the state the record reads as
        let mut state = UpgradeState::new(revision.parse().expect("a revision"), "1.8.1");
        state.applied = history;
        RecordedState {
            state: Some(state),
            fingerprint: Some(fingerprint),
        }
    }

    /// A finished entry of a step
    ///
    /// # Arguments
    ///
    /// * `revision` - The step's revision
    /// * `step` - The step's id
    /// * `outcome` - How it finished
    fn entry(revision: &str, step: &str, outcome: StepState) -> AppliedStep {
        // only the fields the planner reads
        AppliedStep {
            revision: revision.to_owned(),
            step: step.to_owned(),
            outcome,
            ..AppliedStep::default()
        }
    }

    /// An apply over a revision later than the catalog's, or one it doesn't know, is refused with a
    /// message naming both revisions and fingerprints and the ways forward
    #[test]
    fn apply_refuses_a_later_or_unknown_revision() {
        let catalog = Catalog::PRODUCTION;
        let own = declared(&catalog);
        for revision in ["2099-01-v01", "2026-10-v02-pre.unknown"] {
            let recorded = at(revision, print("feedface"), Vec::new());
            let message = check_apply(&catalog, &recorded)
                .expect_err("refused")
                .to_string();
            assert!(message.contains(revision), "{message}");
            assert!(message.contains("feedface"), "{message}");
            assert!(message.contains(catalog.latest().as_str()), "{message}");
            assert!(message.contains(own.as_str()), "{message}");
            assert!(message.contains("Helm chart and the operator"), "{message}");
            assert!(message.contains("restore"), "{message}");
        }
    }

    /// An apply over a known revision goes ahead whatever fingerprint is beside it, as a reverse-edge
    /// downgrade leaves the newer build's, and so does a fresh keyspace
    #[test]
    fn apply_allows_a_known_revision_and_a_fresh_keyspace() {
        let catalog = Catalog::PRODUCTION;
        // this build's own revision and fingerprint
        let own = at(catalog.latest().as_str(), declared(&catalog), Vec::new());
        assert!(check_apply(&catalog, &own).is_ok());
        // an older revision with a fingerprint this build never declared
        let handed = at("2026-10-v02", print("0123abcd"), Vec::new());
        assert!(check_apply(&catalog, &handed).is_ok());
        // nothing recorded at all
        assert!(check_apply(&catalog, &RecordedState::default()).is_ok());
    }

    /// Without a recorded revision, an apply over a fingerprint this build doesn't know is refused,
    /// and one over its own fingerprint or a catalog revision's goes ahead
    #[test]
    fn apply_without_a_revision_refuses_an_unknown_fingerprint() {
        let catalog = Catalog::PRODUCTION;
        // a fingerprint no revision of this catalog names
        let unknown = RecordedState {
            state: None,
            fingerprint: Some(print("feedface")),
        };
        let message = check_apply(&catalog, &unknown)
            .expect_err("refused")
            .to_string();
        assert!(message.contains("feedface"), "{message}");
        assert!(message.contains("no revision"), "{message}");
        // this build's own fingerprint
        let own = RecordedState {
            state: None,
            fingerprint: Some(declared(&catalog)),
        };
        assert!(check_apply(&catalog, &own).is_ok());
        // a fingerprint an edge of the catalog names
        let named = catalog
            .edges
            .iter()
            .find_map(|edge| edge.schema)
            .expect("an edge names a fingerprint");
        let older = RecordedState {
            state: None,
            fingerprint: Some(print(named)),
        };
        assert!(check_apply(&catalog, &older).is_ok());
    }

    /// A run of a step the record moved past, of a step named at a revision this build doesn't
    /// know, or of any step over an unknown recorded revision is refused
    #[test]
    fn a_step_outside_the_plan_is_refused() {
        let catalog = Catalog::PRODUCTION;
        let latest = catalog.latest();
        let fresh = at(latest.as_str(), declared(&catalog), Vec::new());
        // a forward step below the recorded revision
        let message = check_step(
            &catalog,
            &fresh,
            Some(("2026-10-v03", "users-settings-theme")),
            false,
        )
        .expect_err("refused")
        .to_string();
        assert!(
            message.contains("2026-10-v03/users-settings-theme"),
            "{message}"
        );
        assert!(message.contains(latest.as_str()), "{message}");
        // the same step re-processing one issue group goes ahead
        assert!(
            check_step(
                &catalog,
                &fresh,
                Some(("2026-10-v03", "users-settings-theme")),
                true
            )
            .is_ok()
        );
        // a step at a revision this build doesn't know
        assert!(check_step(&catalog, &fresh, Some(("2099-01-v01", "anything")), false).is_err());
        // any run over a revision this build doesn't know, even one naming no step
        let newer = at("2099-01-v01", print("feedface"), Vec::new());
        assert!(check_step(&catalog, &newer, None, false).is_err());
        // a step the catalog doesn't hold, as the operator's re-bucket names, isn't planned
        assert!(
            check_step(
                &catalog,
                &fresh,
                Some((latest.as_str(), "rebucket-files")),
                false
            )
            .is_ok()
        );
    }

    /// A step the plan from the recorded revision holds goes ahead, a finished one doesn't, and a
    /// rebuild the record still shows reingesting may run again
    #[test]
    fn a_planned_step_runs_and_a_finished_one_is_refused() {
        let catalog = Catalog::PRODUCTION;
        // a deployment at the second revision plans the third's users step
        let at_v02 = at("2026-10-v02", print("0123abcd"), Vec::new());
        assert!(
            check_step(
                &catalog,
                &at_v02,
                Some(("2026-10-v03", "users-settings-theme")),
                false
            )
            .is_ok()
        );
        // once the record shows it finished, it isn't planned again
        let finished = at(
            "2026-10-v02",
            print("0123abcd"),
            vec![entry(
                "2026-10-v03",
                "users-settings-theme",
                StepState::Done,
            )],
        );
        let message = check_step(
            &catalog,
            &finished,
            Some(("2026-10-v03", "users-settings-theme")),
            false,
        )
        .expect_err("refused")
        .to_string();
        assert!(message.contains("finished"), "{message}");
        // a rebuild still reingesting at the recorded revision runs again
        let (edge, rebuild) = catalog
            .steps()
            .find(|(_, step)| step.thoradm_migration == Some("elastic-rebuild"))
            .expect("a rebuild step");
        let reingesting = at(
            edge.id,
            print("0123abcd"),
            vec![entry(edge.id, rebuild.id, StepState::Reingesting)],
        );
        assert!(check_step(&catalog, &reingesting, Some((edge.id, rebuild.id)), false).is_ok());
    }
}
