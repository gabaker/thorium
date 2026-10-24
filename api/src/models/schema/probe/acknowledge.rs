//! Acknowledging drifted objects, summarizing a drift report for the status, and finding the steps
//! drift blocks

use serde::{Deserialize, Serialize};

use super::{DriftReport, ObjectDrift};
use crate::models::upgrades::{
    BackendSchemaStatus, DriftEntry, DriftKind, DriftSummary, SchemaBackend, SchemaTarget,
    backend_name,
};

/// How many unacknowledged objects the status lists; `thoradm schema diff` lists them all
pub const SUMMARY_OBJECTS: usize = 20;

/// How many unlisted blocking objects a summary carries, so a summary noted in the operator's
/// state stays small however many objects drift
pub const SUMMARY_BLOCKERS: usize = 200;

/// Find the acknowledgements that name an object
///
/// # Arguments
///
/// * `acks` - The acknowledgements to search
/// * `backend` - The object's backend
/// * `object` - The object's name as `thoradm schema diff` names it
fn naming<'a>(
    acks: &'a [Acknowledgement],
    backend: SchemaBackend,
    object: &'a str,
) -> impl Iterator<Item = &'a Acknowledgement> {
    // the same backend and the same object name
    acks.iter()
        .filter(move |ack| ack.backend == backend && ack.object == object)
}

/// Whether an acknowledgement still covers an object as it is observed now
///
/// # Arguments
///
/// * `ack` - The acknowledgement
/// * `object` - The drifted object
fn covers(ack: &Acknowledgement, object: &ObjectDrift) -> bool {
    // one without a fingerprint covers whatever the object becomes, one with a fingerprint covers
    // only the object it saw
    match (&ack.fingerprint, &object.fingerprint) {
        (None, _) => true,
        (Some(acked), Some(observed)) => acked == observed.as_str(),
        (Some(_), None) => false,
    }
}

/// Name a kind of drift the way the report does
///
/// # Arguments
///
/// * `kind` - The kind of drift
fn kind_name(kind: DriftKind) -> &'static str {
    // the same lowercase names the serialized form uses
    match kind {
        DriftKind::Missing => "missing",
        DriftKind::Changed => "changed",
        DriftKind::Unexpected => "unexpected",
        DriftKind::Unknown => "unknown",
    }
}

/// Whether drift in an object blocks the steps that target it
///
/// A `missing` object doesn't: the converge step creates it, and when it can't, that step fails
/// on its own. A `changed` Elastic index doesn't either, since its rebuild resolves it.
///
/// # Arguments
///
/// * `entry` - The drifted object
fn blocks(entry: &DriftEntry) -> bool {
    // only differences a step would build on top of block it
    match entry.kind {
        DriftKind::Missing => false,
        DriftKind::Changed => entry.backend != SchemaBackend::Elastic,
        DriftKind::Unexpected | DriftKind::Unknown => true,
    }
}

/// Whether an entry is a changed layout of a Redis object
///
/// The schema apply records the layouts a build declares before the step that converts the data
/// runs, so the object a conversion step targets reads as changed until the step finishes; that
/// step finds the real state of each object itself.
///
/// # Arguments
///
/// * `entry` - The drifted object
fn is_layout(entry: &DriftEntry) -> bool {
    // a layout is the version of a Redis key family
    entry.backend == SchemaBackend::Redis && entry.kind == DriftKind::Changed
}

/// Whether two dotted field paths are the same or one holds the other
///
/// # Arguments
///
/// * `left` - One path
/// * `right` - The other path
fn paths_overlap(left: &str, right: &str) -> bool {
    // a property overlaps its own children and its parents
    left == right
        || left
            .strip_prefix(right)
            .is_some_and(|rest| rest.starts_with('.'))
        || right
            .strip_prefix(left)
            .is_some_and(|rest| rest.starts_with('.'))
}

/// Whether a drifted object overlaps the object or field a step targets
///
/// # Arguments
///
/// * `entry` - The drifted object, named `<object>` or `<object>.<field>`
/// * `target` - What the step targets
fn overlaps(entry: &DriftEntry, target: &SchemaTarget) -> bool {
    // the same backend and the same object
    if entry.backend != target.backend {
        return false;
    }
    // a dot may belong to the object's own name or separate it from a field, so every reading of
    // the name is tried
    let whole = entry.object == target.object;
    let split = entry.object.match_indices('.').any(|(at, _)| {
        let (object, field) = (&entry.object[..at], &entry.object[at + 1..]);
        object == target.object
            && target
                .field
                .is_none_or(|targeted| paths_overlap(field, targeted))
    });
    // an entry naming a whole object overlaps every field of it
    whole || split
}

