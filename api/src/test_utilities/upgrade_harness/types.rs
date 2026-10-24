//! The approvals, fixtures, checks, and injections an upgrade scenario is made of

use std::collections::BTreeMap;
use std::time::Duration;

use crate::models::schema::ObjectDrift;
use crate::models::upgrades::{
    BlockReason, DriftKind, Family, FieldPath, Record, SchemaBackend, StepState, UpgradeApproval,
};

/// A backup reference given with an approval, such as `snap-1`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupRef(pub String);

impl BackupRef {
    /// Name a backup
    ///
    /// # Arguments
    ///
    /// * `name` - The backup's reference
    #[must_use]
    pub fn new<T: Into<String>>(name: T) -> Self {
        // keep the name as given
        BackupRef(name.into())
    }
}

/// Where a lossy approval's confirmation code comes from
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmSource {
    /// The code the blocked step shows in the status right now: what an admin does
    Current,
    /// The code this scenario last gave for the same step, to check that reuse is refused
    Previous,
    /// A literal code, to check that a wrong or guessed code is refused
    Literal(String),
}

/// A fixture layout version
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Layout(pub u32);

/// A function that breaks a record in the way a corruption kind says, on a field if one is named
pub type CorruptFn = fn(Record, CorruptionKind, Option<&FieldPath>) -> Record;

/// How a family's fixture objects are built and read back
///
/// A scenario registers one for each family it seeds or verifies, since only the fixture's
/// author knows what a layout of the family looks like.
#[derive(Clone, Copy)]
pub struct FamilyFixture {
    /// Build the object with this index in a layout, with the optional fields shaped as the
    /// variation says (an empty variation leaves every optional field valid)
    pub generate: fn(Layout, usize, &Variation) -> Record,
    /// Find the layout an object is in, or `None` when no layout of the family holds it
    pub layout_of: fn(&Record) -> Option<Layout>,
    /// Break an object as a corruption kind says, when the family needs values other than the
    /// harness's defaults (see [`CorruptionKind::value`])
    pub corrupt: Option<CorruptFn>,
}

impl std::fmt::Debug for FamilyFixture {
    /// Write the fixture without its function pointers
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // function pointers say nothing useful, so only the optional hook's presence is shown
        f.debug_struct("FamilyFixture")
            .field("corrupt", &self.corrupt.is_some())
            .finish_non_exhaustive()
    }
}

/// Fixture objects to write before a step
#[derive(Debug, Clone)]
pub struct FixtureSeed {
    /// The family to write them in
    pub family: Family,
    /// The layout to write them in
    pub layout: Layout,
    /// How many to write
    pub count: usize,
    /// Optional-field variations to spread the objects over, so a migration meets each shape an
    /// optional field can take
    pub variants: Option<VariantSet>,
}

/// One shape a varied field takes in a generated fixture object
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FieldShape {
    /// The field holds a valid value
    Present,
    /// The field is absent
    Absent,
    /// The field is null
    Null,
    /// The field holds a value of the wrong type
    WrongType,
    /// The field holds an empty value (an empty string, list, or map)
    Empty,
    /// An enum field holds a variant no layout defines
    UnknownVariant,
}

/// The values one field takes in each of its shapes
#[derive(Debug, Clone, Copy)]
pub struct FieldValues {
    /// A valid value
    pub valid: &'static str,
    /// A value of the wrong type
    pub wrong_type: &'static str,
    /// An empty value
    pub empty: &'static str,
    /// A variant no layout defines
    pub unknown_variant: &'static str,
}

impl FieldShape {
    /// The stored text a field has in this shape, or `None` when the field is absent
    ///
    /// # Arguments
    ///
    /// * `values` - The values the field takes in each shape
    #[must_use]
    pub fn value(self, values: &FieldValues) -> Option<String> {
        // map each shape to the text the field stores
        match self {
            FieldShape::Present => Some(values.valid.to_owned()),
            FieldShape::Absent => None,
            FieldShape::Null => Some("null".to_owned()),
            FieldShape::WrongType => Some(values.wrong_type.to_owned()),
            FieldShape::Empty => Some(values.empty.to_owned()),
            FieldShape::UnknownVariant => Some(values.unknown_variant.to_owned()),
        }
    }
}

