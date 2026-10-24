//! Test-only fixture catalogs: revision graphs the planner and the catalog checks are tested on
//! without touching the catalog this build deploys

use super::{Catalog, CatalogEntry, StepDef, StepKind, SupportFloor};

#[cfg(feature = "scylla-utils")]
use super::{
    Family, IssueCategory, IssueReason, Record, RecordClass, TransformDef, TransformIssue,
};

/// The baseline every fixture catalog starts from
pub const BASE: &str = "2026-10-v01";

/// The release the fixture catalogs' feature branch builds on
pub const RELEASE: &str = "2026-10-v02";

/// The next release after [`RELEASE`]
pub const NEXT: &str = "2026-10-v03";

/// The stacked pre-releases of the fixture branch on [`RELEASE`], in stack order
pub const STACK: [&str; 5] = [
    "2026-10-v02-pre.stack-1",
    "2026-10-v02-pre.stack-2",
    "2026-10-v02-pre.stack-3",
    "2026-10-v02-pre.stack-4",
    "2026-10-v02-pre.stack-5",
];

/// The sibling pre-release that reaches the stack's last depth by a second path from its fourth
pub const SIBLING: &str = "2026-10-v02-pre.stack-5b";

/// Build a step of a forward edge with every other field at its default
///
/// # Arguments
///
/// * `id` - The id of the step
const fn forward(id: &'static str) -> StepDef {
    StepDef {
        id,
        kind: StepKind::Config,
        description: "A fixture step",
        ..StepDef::BASE
    }
}

/// Build a step of a downward edge that undoes a forward step
///
/// # Arguments
///
/// * `id` - The id of the step
/// * `reverses` - The id of the forward step it undoes
const fn revert(id: &'static str, reverses: &'static str) -> StepDef {
    StepDef {
        id,
        kind: StepKind::Config,
        description: "A fixture revert step",
        reverses: Some(reverses),
        ..StepDef::BASE
    }
}

/// Build a fixture edge
///
/// # Arguments
///
/// * `from` - The revision the edge starts from
/// * `id` - The revision the edge reaches
/// * `prerelease` - Whether the edge belongs to a feature branch
/// * `steps` - The edge's steps
const fn edge(
    from: Option<&'static str>,
    id: &'static str,
    prerelease: bool,
    steps: &'static [StepDef],
) -> CatalogEntry {
    CatalogEntry {
        from,
        id,
        released_with: "0.0.0",
        description: "A fixture edge",
        schema: None,
        prerelease,
        sibling: false,
        steps,
    }
}

/// The baseline edge of every fixture catalog
const BASELINE: CatalogEntry = edge(None, BASE, false, &[]);

/// The release edge to [`RELEASE`]
const RELEASE_EDGE: CatalogEntry = edge(Some(BASE), RELEASE, false, &[forward("release-step")]);

/// A catalog with one release edge after its baseline
pub const ONE_EDGE: Catalog = Catalog {
    edges: &[BASELINE, RELEASE_EDGE],
    floors: &[],
};

/// A catalog with two release edges and a skip edge over both
pub const SKIP: Catalog = Catalog {
    edges: &[
        BASELINE,
        RELEASE_EDGE,
        edge(Some(RELEASE), NEXT, false, &[forward("next-step")]),
        edge(Some(BASE), NEXT, false, &[forward("skip-step")]),
    ],
    floors: &[],
};

/// Build a step of a forward edge of the given kind that needs an approval, with a manual
/// procedure
///
/// # Arguments
///
/// * `id` - The id of the step
/// * `kind` - What kind of step it is
/// * `manual_procedure` - How to perform the step by hand
const fn approved(
    id: &'static str,
    kind: StepKind,
    manual_procedure: Option<&'static str>,
) -> StepDef {
    StepDef {
        id,
        kind,
        description: "A fixture step that needs an approval",
        manual_procedure,
        ..StepDef::BASE
    }
}

/// The id of the one step of the manual and data procedure catalogs
pub const PROCEDURE_STEP: &str = "approved-step";

/// A catalog whose one manual step says how to do it by hand
pub const MANUAL_PROCEDURE: Catalog = Catalog {
    edges: &[
        BASELINE,
        edge(
            Some(BASE),
            RELEASE,
            false,
            &[approved(
                PROCEDURE_STEP,
                StepKind::Manual,
                Some("Scale the API to zero, then run the fixture command"),
            )],
        ),
    ],
    floors: &[],
};

/// A catalog whose one manual step has no manual procedure
pub const MANUAL_NO_PROCEDURE: Catalog = Catalog {
    edges: &[
        BASELINE,
        edge(
            Some(BASE),
            RELEASE,
            false,
            &[approved(PROCEDURE_STEP, StepKind::Manual, None)],
        ),
    ],
    floors: &[],
};

/// A catalog whose one manual step has an empty manual procedure
pub const MANUAL_EMPTY_PROCEDURE: Catalog = Catalog {
    edges: &[
        BASELINE,
        edge(
            Some(BASE),
            RELEASE,
            false,
            &[approved(PROCEDURE_STEP, StepKind::Manual, Some(""))],
        ),
    ],
    floors: &[],
};