/// An admin's acknowledgement of one `unexpected` object, in the `drift_acknowledged` form
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Acknowledgement {
    /// The backend the object belongs to
    pub backend: SchemaBackend,
    /// The object as `thoradm schema diff` names it, such as `samples_list.preview`
    pub object: String,
    /// The observed fingerprint acknowledged; when set, the acknowledgement lapses if the object
    /// changes again
    #[serde(default)]
    pub fingerprint: Option<String>,
}

impl DriftReport {
    /// Apply admins' acknowledgements to the report
    ///
    /// An `unexpected` object is acknowledged by an entry that names it without a fingerprint, or
    /// with the fingerprint observed now. An entry whose fingerprint the object no longer has has
    /// lapsed: the object is unacknowledged again and marked `lapsed`. A `missing` or `changed`
    /// object is never acknowledged, since steps and `schema apply` repair those. A backend is
    /// `ok` again once every one of its objects is acknowledged.
    ///
    /// # Arguments
    ///
    /// * `acks` - The acknowledgements to apply
    pub fn acknowledge(&mut self, acks: &[Acknowledgement]) {
        for (backend, found) in &mut self.backends {
            for object in &mut found.objects {
                // only an unexpected object can be acknowledged; the registry's stay acknowledged
                if object.kind != DriftKind::Unexpected || object.acknowledged {
                    continue;
                }
                // acknowledged while some entry covers it, lapsed when entries name it but none does
                let named = naming(acks, *backend, &object.object).collect::<Vec<_>>();
                object.acknowledged = named.iter().any(|ack| covers(ack, object));
                object.lapsed = !named.is_empty() && !object.acknowledged;
            }
            // the backend drifts while any object is unacknowledged
            found.status = if found.objects.iter().all(|object| object.acknowledged) {
                BackendSchemaStatus::Ok
            } else {
                BackendSchemaStatus::Drift
            };
        }
    }

    /// Summarize the report for `status.upgrade.drift`
    ///
    /// Counts the unacknowledged objects by kind and the acknowledged ones, and lists the first
    /// [`SUMMARY_OBJECTS`] unacknowledged objects in report order, so the status stays small
    /// however much drifts and an acknowledged object never takes the place of an unacknowledged
    /// one. Unlisted objects that would block a step are carried in `blockers`, up to
    /// [`SUMMARY_BLOCKERS`] of them.
    ///
    /// # Arguments
    ///
    /// * `acks` - The acknowledgements the report was acknowledged with
    #[must_use]
    pub fn summary(&self, acks: &[Acknowledgement]) -> DriftSummary {
        let mut summary = DriftSummary::default();
        let mut unlisted = 0;
        for (backend, found) in &self.backends {
            // each backend's status counts every object, listed or not
            summary.backends.insert(*backend, found.status);
            for object in &found.objects {
                // an acknowledged object is only counted, and counted apart when its
                // acknowledgement never lapses
                if object.acknowledged {
                    summary.acknowledged += 1;
                    let open =
                        naming(acks, *backend, &object.object).any(|ack| ack.fingerprint.is_none());
                    summary.unfingerprinted += u32::from(open);
                    continue;
                }
                // count the unacknowledged object by its kind
                match object.kind {
                    DriftKind::Missing => summary.missing += 1,
                    DriftKind::Changed | DriftKind::Unknown => summary.changed += 1,
                    DriftKind::Unexpected => summary.unexpected += 1,
                }
                // list it until the cap, then only count it
                let entry = DriftEntry {
                    backend: *backend,
                    object: object.object.clone(),
                    kind: object.kind,
                    fingerprint: object
                        .fingerprint
                        .as_ref()
                        .map(|print| print.as_str().to_owned()),
                    lapsed: object.lapsed,
                };
                if summary.objects.len() < SUMMARY_OBJECTS {
                    summary.objects.push(entry);
                } else {
                    unlisted += 1;
                    // an unlisted object that blocks a step is still carried so the step blocks
                    if blocks(&entry) {
                        // past the cap, say that blockers were left out instead of carrying them
                        if summary.blockers.len() < SUMMARY_BLOCKERS {
                            summary.blockers.push(entry);
                        } else {
                            summary.blockers_truncated = true;
                        }
                    }
                }
            }
        }
        summary.more = unlisted;
        summary
    }
}

impl DriftSummary {
    /// Whether any unacknowledged drift remains, which sets the `SchemaDrift` condition
    #[must_use]
    pub fn holds(&self) -> bool {
        // any unacknowledged object of any kind
        self.missing + self.changed + self.unexpected > 0
    }

