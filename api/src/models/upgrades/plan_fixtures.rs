//! Test-only fixture catalogs and transforms the planner's composition and fusion are tested on
//!
//! Users move through three layouts: layout 1 stores whether the dark theme is on in `dark`,
//! layout 2 stores the theme's name in `theme`, and layout 3 stores the theme and the optional
//! `ai` flag in a `settings` JSON object. Files move from unprefixed keys to keys under `v2/`.

use std::collections::BTreeMap;

use super::fixtures::{BASE, NEXT, RELEASE, STACK};
use super::{
    Catalog, CatalogEntry, Family, FieldPath, IssueCategory, IssueReason, MigrationPhase, Record,
    RecordClass, StepDef, StepKind, TransformDef, TransformIssue, UpgradeComponent,
};

/// The users family the fixture transforms rewrite
pub const USERS: Family = Family::Redis("users");

/// The files family the fixture transforms rewrite
pub const FILES: Family = Family::S3Prefix {
    bucket: "files",
    prefix: "",
};

/// The theme names a user may hold
const THEMES: [&str; 2] = ["dark", "light"];

/// Build a reason code
///
/// # Arguments
///
/// * `category` - The kind of problem
/// * `field` - The field's path
fn reason(category: IssueCategory, field: &str) -> IssueReason {
    // keep the category and the field as given
    IssueReason::new(category, field)
}

/// Build the issue of a transform that couldn't convert a record
///
/// # Arguments
///
/// * `reason` - Why
/// * `layout` - The layout the transform tried
fn issue(reason: IssueReason, layout: u32) -> TransformIssue {
    // name the one layout tried
    TransformIssue {
        reason,
        layouts_tried: vec![layout],
    }
}

/// Get the fields of a Redis record, or `None` for any other record
///
/// # Arguments
///
/// * `record` - The record to read
fn fields(record: &Record) -> Option<&BTreeMap<String, String>> {
    // only Redis records hold fields
    match record {
        Record::Redis { fields, .. } => Some(fields),
        _ => None,
    }
}

/// Parse a user's `settings` JSON into its theme and optional `ai` flag
///
/// # Arguments
///
/// * `raw` - The stored `settings` value
fn parse_settings(raw: &str) -> Result<(String, Option<bool>), IssueReason> {
    // the whole value must be a JSON object
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|_| reason(IssueCategory::MalformedJson, "settings"))?;
    let object = value
        .as_object()
        .ok_or_else(|| reason(IssueCategory::BadType, "settings"))?;
    // the theme must be a known theme name
    let theme = object
        .get("theme")
        .ok_or_else(|| reason(IssueCategory::MissingField, "settings.theme"))?
        .as_str()
        .ok_or_else(|| reason(IssueCategory::BadType, "settings.theme"))?;
    if !THEMES.contains(&theme) {
        return Err(reason(IssueCategory::UnknownVariant, "settings.theme"));
    }
    // the ai flag is optional but must be a boolean
    let ai = match object.get("ai") {
        None => None,
        Some(ai) => Some(
            ai.as_bool()
                .ok_or_else(|| reason(IssueCategory::BadType, "settings.ai"))?,
        ),
    };
    Ok((theme.to_owned(), ai))
}

/// Render a user's `settings` JSON with its keys in sorted order
///
/// # Arguments
///
/// * `theme` - The theme name
/// * `ai` - The optional `ai` flag
fn render_settings(theme: &str, ai: Option<bool>) -> String {
    // insert the keys in sorted order so every map implementation renders the same text
    let mut object = serde_json::Map::new();
    if let Some(ai) = ai {
        object.insert("ai".to_owned(), serde_json::Value::Bool(ai));
    }
    object.insert(
        "theme".to_owned(),
        serde_json::Value::String(theme.to_owned()),
    );
    serde_json::Value::Object(object).to_string()
}