/// A catalog whose one manual step has a manual procedure of only whitespace
pub const MANUAL_BLANK_PROCEDURE: Catalog = Catalog {
    edges: &[
        BASELINE,
        edge(
            Some(BASE),
            RELEASE,
            false,
            &[approved(PROCEDURE_STEP, StepKind::Manual, Some(" \t\n "))],
        ),
    ],
    floors: &[],
};

/// A catalog whose one data step has a manual procedure of only whitespace
pub const DATA_BLANK_PROCEDURE: Catalog = Catalog {
    edges: &[
        BASELINE,
        edge(
            Some(BASE),
            RELEASE,
            false,
            &[approved(PROCEDURE_STEP, StepKind::Data, Some("   "))],
        ),
    ],
    floors: &[],
};

/// The oldest revision the floored catalog plans from, the baseline its older revisions were
/// squashed into
pub const FLOOR: &str = "2027-01-v01";

/// The floors of the floored catalog: raised once by Thorium 1.6.0 and again by Thorium 2.0.0
pub const FLOORS: &[SupportFloor] = &[
    SupportFloor {
        minimum: "2026-01-v01",
        intermediate: "1.5.0",
        since_version: "1.6.0",
    },
    SupportFloor {
        minimum: FLOOR,
        intermediate: "1.9.0",
        since_version: "2.0.0",
    },
];

/// A catalog whose floor was raised twice, with the four revisions and twelve months its last
/// floor must leave above it
pub const FLOORED: Catalog = Catalog {
    edges: &[
        edge(None, FLOOR, false, &[]),
        edge(Some(FLOOR), "2027-04-v01", false, &[forward("april-step")]),
        edge(
            Some("2027-04-v01"),
            "2027-07-v01",
            false,
            &[forward("july-step")],
        ),
        edge(
            Some("2027-07-v01"),
            "2027-10-v01",
            false,
            &[forward("october-step")],
        ),
        CatalogEntry {
            released_with: "2.0.0",
            ..edge(
                Some("2027-10-v01"),
                "2028-01-v01",
                false,
                &[forward("january-step")],
            )
        },
    ],
    floors: FLOORS,
};

/// The steps of the first pre-release, which rewrites users with a transform
const STACK_1_STEPS: &[StepDef] = &[StepDef {
    #[cfg(feature = "scylla-utils")]
    transform: Some(&THEME),
    ..forward("stack-1-step")
}];

/// A feature branch on [`RELEASE`]: five stacked pre-releases, a sibling of the fifth from the
/// fourth, and a revert edge for each, as the commit series testing upgrades lays them out
pub const SERIES: Catalog = Catalog {
    edges: &[
        BASELINE,
        RELEASE_EDGE,
        edge(Some(RELEASE), STACK[0], true, STACK_1_STEPS),
        edge(
            Some(STACK[0]),
            RELEASE,
            true,
            &[revert("stack-1-revert", "stack-1-step")],
        ),
        edge(Some(STACK[0]), STACK[1], true, &[forward("stack-2-step")]),
        edge(
            Some(STACK[1]),
            STACK[0],
            true,
            &[revert("stack-2-revert", "stack-2-step")],
        ),
        edge(Some(STACK[1]), STACK[2], true, &[forward("stack-3-step")]),
        edge(
            Some(STACK[2]),
            STACK[1],
            true,
            &[revert("stack-3-revert", "stack-3-step")],
        ),
        edge(Some(STACK[2]), STACK[3], true, &[forward("stack-4-step")]),
        edge(
            Some(STACK[3]),
            STACK[2],
            true,
            &[revert("stack-4-revert", "stack-4-step")],
        ),
        edge(Some(STACK[3]), STACK[4], true, &[forward("stack-5-step")]),
        edge(
            Some(STACK[4]),
            STACK[3],
            true,
            &[revert("stack-5-revert", "stack-5-step")],
        ),
        CatalogEntry {
            sibling: true,
            ..edge(Some(STACK[3]), SIBLING, true, &[forward("stack-5b-step")])
        },
        edge(
            Some(SIBLING),
            STACK[3],
            true,
            &[revert("stack-5b-revert", "stack-5b-step")],
        ),
    ],
    floors: &[],
};

/// The pre-release of the paired catalog, whose edge carries two forward steps
pub const PAIR: &str = "2026-10-v02-pre.pair";

/// A feature branch on [`RELEASE`] with one pre-release edge of two forward steps, and a revert
/// edge undoing each of them on its own
pub const PAIRED: Catalog = Catalog {
    edges: &[
        BASELINE,
        RELEASE_EDGE,
        edge(
            Some(RELEASE),
            PAIR,
            true,
            &[forward("pair-first-step"), forward("pair-second-step")],
        ),
        edge(
            Some(PAIR),
            RELEASE,
            true,
            &[
                revert("pair-second-revert", "pair-second-step"),
                revert("pair-first-revert", "pair-first-step"),
            ],
        ),
    ],
    floors: &[],
};

