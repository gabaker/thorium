//! The artifacts each reversibility kind requires, registered beside the catalog, the checks
//! that every step has them, and the lint that keeps lossy fills on security-sensitive fields
//! restrictive
//!
//! A step's transform says how records move; its artifacts prove the move can be undone: fixtures
//! the round trip runs on, fixtures of objects that never had an old form, compatibility tests,
//! the fields and enum variants its revision changed, and which fields grant or deny access. The
//! checks run them, so a missing or failing artifact fails the catalog tests.

use std::collections::BTreeSet;

use scylla::value::CqlValue;

use super::{
    Catalog, CatalogEntry, EnumMapping, Family, Fill, FillValue, MigrationPhase, PrivilegeOrder,
    Record, RecordClass, Reversibility, RevisionId, StepDef, TransformDef,
};

/// The artifacts registered for a catalog's reversible steps
#[derive(Debug, Clone, Copy)]
pub struct ReverseArtifacts {
    /// Round-trip fixtures by forward step: records in the step transform's `from` layout, which
    /// `up`, the reverse, and `up` again run on
    pub round_trips: &'static [StepFixtures],
    /// New-layout fixtures by `Backfill` step: records in the transform's `to` layout that never
    /// had an old form, which the reverse must derive one for
    pub native: &'static [StepFixtures],
    /// The compatibility test of each `DualWrite` and `LossyRevert` step: objects written by the
    /// new revision load through the previous revision's projection
    pub compatibility: &'static [CompatibilityTest],
    /// What each `LossyRevert` step's revision changed, from the snapshot diff
    pub changes: &'static [StepChanges],
    /// The fields that grant or deny access, which lossy fills must fill restrictively
    pub sensitive: &'static [SensitiveField],
    /// Reviewed exceptions that let a constant or derived fill through on a sensitive field of the
    /// kind [`SensitiveKind::Other`]
    pub fill_exceptions: &'static [FillException],
}

impl ReverseArtifacts {
    /// No artifacts at all, for a catalog without reversible steps
    pub const EMPTY: ReverseArtifacts = ReverseArtifacts {
        round_trips: &[],
        native: &[],
        compatibility: &[],
        changes: &[],
        sensitive: &[],
        fill_exceptions: &[],
    };
}

/// Fixtures registered for one step
#[derive(Debug, Clone, Copy)]
pub struct StepFixtures {
    /// The forward step the fixtures are for
    pub step: &'static str,
    /// Build the fixtures
    pub fixtures: fn() -> Vec<Record>,
}

/// A compatibility test registered for one step
#[derive(Debug, Clone, Copy)]
pub struct CompatibilityTest {
    /// The forward step the test is for
    pub step: &'static str,
    /// Run the test, returning why it failed
    pub test: fn() -> Result<(), String>,
}

/// What a lossy step's revision changed, as the snapshot diff shows it
#[derive(Debug, Clone, Copy)]
pub struct StepChanges {
    /// The forward step the changes are for
    pub step: &'static str,
    /// The previous revision's fields or columns the new revision dropped or changed, which a
    /// reverse can't always derive
    pub lost: &'static [&'static str],
    /// The enum variants the new revision added
    pub new_variants: &'static [EnumVariant],
}

/// An enum variant a revision added to a field
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnumVariant {
    /// The field holding the enum
    pub field: &'static str,
    /// The added variant
    pub variant: &'static str,
}

/// A field that grants or denies access, marked security-sensitive in its table or layout
/// declaration, with the kind of grant that decides what a restrictive fill is
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SensitiveField {
    /// The family holding the field
    pub family: Family,
    /// The field or column
    pub field: &'static str,
    /// What kind of grant or denial it holds
    pub kind: SensitiveKind,
}

/// What kind of grant or denial a security-sensitive field holds, which decides the restrictive
/// fills the lint accepts
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SensitiveKind {
    /// A boolean grant: only `false` is restrictive
    Grant,
    /// A role or privilege level: a target whose grants are a subset of the source's, by the
    /// step's privilege order
    Role {
        /// Every variant the field can hold in this revision or the previous one, which the
        /// privilege order must list
        variants: &'static [&'static str],
    },
    /// A set of grants: only the empty set is restrictive
    GrantSet,
    /// A ban, lock, or expiration: only its most restrictive value is
    Denial {
        /// The most restrictive value (banned, disabled, already expired)
        most_restrictive: &'static str,
    },
    /// A credential or token: only clearing it is restrictive
    Credential,
    /// Anything else: only the sentinel the authorization code treats as deny is restrictive
    Other {
        /// The deny sentinel
        deny: &'static str,
    },
}