    /// Describe the summary for the `SchemaDrift` condition's message
    ///
    /// Counts each kind, names the backends that drift, and says how many acknowledgements lapsed
    /// among the listed objects.
    #[must_use]
    pub fn describe(&self) -> String {
        // without drift, say what was acknowledged
        if !self.holds() {
            return format!(
                "No unacknowledged schema drift ({} acknowledged)",
                self.acknowledged
            );
        }
        // count each kind that occurs
        let kinds = [
            (self.missing, "missing"),
            (self.changed, "changed"),
            (self.unexpected, "unexpected"),
        ]
        .iter()
        .filter(|(count, _)| *count > 0)
        .map(|(count, name)| format!("{count} {name}"))
        .collect::<Vec<_>>()
        .join(", ");
        // name the backends that drift
        let backends = self
            .backends
            .iter()
            .filter(|(_, status)| **status == BackendSchemaStatus::Drift)
            .map(|(backend, _)| backend_name(*backend))
            .collect::<Vec<_>>()
            .join(", ");
        // note acknowledgements that lapsed because their object changed
        let lapsed = self.objects.iter().filter(|object| object.lapsed).count();
        let note = if lapsed > 0 {
            format!("; {lapsed} acknowledgement(s) lapsed because the object changed")
        } else {
            String::new()
        };
        format!("{backends}: {kinds} not acknowledged{note} (thoradm schema diff for details)")
    }

    /// List the objects, listed or carried as blockers, that block a step with these targets
    ///
    /// # Arguments
    ///
    /// * `targets` - The objects and fields the step brings to the target revision's definition
    #[must_use]
    pub fn blocking(&self, targets: &[SchemaTarget]) -> Vec<&DriftEntry> {
        // a step that doesn't convert a layout is blocked by every kind of blocking drift
        self.blocking_for(targets, false)
    }

    /// List the objects, listed or carried as blockers, that block a step with these targets
    ///
    /// A summary whose blockers were cut short can't say which objects it left out, so
    /// [`DriftSummary::block_message`] blocks every step with targets then.
    ///
    /// # Arguments
    ///
    /// * `targets` - The objects and fields the step brings to the target revision's definition
    /// * `converts` - Whether the step converts the layout of the Redis objects it targets, in
    ///   which case a changed layout of those objects doesn't block it
    #[must_use]
    pub fn blocking_for(&self, targets: &[SchemaTarget], converts: bool) -> Vec<&DriftEntry> {
        // an object that blocks and overlaps any target
        self.objects
            .iter()
            .chain(&self.blockers)
            .filter(|entry| blocks(entry))
            .filter(|entry| !(converts && is_layout(entry)))
            .filter(|entry| targets.iter().any(|target| overlaps(entry, target)))
            .collect()
    }

    /// Explain why a step is blocked, naming the drifted objects it targets
    ///
    /// # Arguments
    ///
    /// * `step` - The step's id
    /// * `targets` - The objects and fields the step brings to the target revision's definition
    #[must_use]
    pub fn block_message(&self, step: &str, targets: &[SchemaTarget]) -> Option<String> {
        // a step that doesn't convert a layout is blocked by every kind of blocking drift
        self.block_message_for(step, targets, false)
    }

    /// Explain why a step is blocked, naming the drifted objects it targets
    ///
    /// # Arguments
    ///
    /// * `step` - The step's id
    /// * `targets` - The objects and fields the step brings to the target revision's definition
    /// * `converts` - Whether the step converts the layout of the Redis objects it targets, in
    ///   which case a changed layout of those objects doesn't block it
    #[must_use]
    pub fn block_message_for(
        &self,
        step: &str,
        targets: &[SchemaTarget],
        converts: bool,
    ) -> Option<String> {
        // nothing blocks a step whose targets haven't drifted
        let blocking = self.blocking_for(targets, converts);
        // blockers cut short may hide the object a step targets, so any step with targets blocks
        let hidden = self.blockers_truncated && !targets.is_empty();
        if blocking.is_empty() && !hidden {
            return None;
        }
        // name each object with its backend and how it differs
        let named = blocking
            .iter()
            .map(|entry| {
                format!(
                    "{} {} ({})",
                    backend_name(entry.backend),
                    entry.object,
                    kind_name(entry.kind)
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        // say when more objects than the summary carries may differ
        let named = if hidden {
            format!("{named} and more than the {SUMMARY_BLOCKERS} the status carries")
        } else {
            named
        };
        Some(format!(
            "Step {step} targets schema objects that differ from the recorded revision: {named}. \
             Restore them to the recorded definition, or acknowledge an unexpected object in \
             spec.upgrade.drift_acknowledged; thoradm schema diff has the details"
        ))
    }
}