/// The shapes one generated object gives each of its varied fields
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Variation {
    /// Each varied field and its shape; a field not listed is valid
    pub shapes: BTreeMap<FieldPath, FieldShape>,
}

impl Variation {
    /// The shape a field takes, which is [`FieldShape::Present`] unless the variation says
    /// otherwise
    ///
    /// # Arguments
    ///
    /// * `field` - The field
    #[must_use]
    pub fn shape(&self, field: &FieldPath) -> FieldShape {
        // an unlisted field is valid
        self.shapes
            .get(field)
            .copied()
            .unwrap_or(FieldShape::Present)
    }
}

/// The optional-field variations a seed generates: every combination of `shapes` over up to
/// three `fields`, and pairwise coverage (every pair of two fields' shapes) above three
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariantSet {
    /// The fields varied, normally the transform's `optional` fields
    pub fields: Vec<FieldPath>,
    /// The shapes each field takes
    pub shapes: Vec<FieldShape>,
}

/// A function that judges every object of a family, giving the reason when they aren't as expected
pub type RecordsFn = fn(&[Record]) -> Result<(), String>;

/// A function that rewrites one object, the way the previous release's code would change it
pub type ModifyFn = fn(Record) -> Record;

/// A check that reads every object of a family and judges them with a function
#[derive(Clone, Copy)]
pub struct RecordsCheck {
    /// The family whose objects are read
    pub family: Family,
    /// What the check is about, named in the failure and the action log
    pub name: &'static str,
    /// Judges the objects, naming what is wrong
    pub check: RecordsFn,
}

impl std::fmt::Debug for RecordsCheck {
    /// Write the check without its function pointer
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // the function pointer says nothing useful, so the family and name stand for it
        f.debug_struct("RecordsCheck")
            .field("family", &self.family)
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// A check on the data, state, or history
#[derive(Debug, Clone)]
pub enum Check {
    /// Every fixture object is in the expected layout
    Fixtures(FixtureVerifier),
    /// A function judges every object of a family, for what a layout alone can't say
    Records(RecordsCheck),
    /// `schema_migration_issues` holds exactly these groups for a pass, with these object
    /// counts summed across workers
    Issues {
        /// The pass key: a family's status name, such as `redis:users`
        migration: String,
        /// The reason codes expected, each with its object count
        groups: Vec<(String, u64)>,
    },
    /// The probe reports exactly this drift
    Drift(Vec<ObjectDrift>),
    /// The applied history holds these steps, in this order, with these outcomes
    History(Vec<HistoryExpectation>),
    /// The recorded per-pass counts match
    Passes(PassCountCheck),
    /// The data equals what an earlier `Capture` recorded
    SameDataAs(String),
    /// The deployment's status does (or doesn't) carry a condition
    Condition {
        /// The condition's type
        kind: crate::models::upgrades::UpgradeConditionType,
        /// Whether the status must carry it
        present: bool,
    },
}

/// What fixture objects must look like after a step
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixtureVerifier {
    /// The family checked
    pub family: Family,
    /// The layout every object must be in
    pub layout: Layout,
    /// Fields that must hold their sentinel value instead of a value, with that value
    pub sentinels: Vec<(String, String)>,
    /// Objects that must still be flagged: each key must be among its issue group's samples
    pub flagged: Vec<String>,
}

impl FixtureVerifier {
    /// Expect every object of a family in a layout
    ///
    /// # Arguments
    ///
    /// * `family` - The family checked
    /// * `layout` - The layout every object must be in
    #[must_use]
    pub fn items_in(family: Family, layout: Layout) -> Self {
        // no sentinels and no flagged objects until the check asks for them
        FixtureVerifier {
            family,
            layout,
            sentinels: Vec::new(),
            flagged: Vec::new(),
        }
    }