/// A reviewed exception for one fill on a sensitive field of the kind [`SensitiveKind::Other`]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FillException {
    /// The forward step whose fill is excepted
    pub step: &'static str,
    /// The filled field
    pub field: &'static str,
    /// Who reviewed it and why the fill is safe
    pub review: &'static str,
}

/// The text forms of an empty set a set-of-grants fill may write
const EMPTY_SETS: [&str; 3] = ["", "[]", "{}"];

/// The text forms of a cleared credential
const CLEARED: [&str; 2] = ["", "null"];

impl Catalog {
    /// Check that every reversible step has the artifacts its kind requires, running them
    ///
    /// Every step of an edge leading down that reverses a transform (other than an irreversible
    /// one) has that transform's `down` and registered round-trip fixtures, on which `up`, the
    /// reverse, and `up` again match the first `up` (for a lossy reverse: every sentinel fill
    /// lands, and the re-upgraded record is current). A `DualWrite` step has a `down`, a
    /// compatibility test, and a later contract revision holding a contract step; a `Backfill`
    /// step has a `down` and new-layout fixtures its reverse derives an old form for; a
    /// `LossyRevert` step has a `down`, a compatibility test, a loss description, the changes of
    /// its revision, a lost field for every field the changes dropped, a fill for every lost
    /// field, a mapping for every added enum variant, and fills and mappings the lint accepts.
    ///
    /// # Arguments
    ///
    /// * `artifacts` - The artifacts registered for this catalog
    pub fn check_artifacts(&self, artifacts: &ReverseArtifacts) -> Result<(), Vec<String>> {
        // gather the problems of the reverse steps and of each forward step's kind
        let mut problems = Vec::new();
        self.check_round_trips(artifacts, &mut problems);
        for (edge, step) in self.steps().filter(|(edge, _)| !edge.is_downward()) {
            // each kind has its own artifacts
            match step.reversibility {
                Some(Reversibility::DualWrite { until }) => {
                    self.check_dual_write(edge, step, until, artifacts, &mut problems);
                }
                Some(Reversibility::Backfill) => check_backfill(step, artifacts, &mut problems),
                Some(Reversibility::LossyRevert { .. }) => {
                    check_lossy(step, artifacts, &mut problems);
                    problems.extend(lint_fills(step, artifacts));
                }
                Some(Reversibility::Irreversible { .. }) | None => (),
            }
        }
        // a check with no problems passes
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems)
        }
    }

    /// Check that every step of an edge leading down that reverses a transform has its `down`
    /// and round-trip fixtures that pass
    ///
    /// # Arguments
    ///
    /// * `artifacts` - The artifacts registered for this catalog
    /// * `problems` - The problems found so far
    fn check_round_trips(&self, artifacts: &ReverseArtifacts, problems: &mut Vec<String>) {
        // look at the forward step each reverse step undoes
        for (_, reverse) in self.steps().filter(|(edge, _)| edge.is_downward()) {
            let Some((_, forward)) = reverse.reverses.and_then(|id| self.step(id)) else {
                continue;
            };
            // a step without a transform, or one refused on the way down, has no records to undo
            let Some(transform) = forward.transform else {
                continue;
            };
            if matches!(
                forward.reversibility,
                Some(Reversibility::Irreversible { .. })
            ) {
                continue;
            }
            // the reverse runs the transform's `down`
            if transform.down.is_none() {
                problems.push(format!(
                    "step {} reverses {}, whose transform has no `down`",
                    reverse.id, forward.id
                ));
                continue;
            }
            // and a registered round trip proves it
            let Some(registered) = fixtures_of(artifacts.round_trips, forward.id) else {
                problems.push(format!(
                    "step {} reverses {}, which has no registered round-trip test",
                    reverse.id, forward.id
                ));
                continue;
            };
            for fixture in (registered.fixtures)() {
                if let Err(error) = round_trip(transform, forward.reversibility.as_ref(), &fixture)
                {
                    problems.push(format!("the round trip of {} failed: {error}", forward.id));
                }
            }
        }
    }

    /// Check a `DualWrite` step's artifacts: its `down`, its compatibility test, and a later
    /// contract revision whose edge holds a contract step
    ///
    /// # Arguments
    ///
    /// * `edge` - The edge holding the step
    /// * `step` - The step
    /// * `until` - The contract revision the step names
    /// * `artifacts` - The artifacts registered for this catalog
    /// * `problems` - The problems found so far
    fn check_dual_write(
        &self,
        edge: &CatalogEntry,
        step: &StepDef,
        until: &str,
        artifacts: &ReverseArtifacts,
        problems: &mut Vec<String>,
    ) {
        // the new-to-old conversion the dual writes use is the transform's `down`
        require_down(step, "DualWrite", problems);
        // objects the new revision writes must load through the previous revision's projection
        run_compatibility(step, artifacts, problems);
        // the contract revision comes after the step's and its edge removes the dual write
        let later = until
            .parse::<RevisionId>()
            .ok()
            .filter(|until| *until != edge.revision() && self.at_or_below(&edge.revision(), until));
        let contracted = later.is_some_and(|until| {
            self.edges.iter().any(|contract| {
                !contract.is_downward()
                    && contract.id == until.as_str()
                    && contract
                        .steps
                        .iter()
                        .any(|later| later.phase == Some(MigrationPhase::Contract))
            })
        });
        if !contracted {
            problems.push(format!(
                "DualWrite step {} names no later contract revision {until} whose edge holds a \
                 contract step removing the dual write",
                step.id
            ));
        }
    }
}