/// The pre-release of the converting catalog, which a branch builds on [`RELEASE`]
pub const CONVERT: &str = "2026-10-v02-pre.convert";

/// A feature branch whose pre-release converts into the release after its base: the edge from
/// the pre-release to [`NEXT`] holds the branch's steps, and an edge back undoes them
pub const CONVERTING: Catalog = Catalog {
    edges: &[
        BASELINE,
        RELEASE_EDGE,
        edge(Some(RELEASE), CONVERT, true, &[]),
        edge(Some(CONVERT), RELEASE, true, &[]),
        edge(Some(CONVERT), NEXT, true, &[forward("convert-step")]),
        edge(
            Some(NEXT),
            CONVERT,
            true,
            &[revert("convert-revert", "convert-step")],
        ),
    ],
    floors: &[],
};

/// A pre-release of the release after [`RELEASE`], which a pre-release of [`RELEASE`] must not
/// reach
pub const ACROSS: &str = "2026-10-v03-pre.across";

/// A branch whose edge leads from a pre-release of one base to a pre-release of another
pub const CROSSING: Catalog = Catalog {
    edges: &[
        BASELINE,
        RELEASE_EDGE,
        edge(Some(RELEASE), CONVERT, true, &[]),
        edge(Some(CONVERT), RELEASE, true, &[]),
        edge(Some(CONVERT), ACROSS, true, &[forward("cross-step")]),
        edge(
            Some(ACROSS),
            CONVERT,
            true,
            &[revert("cross-revert", "cross-step")],
        ),
    ],
    floors: &[],
};

/// The users transform of the first pre-release: layout 1 stores whether the dark theme is on in
/// `dark`, layout 2 adds the theme's name in `theme`, derived from `dark`
#[cfg(feature = "scylla-utils")]
pub const THEME: TransformDef = TransformDef {
    family: Family::Redis("users"),
    reads: &[],
    from: 1,
    to: 2,
    classify: theme_classify,
    up: theme_up,
    down: Some(theme_down),
    optional: &[],
};

/// Classify a user for [`THEME`]: current once it has a theme, migratable when it only has `dark`
///
/// # Arguments
///
/// * `record` - The user to classify
#[cfg(feature = "scylla-utils")]
fn theme_classify(record: &Record) -> RecordClass {
    // only Redis users with a dark flag can be classified
    match record {
        Record::Redis { fields, .. } if fields.contains_key("theme") => RecordClass::Current,
        Record::Redis { fields, .. } if fields.contains_key("dark") => {
            RecordClass::Migratable { layout: 1 }
        }
        _ => RecordClass::Unrecognized {
            reason: IssueReason::new(IssueCategory::MissingField, "dark"),
        },
    }
}

/// Upgrade a user for [`THEME`], deriving `theme` from `dark`, which stays authoritative
///
/// # Arguments
///
/// * `record` - The user to upgrade
#[cfg(feature = "scylla-utils")]
fn theme_up(mut record: Record) -> Result<Record, TransformIssue> {
    // read the dark flag, which the base code keeps writing
    let issue = || TransformIssue {
        reason: IssueReason::new(IssueCategory::MissingField, "dark"),
        layouts_tried: vec![1],
    };
    let Record::Redis { fields, .. } = &mut record else {
        return Err(issue());
    };
    let theme = match fields.get("dark").map(String::as_str) {
        Some("true") => "dark",
        Some("false") => "light",
        _ => return Err(issue()),
    };
    // write the theme derived from it, replacing any stale one
    fields.insert("theme".to_owned(), theme.to_owned());
    Ok(record)
}

/// Revert a user for [`THEME`], deriving `dark` from `theme` and leaving `theme` in place
///
/// # Arguments
///
/// * `record` - The user to revert
#[cfg(feature = "scylla-utils")]
fn theme_down(mut record: Record) -> Result<Record, TransformIssue> {
    // read the theme the upgraded code wrote
    let issue = || TransformIssue {
        reason: IssueReason::new(IssueCategory::MissingField, "theme"),
        layouts_tried: vec![2],
    };
    let Record::Redis { fields, .. } = &mut record else {
        return Err(issue());
    };
    let dark = match fields.get("theme").map(String::as_str) {
        Some("dark") => "true",
        Some("light") => "false",
        _ => return Err(issue()),
    };
    // write the dark flag the base code reads
    fields.insert("dark".to_owned(), dark.to_owned());
    Ok(record)
}

/// Build the users [`THEME`] is tested on, in its `from` layout
#[cfg(feature = "scylla-utils")]
#[must_use]
pub fn theme_fixtures() -> Vec<Record> {
    // one user with each value of the dark flag
    ["true", "false"]
        .into_iter()
        .enumerate()
        .map(|(index, dark)| Record::Redis {
            key: format!("users:fixture-{index}"),
            fields: [("dark".to_owned(), dark.to_owned())].into(),
        })
        .collect()
}