    /// Also expect a field to hold its sentinel on every object
    ///
    /// # Arguments
    ///
    /// * `field` - The field
    /// * `value` - The sentinel it must hold
    #[must_use]
    pub fn with_sentinel<T: Into<String>, V: Into<String>>(mut self, field: T, value: V) -> Self {
        // add the field to those that must hold a sentinel
        self.sentinels.push((field.into(), value.into()));
        self
    }

    /// Also expect objects to be flagged still: each key must be among its issue group's samples
    /// and keep the layout it had
    ///
    /// # Arguments
    ///
    /// * `keys` - The flagged objects' keys
    #[must_use]
    pub fn with_flagged<T: Into<String>>(mut self, keys: Vec<T>) -> Self {
        // keep the keys as given
        self.flagged = keys.into_iter().map(Into::into).collect();
        self
    }
}

/// How often the passes of a family touched each of its objects, from the per-pass counts
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PassCountCheck {
    /// The family whose passes are summed
    pub family: Family,
    /// Reads per object
    pub reads_per_object: u64,
    /// Writes per object (0 for a satisfied step)
    pub writes_per_object: u64,
}

/// One entry the applied history must hold
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryExpectation {
    /// The step id
    pub step: String,
    /// The outcome recorded
    pub outcome: StepState,
    /// The backup reference recorded, when the step needed an approval
    pub backup: Option<String>,
    /// Whether the entry must record a lossy confirmation code; the code itself isn't compared
    pub confirmed: bool,
    /// Whether the entry must record the step's loss report
    pub loss: bool,
}

impl HistoryExpectation {
    /// Expect a step that needed no approval and lost nothing
    ///
    /// # Arguments
    ///
    /// * `step` - The step id
    /// * `outcome` - The outcome recorded
    #[must_use]
    pub fn new<T: Into<String>>(step: T, outcome: StepState) -> Self {
        // no backup, confirmation, or loss
        HistoryExpectation {
            step: step.into(),
            outcome,
            backup: None,
            confirmed: false,
            loss: false,
        }
    }

    /// Also expect the backup the approval named
    ///
    /// # Arguments
    ///
    /// * `backup` - The backup reference
    #[must_use]
    pub fn with_backup<T: Into<String>>(mut self, backup: T) -> Self {
        // record the reference
        self.backup = Some(backup.into());
        self
    }