/// Find the fixtures registered for a step
///
/// # Arguments
///
/// * `registered` - The registered fixtures
/// * `step` - The forward step
fn fixtures_of(registered: &[StepFixtures], step: &str) -> Option<StepFixtures> {
    // the first registration for the step wins
    registered
        .iter()
        .find(|fixtures| fixtures.step == step)
        .copied()
}

/// Check that a step's transform has a `down`
///
/// # Arguments
///
/// * `step` - The step
/// * `kind` - The step's reversibility kind, for the message
/// * `problems` - The problems found so far
fn require_down(step: &StepDef, kind: &str, problems: &mut Vec<String>) {
    // a kind that reverses records needs the transform's `down`
    if step
        .transform
        .is_none_or(|transform| transform.down.is_none())
    {
        problems.push(format!(
            "{kind} step {} has no transform with a `down`",
            step.id
        ));
    }
}

/// Run a step's registered compatibility test, reporting a missing or failing one
///
/// # Arguments
///
/// * `step` - The step
/// * `artifacts` - The artifacts registered for the catalog
/// * `problems` - The problems found so far
fn run_compatibility(step: &StepDef, artifacts: &ReverseArtifacts, problems: &mut Vec<String>) {
    // find the step's test and run it
    match artifacts
        .compatibility
        .iter()
        .find(|test| test.step == step.id)
    {
        Some(test) => {
            if let Err(error) = (test.test)() {
                problems.push(format!(
                    "the compatibility test of {} failed: {error}",
                    step.id
                ));
            }
        }
        None => problems.push(format!(
            "step {} has no generated compatibility test",
            step.id
        )),
    }
}

/// Check a `Backfill` step's artifacts: its `down`, and new-layout fixtures its reverse derives
/// an old form for that upgrades back to the same record
///
/// # Arguments
///
/// * `step` - The step
/// * `artifacts` - The artifacts registered for the catalog
/// * `problems` - The problems found so far
fn check_backfill(step: &StepDef, artifacts: &ReverseArtifacts, problems: &mut Vec<String>) {
    // the reverse pass derives the old form with the transform's `down`
    require_down(step, "Backfill", problems);
    let Some(transform) = step.transform.filter(|transform| transform.down.is_some()) else {
        return;
    };
    // objects created in the new layout must revert too
    let Some(registered) = fixtures_of(artifacts.native, step.id) else {
        problems.push(format!(
            "Backfill step {} has no new-layout fixtures",
            step.id
        ));
        return;
    };
    for fixture in (registered.fixtures)() {
        if let Err(error) = native_round_trip(transform, &fixture) {
            problems.push(format!(
                "a new-layout fixture of {} failed: {error}",
                step.id
            ));
        }
    }
}