/// Classify a user for [`USERS_V2`]: current in layout 2 or later, migratable in layout 1
///
/// # Arguments
///
/// * `record` - The user to classify
fn users_v2_classify(record: &Record) -> RecordClass {
    // only Redis users can be classified
    let Some(fields) = fields(record) else {
        return RecordClass::Unrecognized {
            reason: reason(IssueCategory::MalformedJson, ""),
        };
    };
    // a later layout is past this transform; layout 2 needs a known theme; layout 1 a flag
    let unrecognized = |category, field| RecordClass::Unrecognized {
        reason: reason(category, field),
    };
    if fields.contains_key("settings") {
        RecordClass::Current
    } else if let Some(theme) = fields.get("theme") {
        if THEMES.contains(&theme.as_str()) {
            RecordClass::Current
        } else {
            unrecognized(IssueCategory::UnknownVariant, "theme")
        }
    } else if let Some(dark) = fields.get("dark") {
        if dark == "true" || dark == "false" {
            RecordClass::Migratable { layout: 1 }
        } else {
            unrecognized(IssueCategory::InvalidValue, "dark")
        }
    } else {
        unrecognized(IssueCategory::MissingField, "dark")
    }
}

/// Upgrade a user for [`USERS_V2`], replacing `dark` with the theme's name
///
/// Whenever the user holds `dark`, the theme is derived from it, so a stale theme left beside a
/// flag changed after a reverse is replaced
///
/// # Arguments
///
/// * `record` - The user to upgrade
fn users_v2_up(mut record: Record) -> Result<Record, TransformIssue> {
    // a user holding the old flag is converted from it, even beside a stale theme; otherwise a
    // current user is left as it is, and an unrecognized one is an issue
    let flagged = fields(&record).is_some_and(|fields| {
        !fields.contains_key("settings")
            && matches!(
                fields.get("dark").map(String::as_str),
                Some("true" | "false")
            )
    });
    if !flagged {
        match users_v2_classify(&record) {
            RecordClass::Current => return Ok(record),
            RecordClass::Unrecognized { reason } => return Err(issue(reason, 1)),
            RecordClass::Migratable { .. } => (),
        }
    }
    // replace the flag with the theme it names
    if let Record::Redis { fields, .. } = &mut record {
        let dark = fields.remove("dark");
        let theme = if dark.as_deref() == Some("true") {
            "dark"
        } else {
            "light"
        };
        fields.insert("theme".to_owned(), theme.to_owned());
    }
    Ok(record)
}

/// Revert a user for [`USERS_V2`], replacing the theme's name with `dark`
///
/// # Arguments
///
/// * `record` - The user to revert
fn users_v2_down(mut record: Record) -> Result<Record, TransformIssue> {
    // only a user holding a known theme reverts
    let theme = fields(&record)
        .and_then(|fields| fields.get("theme"))
        .cloned();
    let dark = match theme.as_deref() {
        Some("dark") => "true",
        Some("light") => "false",
        Some(_) => return Err(issue(reason(IssueCategory::UnknownVariant, "theme"), 2)),
        None => return Err(issue(reason(IssueCategory::MissingField, "theme"), 2)),
    };
    // replace the theme with the flag it implies
    if let Record::Redis { fields, .. } = &mut record {
        fields.remove("theme");
        fields.insert("dark".to_owned(), dark.to_owned());
    }
    Ok(record)
}

/// Classify a user for [`USERS_V3`]: current with valid `settings`, migratable in layout 2, and
/// in layout 1 for an earlier transform
///
/// # Arguments
///
/// * `record` - The user to classify
fn users_v3_classify(record: &Record) -> RecordClass {
    // only Redis users can be classified
    let Some(fields) = fields(record) else {
        return RecordClass::Unrecognized {
            reason: reason(IssueCategory::MalformedJson, ""),
        };
    };
    // settings must parse; layout 2 needs a known theme; layout 1 is left to the previous link
    if let Some(settings) = fields.get("settings") {
        match parse_settings(settings) {
            Ok(_) => RecordClass::Current,
            Err(reason) => RecordClass::Unrecognized { reason },
        }
    } else if let Some(theme) = fields.get("theme") {
        if THEMES.contains(&theme.as_str()) {
            RecordClass::Migratable { layout: 2 }
        } else {
            RecordClass::Unrecognized {
                reason: reason(IssueCategory::UnknownVariant, "theme"),
            }
        }
    } else if fields.contains_key("dark") {
        RecordClass::Migratable { layout: 1 }
    } else {
        RecordClass::Unrecognized {
            reason: reason(IssueCategory::MissingField, "theme"),
        }
    }
}