    /// Also expect the entry to record a lossy confirmation code and the step's loss report
    #[must_use]
    pub fn lossy(mut self) -> Self {
        // a confirmed lossy step records both
        self.confirmed = true;
        self.loss = true;
        self
    }
}

/// A failure or drift injected between steps
#[derive(Debug, Clone)]
pub enum Injection {
    /// Overwrite fixture objects so no layout parses them, in a way that yields a known reason
    /// code
    CorruptObjects {
        /// The family the objects are in
        family: Family,
        /// The object keys to corrupt
        objects: Vec<String>,
        /// How to corrupt them
        kind: CorruptionKind,
        /// The field corrupted (`None` for a whole-record corruption such as malformed JSON)
        field: Option<FieldPath>,
    },
    /// Give one field the wrong type on every object of a family, so the migration meets a
    /// systematic break that forms one issue group
    SystematicBreak {
        /// The family broken
        family: Family,
        /// The field given the wrong type
        field: FieldPath,
    },
    /// Stop the next migration run after a batch
    InterruptAtBatch {
        /// The batch to stop after
        batch: u64,
        /// How it stops
        signal: Interrupt,
    },
    /// Make a backend unreachable for a while
    BackendOutage {
        /// The backend
        backend: SchemaBackend,
        /// How long
        duration: Duration,
    },
    /// Rewrite objects of a family the way the previous release's code would, such as an edit made
    /// after a downgrade
    Modify {
        /// The family the objects are in
        family: Family,
        /// The object keys to rewrite, or `None` for every object
        keys: Option<Vec<String>>,
        /// Rewrites one object
        with: ModifyFn,
    },
    /// Create drift of one kind on one object
    Drift {
        /// The kind of drift
        kind: DriftKind,
        /// The object to drift
        object: String,
    },
}

/// How `CorruptObjects` breaks an object, each giving its own reason code
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorruptionKind {
    /// Unparseable JSON: `malformed_json`
    MalformedJson,
    /// A required field removed: `missing_field:<field>`
    MissingField,
    /// An enum field set to an undefined variant: `unknown_variant:<field>`
    UnknownVariant,
    /// A field given the wrong type: `bad_type:<field>`
    BadType,
}

impl CorruptionKind {
    /// The text the harness writes into a field to break it this way, `None` for a kind that
    /// removes the field
    #[must_use]
    pub fn value(self) -> Option<&'static str> {
        // each kind writes its own kind of broken text
        match self {
            CorruptionKind::MalformedJson => Some("{\"broken"),
            CorruptionKind::MissingField => None,
            CorruptionKind::UnknownVariant => Some("__undefined__"),
            CorruptionKind::BadType => Some("[7]"),
        }
    }
}

/// How an injected interruption stops a run
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interrupt {
    /// SIGTERM: finish the batch, write the checkpoint, exit
    Term,
    /// SIGKILL: stop at once
    Kill,
}

/// What to restart in a cluster scenario
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartTarget {
    /// The operator's pod
    Operator,
    /// The running migration Job's pod
    JobPod,
}

/// Why a block reason was expected, for messages
///
/// # Arguments
///
/// * `reason` - The reason to render
#[must_use]
pub fn render_reason(reason: &BlockReason) -> String {
    // a reason's confirmation code is never part of what a scenario compares
    match reason {
        BlockReason::Approval => "approval".to_owned(),
        BlockReason::LossAcceptance { .. } => "loss acceptance".to_owned(),
        BlockReason::ConfirmationRefused { refusal, .. } => format!("confirmation {refusal:?}"),
        BlockReason::Drift { objects } => format!("drift on {}", objects.join(", ")),
        BlockReason::Unknown => "unknown".to_owned(),
    }
}

/// Whether two block reasons agree, ignoring any confirmation code
///
/// # Arguments
///
/// * `expected` - The reason a scenario expects
/// * `found` - The reason the status shows
#[must_use]
pub fn reasons_match(expected: &BlockReason, found: &BlockReason) -> bool {
    // a code inside the reason isn't compared, only that one is shown
    match (expected, found) {
        (BlockReason::LossAcceptance { .. }, BlockReason::LossAcceptance { confirm }) => {
            !confirm.is_empty()
        }
        (
            BlockReason::ConfirmationRefused { refusal: want, .. },
            BlockReason::ConfirmationRefused {
                refusal: got,
                confirm,
            },
        ) => want == got && !confirm.is_empty(),
        (want, got) => want == got,
    }
}

/// Build the approval of a lossy step: `{step, backup, accept_loss: true, confirm}`
///
/// # Arguments
///
/// * `step` - The lossy step
/// * `backup` - The backup the approval names, or `None` for an approval with no backup
/// * `code` - The confirmation code the approval repeats
#[must_use]
pub fn loss_approval(step: &str, backup: Option<&BackupRef>, code: String) -> UpgradeApproval {
    // a lossy approval names the step, a backup, the acceptance, and the code
    UpgradeApproval {
        step: step.to_owned(),
        backup: backup.map_or_else(String::new, |backup| backup.0.clone()),
        accept_loss: true,
        confirm: Some(code),
        ..UpgradeApproval::default()
    }
}