/// Check that a record that never had an old form reverts and upgrades back to itself
///
/// # Arguments
///
/// * `transform` - The transform
/// * `fixture` - A record in the transform's `to` layout
fn native_round_trip(transform: &TransformDef, fixture: &Record) -> Result<(), String> {
    // the fixture must be in the transform's new layout
    if (transform.classify)(fixture) != RecordClass::Current {
        return Err(format!("{fixture:?} isn't in the new layout"));
    }
    // reverting and upgrading it again must give it back
    let reverted = transform
        .revert(fixture.clone(), None)
        .map_err(|issue| format!("the reverse failed: {}", issue.reason))?;
    let again = (transform.up)(reverted).map_err(|issue| format!("up failed: {}", issue.reason))?;
    if again == *fixture {
        Ok(())
    } else {
        Err(format!("reverse -> up gave {again:?}, not {fixture:?}"))
    }
}

/// Run the round trip of a transform on one fixture: `up`, the reverse, and `up` again
///
/// A lossless reverse must give back the first `up`'s record. A lossy one can't, so every
/// sentinel fill whose field `down` left absent must hold its sentinel after the reverse, and the
/// re-upgraded record must be current.
///
/// # Arguments
///
/// * `transform` - The transform
/// * `reversibility` - How the step is undone
/// * `fixture` - A record in the transform's `from` layout
fn round_trip(
    transform: &TransformDef,
    reversibility: Option<&Reversibility>,
    fixture: &Record,
) -> Result<(), String> {
    // a lossless reverse is the plain round trip
    let Some(Reversibility::LossyRevert { fills, .. }) = reversibility else {
        return transform.check_round_trip(fixture);
    };
    // upgrade the fixture and revert it, noting which fields `down` alone leaves absent
    let upgraded =
        (transform.up)(fixture.clone()).map_err(|issue| format!("up failed: {}", issue.reason))?;
    let down = transform
        .down
        .ok_or_else(|| "the transform has no `down`".to_owned())?;
    let bare = down(upgraded.clone()).map_err(|issue| format!("down failed: {}", issue.reason))?;
    let reverted = transform
        .revert(upgraded, reversibility)
        .map_err(|issue| format!("the reverse failed: {}", issue.reason))?;
    // every sentinel fill of a field `down` couldn't derive holds its sentinel
    for fill in *fills {
        if let FillValue::Sentinel(sentinel) = fill.value
            && !bare.has_field(fill.field)
            && reverted.text(fill.field).as_deref() != Some(sentinel)
        {
            return Err(format!(
                "the reverse wrote no sentinel {sentinel:?} in {}",
                fill.field
            ));
        }
    }
    // the reverted record upgrades again to a current record
    let again = (transform.up)(reverted)
        .map_err(|issue| format!("up after the reverse failed: {}", issue.reason))?;
    if (transform.classify)(&again) == RecordClass::Current {
        Ok(())
    } else {
        Err(format!(
            "up after the reverse gave {again:?}, which isn't current"
        ))
    }
}