/// Upgrade a user for [`USERS_V3`], moving the theme and the `ai` flag into `settings`
///
/// Whenever the user holds a flat theme, `settings` is derived from it, so stale settings left
/// beside a theme changed after a reverse are replaced
///
/// # Arguments
///
/// * `record` - The user to upgrade
fn users_v3_up(mut record: Record) -> Result<Record, TransformIssue> {
    // a user holding a known flat theme is converted from it, even beside stale settings
    let flat = fields(&record)
        .and_then(|fields| fields.get("theme"))
        .is_some_and(|theme| THEMES.contains(&theme.as_str()));
    // otherwise a current user is left as it is; only layout 2 converts
    match users_v3_classify(&record) {
        _ if flat => (),
        RecordClass::Current => return Ok(record),
        RecordClass::Unrecognized { reason } => return Err(issue(reason, 2)),
        RecordClass::Migratable { layout: 1 } => {
            return Err(issue(reason(IssueCategory::MissingField, "theme"), 2));
        }
        RecordClass::Migratable { .. } => (),
    }
    if let Record::Redis { fields, .. } = &mut record {
        // the ai flag is optional but must be a boolean
        let ai = match fields.get("ai").map(String::as_str) {
            None => None,
            Some("true") => Some(true),
            Some("false") => Some(false),
            Some(_) => return Err(issue(reason(IssueCategory::InvalidValue, "ai"), 2)),
        };
        // move both into settings
        let theme = fields.remove("theme").unwrap_or_default();
        fields.remove("ai");
        fields.insert("settings".to_owned(), render_settings(&theme, ai));
    }
    Ok(record)
}

/// Revert a user for [`USERS_V3`], moving the theme and the `ai` flag back out of `settings`
///
/// # Arguments
///
/// * `record` - The user to revert
fn users_v3_down(mut record: Record) -> Result<Record, TransformIssue> {
    // only a user with valid settings reverts
    let settings = fields(&record)
        .and_then(|fields| fields.get("settings"))
        .cloned()
        .ok_or_else(|| issue(reason(IssueCategory::MissingField, "settings"), 3))?;
    let (theme, ai) = parse_settings(&settings).map_err(|reason| issue(reason, 3))?;
    // write the flat fields layout 2 reads
    if let Record::Redis { fields, .. } = &mut record {
        fields.remove("settings");
        fields.insert("theme".to_owned(), theme);
        if let Some(ai) = ai {
            fields.insert("ai".to_owned(), ai.to_string());
        }
    }
    Ok(record)
}

/// Users from layout 1 to layout 2
pub const USERS_V2: TransformDef = TransformDef {
    family: USERS,
    reads: &[],
    from: 1,
    to: 2,
    classify: users_v2_classify,
    up: users_v2_up,
    down: Some(users_v2_down),
    optional: &[FieldPath("ai")],
};

/// Users from layout 2 to layout 3
pub const USERS_V3: TransformDef = TransformDef {
    family: USERS,
    reads: &[],
    from: 2,
    to: 3,
    classify: users_v3_classify,
    up: users_v3_up,
    down: Some(users_v3_down),
    optional: &[FieldPath("ai"), FieldPath("settings.ai")],
};

/// Users from layout 1 to layout 2, reading files (only the catalog check's cycle uses it)
pub const USERS_V2_READING_FILES: TransformDef = TransformDef {
    reads: &[FILES],
    ..USERS_V2
};

/// Classify a file for [`FILES_V2`]: current under `v2/`, migratable otherwise
///
/// # Arguments
///
/// * `record` - The file to classify
fn files_classify(record: &Record) -> RecordClass {
    // only S3 objects are files
    match record {
        Record::S3 { key } if key.starts_with("v2/") => RecordClass::Current,
        Record::S3 { .. } => RecordClass::Migratable { layout: 1 },
        _ => RecordClass::Unrecognized {
            reason: reason(IssueCategory::InvalidValue, ""),
        },
    }
}