/// Check a `LossyRevert` step's artifacts, apart from the fill lint: its `down`, compatibility
/// test, loss description, the changes of its revision, a lost field for every field they drop,
/// a fill for every lost field, and a mapping for every added enum variant
///
/// # Arguments
///
/// * `step` - The step
/// * `artifacts` - The artifacts registered for the catalog
/// * `problems` - The problems found so far
fn check_lossy(step: &StepDef, artifacts: &ReverseArtifacts, problems: &mut Vec<String>) {
    // only a lossy step has these artifacts
    let Some(Reversibility::LossyRevert {
        fills,
        lost_fields,
        enum_mappings,
        description,
        ..
    }) = step.reversibility
    else {
        return;
    };
    // the reverse pass runs the transform's `down`, and the new revision's objects must load
    // through the previous revision's projection
    require_down(step, "LossyRevert", problems);
    run_compatibility(step, artifacts, problems);
    // the approval message and release notes describe the loss
    if description.trim().is_empty() {
        problems.push(format!(
            "LossyRevert step {} doesn't describe what it loses",
            step.id
        ));
    }
    // the snapshot diff of the step's revision says which fields and variants changed
    let Some(changes) = artifacts
        .changes
        .iter()
        .find(|changes| changes.step == step.id)
    else {
        problems.push(format!(
            "LossyRevert step {} has no registered snapshot diff",
            step.id
        ));
        return;
    };
    // every field the diff drops or changes is declared lost
    for lost in changes.lost {
        if !lost_fields.contains(lost) {
            problems.push(format!(
                "LossyRevert step {} doesn't declare {lost}, which its revision drops, as lost",
                step.id
            ));
        }
    }
    // every lost field has a fill
    for lost in lost_fields {
        if !fills.iter().any(|fill| fill.field == *lost) {
            problems.push(format!(
                "LossyRevert step {} has no fill for the lost field {lost}",
                step.id
            ));
        }
    }
    // every added variant maps to a previous variant
    for added in changes.new_variants {
        if !enum_mappings
            .iter()
            .any(|mapping| mapping.field == added.field && mapping.new_variant == added.variant)
        {
            problems.push(format!(
                "LossyRevert step {} has no mapping for the variant {} added to {}",
                step.id, added.variant, added.field
            ));
        }
    }
}

/// Lint the fills and enum mappings of a lossy step on security-sensitive fields, returning
/// every one that isn't restrictive
///
/// A field is sensitive when the artifacts mark it, in the family of the step's transform (or
/// in any family for a step without one). Each kind accepts only its restrictive fills: a boolean
/// grant `false`; a role a target whose grants are a subset of every source's, by a privilege
/// order of the step that lists every variant (a derived role fill is run on every source
/// variant); a set of grants the empty set; a denial its most restrictive value; a credential
/// cleared; anything else its deny sentinel, or a fill a reviewed exception names. A mapping on a
/// role field is accepted only when the old variant's grants are a subset of the new one's.
///
/// # Arguments
///
/// * `step` - The lossy step
/// * `artifacts` - The artifacts registered for the catalog
#[must_use]
pub fn lint_fills(step: &StepDef, artifacts: &ReverseArtifacts) -> Vec<String> {
    // only a lossy step fills or maps anything
    let Some(Reversibility::LossyRevert {
        fills,
        enum_mappings,
        privilege_orders,
        ..
    }) = step.reversibility
    else {
        return Vec::new();
    };
    // lint each fill and each mapping on a sensitive field
    let mut problems = Vec::new();
    for fill in fills {
        if let Some(sensitive) = sensitive_field(step, fill.field, artifacts) {
            lint_fill(
                step,
                fill,
                sensitive,
                privilege_orders,
                artifacts,
                &mut problems,
            );
        }
    }
    for mapping in enum_mappings {
        if let Some(sensitive) = sensitive_field(step, mapping.field, artifacts) {
            lint_mapping(step, mapping, sensitive, privilege_orders, &mut problems);
        }
    }
    problems
}

/// Find the sensitive field a step's fill or mapping writes, if the field is sensitive
///
/// # Arguments
///
/// * `step` - The step
/// * `field` - The field written
/// * `artifacts` - The artifacts registered for the catalog
fn sensitive_field(
    step: &StepDef,
    field: &str,
    artifacts: &ReverseArtifacts,
) -> Option<SensitiveField> {
    // a step with a transform writes only its own family
    let family = step.transform.map(|transform| transform.family);
    artifacts
        .sensitive
        .iter()
        .find(|sensitive| {
            sensitive.field == field && family.is_none_or(|family| family == sensitive.family)
        })
        .copied()
}

/// Lint one fill on a sensitive field
///
/// # Arguments
///
/// * `step` - The lossy step
/// * `fill` - The fill
/// * `sensitive` - The sensitive field it writes
/// * `orders` - The step's privilege orders
/// * `artifacts` - The artifacts registered for the catalog
/// * `problems` - The problems found so far
fn lint_fill(
    step: &StepDef,
    fill: &Fill,
    sensitive: SensitiveField,
    orders: &[PrivilegeOrder],
    artifacts: &ReverseArtifacts,
    problems: &mut Vec<String>,
) {
    // a constant or sentinel is judged by its text; a derived value only where it can be run
    let constant = match fill.value {
        FillValue::Constant(value) | FillValue::Sentinel(value) => Some(value),
        FillValue::Derived(_) => None,
    };
    // a rejection names the step and the field
    let reject = |why: String| format!("step {} fills {}: {why}", step.id, fill.field);
    match sensitive.kind {
        // a boolean grant is filled with `false`
        SensitiveKind::Grant => {
            if constant != Some("false") {
                problems.push(reject(
                    "a boolean grant may only be filled with false".to_owned(),
                ));
            }
        }
        // a role is filled with one no more privileged than any source
        SensitiveKind::Role { variants } => {
            let Some(order) = complete_order(orders, sensitive.field, variants, problems, step)
            else {
                return;
            };
            match fill.value {
                FillValue::Constant(target) | FillValue::Sentinel(target) => {
                    for source in order.variants {
                        if !narrower(order, target, source.variant) {
                            problems.push(reject(format!(
                                "the role {target} isn't below {} by the privilege order",
                                source.variant
                            )));
                            break;
                        }
                    }
                }
                FillValue::Derived(derive) => {
                    for source in order.variants {
                        let target = derive(&role_record(step, sensitive.field, source.variant));
                        if !narrower(order, &target, source.variant) {
                            problems.push(reject(format!(
                                "the derived role {target} for {} holds a grant it lacks",
                                source.variant
                            )));
                        }
                    }
                }
            }
        }
        // a set of grants is filled with the empty set
        SensitiveKind::GrantSet => {
            if !constant.is_some_and(|value| EMPTY_SETS.contains(&value)) {
                problems.push(reject(
                    "a set of grants may only be filled with the empty set".to_owned(),
                ));
            }
        }
        // a denial is filled with its most restrictive value
        SensitiveKind::Denial { most_restrictive } => {
            if constant != Some(most_restrictive) {
                problems.push(reject(format!(
                    "a denial may only be filled with its most restrictive value {most_restrictive}"
                )));
            }
        }
        // a credential is cleared
        SensitiveKind::Credential => {
            if !constant.is_some_and(|value| CLEARED.contains(&value)) {
                problems.push(reject("a credential may only be cleared".to_owned()));
            }
        }
        // anything else gets the deny sentinel, unless a reviewed exception names the fill
        SensitiveKind::Other { deny } => {
            let denied = matches!(fill.value, FillValue::Sentinel(value) if value == deny);
            let excepted = artifacts.fill_exceptions.iter().any(|exception| {
                exception.step == step.id
                    && exception.field == fill.field
                    && !exception.review.trim().is_empty()
            });
            if !denied && !excepted {
                problems.push(reject(format!(
                    "only the deny sentinel {deny} or a reviewed exception is allowed"
                )));
            }
        }
    }
}

/// Lint one enum mapping on a sensitive field: only a role mapping, to a variant no more
/// privileged than the new one, is accepted
///
/// # Arguments
///
/// * `step` - The lossy step
/// * `mapping` - The mapping
/// * `sensitive` - The sensitive field it writes
/// * `orders` - The step's privilege orders
/// * `problems` - The problems found so far
fn lint_mapping(
    step: &StepDef,
    mapping: &EnumMapping,
    sensitive: SensitiveField,
    orders: &[PrivilegeOrder],
    problems: &mut Vec<String>,
) {
    // a mapping on a sensitive field must be a role mapping with a privilege order
    let SensitiveKind::Role { variants } = sensitive.kind else {
        problems.push(format!(
            "step {} maps a variant of {}, a sensitive field that isn't a role",
            step.id, mapping.field
        ));
        return;
    };
    let Some(order) = complete_order(orders, sensitive.field, variants, problems, step) else {
        return;
    };
    // the old variant may hold no grant the new one lacks
    if !narrower(order, mapping.old_variant, mapping.new_variant) {
        problems.push(format!(
            "step {} maps {} to {} on {}, which holds a grant {} lacks",
            step.id, mapping.new_variant, mapping.old_variant, mapping.field, mapping.new_variant
        ));
    }
}