/// Upgrade a file for [`FILES_V2`], moving its key under `v2/`
///
/// # Arguments
///
/// * `record` - The file to upgrade
fn files_up(record: Record) -> Result<Record, TransformIssue> {
    // move only files not yet under the prefix
    match (files_classify(&record), record) {
        (RecordClass::Migratable { .. }, Record::S3 { key }) => Ok(Record::S3 {
            key: format!("v2/{key}"),
        }),
        (RecordClass::Unrecognized { reason }, _) => Err(issue(reason, 1)),
        (_, record) => Ok(record),
    }
}

/// Files from unprefixed keys to keys under `v2/`
pub const FILES_V2: TransformDef = TransformDef {
    family: FILES,
    reads: &[],
    from: 1,
    to: 2,
    classify: files_classify,
    up: files_up,
    down: None,
    optional: &[],
};

/// Files from unprefixed keys to keys under `v2/`, reading users
pub const FILES_V2_READING_USERS: TransformDef = TransformDef {
    reads: &[USERS],
    ..FILES_V2
};

/// The procedure every fixture step that needs an approval names
const PROCEDURE: &str = "A fixture procedure";

/// Build a step with a kind and a phase
///
/// # Arguments
///
/// * `id` - The id of the step
/// * `kind` - What kind of step it is
/// * `phase` - Where it sits in expand, migrate, contract
const fn step(id: &'static str, kind: StepKind, phase: Option<MigrationPhase>) -> StepDef {
    StepDef {
        id,
        kind,
        description: "A fixture step",
        phase,
        manual_procedure: Some(PROCEDURE),
        ..StepDef::BASE
    }
}

/// Build a step that runs a transform
///
/// # Arguments
///
/// * `id` - The id of the step
/// * `transform` - The transform it runs
const fn transform(id: &'static str, transform: &'static TransformDef) -> StepDef {
    StepDef {
        transform: Some(transform),
        thoradm_migration: Some(id),
        ..step(id, StepKind::Data, Some(MigrationPhase::Migrate))
    }
}

/// Build an additive step
///
/// # Arguments
///
/// * `id` - The id of the step
const fn expand(id: &'static str) -> StepDef {
    step(id, StepKind::Infra, Some(MigrationPhase::Expand))
}

/// Build a contract step
///
/// # Arguments
///
/// * `id` - The id of the step
const fn contract(id: &'static str) -> StepDef {
    step(id, StepKind::Data, Some(MigrationPhase::Contract))
}