/// Find a step's privilege order for a role field, reporting a missing one, a second one for the
/// same field, or one that lists a variant twice or leaves out a variant the field can hold
///
/// # Arguments
///
/// * `orders` - The step's privilege orders
/// * `field` - The role field
/// * `variants` - Every variant the field can hold
/// * `problems` - The problems found so far
/// * `step` - The step, for the message
fn complete_order<'a>(
    orders: &'a [PrivilegeOrder],
    field: &str,
    variants: &[&str],
    problems: &mut Vec<String>,
    step: &StepDef,
) -> Option<&'a PrivilegeOrder> {
    // the step must declare an order for the field
    let mut declared = orders.iter().filter(|order| order.field == field);
    let Some(order) = declared.next() else {
        problems.push(format!(
            "step {} maps or fills the role {field} without a privilege order",
            step.id
        ));
        return None;
    };
    // and only one, since the first found would decide the grants
    if declared.next().is_some() {
        problems.push(format!(
            "step {} declares more than one privilege order for {field}",
            step.id
        ));
        return None;
    }
    // the order lists each variant once, since its first entry would decide the variant's grants
    let mut listed = BTreeSet::new();
    let repeated = order
        .variants
        .iter()
        .filter(|variant| !listed.insert(variant.variant))
        .map(|variant| variant.variant)
        .collect::<BTreeSet<_>>();
    if !repeated.is_empty() {
        problems.push(format!(
            "step {}'s privilege order for {field} lists {} more than once",
            step.id,
            repeated.into_iter().collect::<Vec<_>>().join(", ")
        ));
        return None;
    }
    // and every variant the field can hold
    let missing = variants
        .iter()
        .filter(|variant| !listed.contains(*variant))
        .copied()
        .collect::<Vec<_>>();
    if missing.is_empty() {
        Some(order)
    } else {
        problems.push(format!(
            "step {}'s privilege order for {field} leaves out {}",
            step.id,
            missing.join(", ")
        ));
        None
    }
}

/// Whether a target variant's grants are a subset of a source variant's, both listed in the
/// privilege order
///
/// # Arguments
///
/// * `order` - The privilege order
/// * `target` - The variant written
/// * `source` - The variant it stands for
fn narrower(order: &PrivilegeOrder, target: &str, source: &str) -> bool {
    // a variant the order doesn't list is never accepted
    let grants = |name: &str| {
        order
            .variants
            .iter()
            .find(|variant| variant.variant == name)
            .map(|variant| variant.grants.iter().copied().collect::<BTreeSet<_>>())
    };
    match (grants(target), grants(source)) {
        (Some(target), Some(source)) => target.is_subset(&source),
        _ => false,
    }
}

/// Whether a role a lossy reverse fills is no broader than the role the record held
///
/// The role must be listed in the privilege order, and its grants a subset of the grants of the
/// role the record holds in the new layout when the order lists that role, or of every listed
/// role when it doesn't (the field was renamed, dropped, or holds something unknown).
///
/// # Arguments
///
/// * `order` - The privilege order of the role field
/// * `new` - The record in the new layout, before the reverse
/// * `field` - The role field
/// * `role` - The role the fill writes
pub(super) fn within_role(order: &PrivilegeOrder, new: &Record, field: &str, role: &str) -> bool {
    // a role the order doesn't list is never written
    let listed = |name: &str| order.variants.iter().any(|variant| variant.variant == name);
    if !listed(role) {
        return false;
    }
    // compare with the record's own role, or with every role when it holds none the order lists
    match new.text(field).filter(|source| listed(source)) {
        Some(source) => narrower(order, role, &source),
        None => order
            .variants
            .iter()
            .all(|source| narrower(order, role, source.variant)),
    }
}

/// Build a record holding one role, in the form of the step's family, to run a derived fill on
///
/// # Arguments
///
/// * `step` - The step whose transform names the family
/// * `field` - The role field
/// * `variant` - The role it holds
fn role_record(step: &StepDef, field: &str, variant: &str) -> Record {
    // a Scylla row holds the role as text, and anything else as a Redis field
    match step.transform.map(|transform| transform.family) {
        Some(Family::ScyllaTable(_)) => Record::Scylla {
            key: "lint".to_owned(),
            columns: [(field.to_owned(), Some(CqlValue::Text(variant.to_owned())))].into(),
        },
        _ => Record::Redis {
            key: "lint".to_owned(),
            fields: [(field.to_owned(), variant.to_owned())].into(),
        },
    }
}