/// Build a rebuild step: a migration without a transform
///
/// # Arguments
///
/// * `id` - The id of the step
/// * `supersedes` - The steps it makes redundant
const fn rebuild(id: &'static str, supersedes: &'static [&'static str]) -> StepDef {
    StepDef {
        thoradm_migration: Some(id),
        supersedes,
        ..step(id, StepKind::Data, Some(MigrationPhase::Migrate))
    }
}

/// Build a fixture release edge
///
/// # Arguments
///
/// * `from` - The revision the edge starts from
/// * `id` - The revision the edge reaches
/// * `steps` - The edge's steps
const fn edge(
    from: Option<&'static str>,
    id: &'static str,
    steps: &'static [StepDef],
) -> CatalogEntry {
    CatalogEntry {
        from,
        id,
        released_with: "0.0.0",
        description: "A fixture edge",
        schema: None,
        prerelease: false,
        sibling: false,
        steps,
    }
}

/// Build a catalog of a baseline and two release edges, the first to [`RELEASE`] and the second
/// to [`NEXT`]
macro_rules! two_edges {
    ($first:expr, $second:expr) => {
        Catalog {
            edges: &[
                edge(None, BASE, &[]),
                edge(Some(BASE), RELEASE, $first),
                edge(Some(RELEASE), NEXT, $second),
            ],
            floors: &[],
        }
    };
}

/// Users move from layout 1 to 2 on the first edge and from 2 to 3 on the second
pub const FUSED: Catalog = two_edges!(
    &[transform("users-v2", &USERS_V2)],
    &[transform("users-v3", &USERS_V3)]
);

/// Users and files are rewritten on the same edge, neither reading the other
pub const INDEPENDENT: Catalog = two_edges!(
    &[
        transform("users-v2", &USERS_V2),
        transform("files-v2", &FILES_V2)
    ],
    &[]
);

/// Users are rewritten on the first edge, and files on the second by a transform reading users;
/// the second edge's users transform comes after the files transform
pub const READS: Catalog = two_edges!(
    &[transform("users-v2", &USERS_V2)],
    &[
        transform("files-v2", &FILES_V2_READING_USERS),
        transform("users-v3", &USERS_V3)
    ]
);

/// Users read files and files read users, a cycle the catalog check refuses
pub const CYCLE: Catalog = two_edges!(
    &[transform("users-v2", &USERS_V2_READING_FILES)],
    &[transform("files-v2", &FILES_V2_READING_USERS)]
);

/// A contract step sits between the two users transforms
pub const CONTRACT_BETWEEN: Catalog = two_edges!(
    &[transform("users-v2", &USERS_V2), contract("drop-dark")],
    &[transform("users-v3", &USERS_V3)]
);

/// A manual step sits between the two users transforms
pub const MANUAL_BETWEEN: Catalog = two_edges!(
    &[
        transform("users-v2", &USERS_V2),
        step("by-hand", StepKind::Manual, None)
    ],
    &[transform("users-v3", &USERS_V3)]
);

/// A rebuild sits between the two users transforms
pub const REBUILD_BETWEEN: Catalog = two_edges!(
    &[
        transform("users-v2", &USERS_V2),
        rebuild("rebuild-results", &[])
    ],
    &[transform("users-v3", &USERS_V3)]
);

/// The planner's worked example: the first edge adds a `samples_list` column, moves users from
/// layout 1 to 2, and rebuilds the results index; the second adds a table, moves users from
/// layout 2 to 3, moves files, and rebuilds the results index again, superseding the first
/// rebuild
pub const EXAMPLE: Catalog = two_edges!(
    &[
        expand("add-samples-column"),
        transform("users-v2", &USERS_V2),
        rebuild("rebuild-results-1", &[])
    ],
    &[
        expand("add-table"),
        transform("users-v3", &USERS_V3),
        transform("files-v2", &FILES_V2),
        rebuild("rebuild-results-2", &["rebuild-results-1"])
    ]
);

/// Expand, migrate, and contract steps spread over both edges: the first edge has a config step,
/// an additive step, the users transform, and a contract step dropping the old field; the second
/// another additive step and a check
pub const PHASES: Catalog = two_edges!(
    &[
        step("config-1", StepKind::Config, None),
        expand("add-theme-column"),
        transform("users-v2", &USERS_V2),
        contract("drop-dark")
    ],
    &[expand("add-table"), step("check-2", StepKind::Infra, None)]
);

/// Two steps run as is, one per edge, whose work the tests decide
pub const CHECKED: Catalog = two_edges!(
    &[step("reindex", StepKind::Data, None)],
    &[step("repair", StepKind::Infra, None)]
);

/// Steps tagged `Expand` that aren't additive schema: a config step, a data migration, and an
/// infra step with a migration, beside one additive step
pub const EXPAND_KINDS: Catalog = two_edges!(
    &[
        step(
            "expand-config",
            StepKind::Config,
            Some(MigrationPhase::Expand)
        ),
        StepDef {
            thoradm_migration: Some("expand-data"),
            ..step("expand-data", StepKind::Data, Some(MigrationPhase::Expand))
        },
        StepDef {
            thoradm_migration: Some("expand-backfill"),
            ..expand("expand-backfill")
        },
        expand("add-table")
    ],
    &[]
);

/// A chain of rebuilds, each superseding the one before it
pub const SUPERSEDES_CHAIN: Catalog = two_edges!(
    &[rebuild("rebuild-a", &[])],
    &[
        rebuild("rebuild-b", &["rebuild-a"]),
        rebuild("rebuild-c", &["rebuild-b"])
    ]
);

/// A diamond of rebuilds: two rebuilds supersede the first, and a last one supersedes both
pub const SUPERSEDES_DIAMOND: Catalog = two_edges!(
    &[
        rebuild("rebuild-a", &[]),
        rebuild("rebuild-b", &["rebuild-a"])
    ],
    &[
        rebuild("rebuild-c", &["rebuild-a"]),
        rebuild("rebuild-d", &["rebuild-b", "rebuild-c"])
    ]
);

/// Files whose check reads the first users transform's output, then the second users transform
pub const READS_AFTER: Catalog = two_edges!(
    &[transform("users-v2", &USERS_V2)],
    &[
        StepDef {
            reads_after: &["users-v2"],
            ..transform("files-v2", &FILES_V2)
        },
        transform("users-v3", &USERS_V3)
    ]
);

/// A step that needs the API running leads the second edge, after the first edge's reindex, which
/// pauses only the search streamer, and its rebuild, and before its users transform, which pauses
/// the API
pub const LEADS: Catalog = two_edges!(
    &[
        StepDef {
            pause: &[UpgradeComponent::SearchStreamer],
            ..rebuild("reindex", &[])
        },
        rebuild("rebuild-results", &[]),
        StepDef {
            pause: &[UpgradeComponent::Api],
            ..transform("users-v2", &USERS_V2)
        }
    ],
    &[
        StepDef {
            needs_running: &[UpgradeComponent::Api],
            ..step("wait-idle", StepKind::Data, None)
        },
        transform("users-v3", &USERS_V3)
    ]
);

/// Users from layout 2 to layout 3, reading users
pub const USERS_V3_READING_USERS: TransformDef = TransformDef {
    reads: &[USERS],
    ..USERS_V3
};

/// The two users transforms, the second reading its own family and naming the first in its
/// `reads_after`
pub const FUSED_SELF_READS: Catalog = two_edges!(
    &[transform("users-v2", &USERS_V2)],
    &[StepDef {
        reads_after: &["users-v2"],
        ..transform("users-v3", &USERS_V3_READING_USERS)
    }]
);

/// The two users transforms on stacked pre-releases of [`RELEASE`], each with a revert edge whose
/// step reverses it
pub const REVERSIBLE: Catalog = Catalog {
    edges: &[
        edge(None, BASE, &[]),
        edge(Some(BASE), RELEASE, &[]),
        CatalogEntry {
            prerelease: true,
            ..edge(Some(RELEASE), STACK[0], &[transform("users-v2", &USERS_V2)])
        },
        CatalogEntry {
            prerelease: true,
            ..edge(
                Some(STACK[0]),
                RELEASE,
                &[StepDef {
                    reverses: Some("users-v2"),
                    ..step("users-v2-revert", StepKind::Data, None)
                }],
            )
        },
        CatalogEntry {
            prerelease: true,
            ..edge(
                Some(STACK[0]),
                STACK[1],
                &[transform("users-v3", &USERS_V3)],
            )
        },
        CatalogEntry {
            prerelease: true,
            ..edge(
                Some(STACK[1]),
                STACK[0],
                &[StepDef {
                    reverses: Some("users-v3"),
                    ..step("users-v3-revert", StepKind::Data, None)
                }],
            )
        },
    ],
    floors: &[],
};

/// A user in a layout, or broken in a way a layout can't hold
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserVariant {
    /// Layout 1, `dark` set
    Layout1,
    /// Layout 2, `theme` set
    Layout2,
    /// Layout 3, `settings` set
    Layout3,
    /// Layout 1 with a `dark` that is neither `true` nor `false`
    BadDark,
    /// Layout 1 with an `ai` that is neither `true` nor `false`
    BadAi,
    /// Layout 2 with an unknown theme
    UnknownTheme,
    /// Layout 3 with `settings` that don't parse
    MalformedSettings,
    /// Layout 3 with a theme that isn't a string
    SettingsThemeType,
    /// No field of any layout
    Empty,
}

impl UserVariant {
    /// Every variant, the three layouts first
    pub const ALL: [UserVariant; 9] = [
        Self::Layout1,
        Self::Layout2,
        Self::Layout3,
        Self::BadDark,
        Self::BadAi,
        Self::UnknownTheme,
        Self::MalformedSettings,
        Self::SettingsThemeType,
        Self::Empty,
    ];
}

/// Build one user
///
/// # Arguments
///
/// * `index` - The user's number, used in its key
/// * `variant` - The layout or broken variant to build
/// * `dark` - Whether the user's theme is dark
/// * `ai` - The user's optional `ai` flag
#[must_use]
pub fn user(index: usize, variant: UserVariant, dark: bool, ai: Option<bool>) -> Record {
    // the theme's name and the ai flag in their flat forms
    let theme = if dark { "dark" } else { "light" };
    let ai_text = ai.map(|ai| ai.to_string());
    let mut fields = BTreeMap::new();
    let mut set = |key: &str, value: &str| {
        fields.insert(key.to_owned(), value.to_owned());
    };
    // set the fields of the variant
    match variant {
        UserVariant::Layout1 | UserVariant::BadAi => set("dark", &dark.to_string()),
        UserVariant::Layout2 => set("theme", theme),
        UserVariant::Layout3 => set("settings", &render_settings(theme, ai)),
        UserVariant::BadDark => set("dark", "maybe"),
        UserVariant::UnknownTheme => set("theme", "purple"),
        UserVariant::MalformedSettings => set("settings", "{\"theme\":"),
        UserVariant::SettingsThemeType => set("settings", "{\"theme\":7}"),
        UserVariant::Empty => (),
    }
    // flat layouts carry the ai flag as text, and the broken one carries a bad value
    match variant {
        UserVariant::BadAi => set("ai", "maybe"),
        UserVariant::Layout1 | UserVariant::Layout2 | UserVariant::BadDark => {
            if let Some(ai) = &ai_text {
                set("ai", ai);
            }
        }
        _ => (),
    }
    Record::Redis {
        key: format!("users:fixture-{index}"),
        fields,
    }
}

/// Generate many users of one variant, varying the theme and the optional `ai` flag with a
/// fixed pseudo-random sequence so every run sees the same users
///
/// # Arguments
///
/// * `variant` - The layout or broken variant to build
/// * `count` - How many users to build
#[must_use]
pub fn users(variant: UserVariant, count: usize) -> Vec<Record> {
    // step a linear congruential generator for each user's choices
    let mut seed: u64 = 0x5eed_u64;
    (0..count)
        .map(|index| {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let dark = seed >> 63 == 1;
            let ai = match (seed >> 61) & 0b11 {
                0 => None,
                1 => Some(true),
                _ => Some(false),
            };
            user(index, variant, dark, ai)
        })
        .collect()
}

/// Build files whose keys are unprefixed or already under `v2/`
///
/// # Arguments
///
/// * `count` - How many files of each form to build
#[must_use]
pub fn files(count: usize) -> Vec<Record> {
    // one unprefixed and one prefixed file per index
    (0..count)
        .flat_map(|index| {
            [
                Record::S3 {
                    key: format!("file-{index}"),
                },
                Record::S3 {
                    key: format!("v2/file-moved-{index}"),
                },
            ]
        })
        .collect()
}

/// Build a step that runs the Elastic rebuild, which pauses the search-streamer
///
/// # Arguments
///
/// * `id` - The id of the step
/// * `supersedes` - The steps it makes redundant
const fn elastic_rebuild(id: &'static str, supersedes: &'static [&'static str]) -> StepDef {
    StepDef {
        thoradm_migration: Some("elastic-rebuild"),
        pause: &[UpgradeComponent::SearchStreamer],
        supersedes,
        ..step(id, StepKind::Data, Some(MigrationPhase::Migrate))
    }
}

/// Two edges that each rebuild the Elastic indexes: the second's rebuild supersedes the first's
pub const ELASTIC_REBUILDS: Catalog = two_edges!(
    &[elastic_rebuild("elastic-rebuild-1", &[])],
    &[elastic_rebuild("elastic-rebuild-2", &["elastic-rebuild-1"])]
);
