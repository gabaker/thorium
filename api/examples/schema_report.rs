//! Reports how the schema this build declares differs from the latest revision's snapshot, and
//! writes revision snapshots
//!
//! A development tool for CI and for writing a release's migration commit; it ships in no
//! binary.
//!
//! Report the unreleased changes as SARIF (the findings never fail the run; errors do):
//!
//! ```text
//! cargo run -p thorium-api --example schema_report -- --sarif <file> [--against <snapshot>]
//! ```
//!
//! Each changed object is a warning naming its class and default step, and a breaking change no
//! catalog step migrates is a second warning. An informational change (a breaking or contract
//! change of an object whose every write sets a TTL, or the removal of an object registered as
//! legacy) is listed on its own line with no warning. `--against` compares with a snapshot file
//! instead of the latest catalog revision's committed snapshot.
//!
//! Write the committed snapshot of one or more revisions, printing the deployment fingerprint each
//! revision's catalog entry records in `schema`. A revision released before the schema registry
//! existed (`2026-10-v01`, `2026-10-v02`) gets the schema those releases created; any other gets
//! the schema this build declares:
//!
//! ```text
//! cargo run -p thorium-api --example schema_report -- --write-snapshot <revision> ...
//! ```

use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use thorium::models::schema::{
    BASELINE_REVISIONS, ChangeClass, ChangeKind, RevisionSnapshot, SchemaChange,
};
use thorium::models::upgrades::{CATALOG, RevisionId, SchemaBackend, latest};

/// The directory revision snapshots are committed in, relative to the repository root
const SNAPSHOT_DIR: &str = "api/schema/revisions";

/// The rule of a change the latest revision's snapshot doesn't have
const UNRELEASED_RULE: &str = "unreleased-schema-change";

/// The rule of a breaking change no catalog step migrates yet
const NO_MIGRATION_RULE: &str = "breaking-change-without-migration";

/// Tells whether a line declares an object, given the line before it
type LineMatcher = Box<dyn Fn(&str, &str) -> bool>;

/// What this run was asked to do
#[derive(Debug, Default)]
struct Options {
    /// Where to write the SARIF report
    sarif: Option<PathBuf>,
    /// A snapshot file to compare with instead of the latest revision's committed snapshot
    against: Option<PathBuf>,
    /// The revisions to write snapshots for
    write: Vec<String>,
}

/// Parse the command line
///
/// # Arguments
///
/// * `args` - The arguments after the program name
fn parse_args(args: impl Iterator<Item = String>) -> Result<Options, String> {
    // read each flag and its value
    let mut options = Options::default();
    let mut args = args.peekable();
    while let Some(flag) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--sarif" => options.sarif = Some(PathBuf::from(value()?)),
            "--against" => options.against = Some(PathBuf::from(value()?)),
            "--write-snapshot" => options.write.push(value()?),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    // a run either reports or writes snapshots
    if options.write.is_empty() == options.sarif.is_none() {
        return Err("pass either --sarif <file> or --write-snapshot <revision>".to_owned());
    }
    Ok(options)
}

/// The repository root, the parent of the api crate
fn repo_root() -> PathBuf {
    // the api crate sits directly below the repository root
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

/// Write the committed snapshot of each revision
///
/// A revision released before the schema registry existed gets the baseline snapshot. Any other
/// revision gets the declared schema with the layouts that revision wrote.
///
/// # Arguments
///
/// * `revisions` - The revisions to write snapshots for
fn write_snapshots(revisions: &[String]) -> Result<(), String> {
    // write each revision's snapshot in the order given
    for raw in revisions {
        // snapshot the revision's schema: the baseline for a revision released before the
        // registry existed, otherwise the declared schema with the layouts the revision wrote
        let revision = raw
            .parse::<RevisionId>()
            .map_err(|error| format!("{raw} isn't a revision id: {error}"))?;
        let snapshot = if BASELINE_REVISIONS.contains(&revision.as_str()) {
            RevisionSnapshot::baseline(revision)
        } else {
            RevisionSnapshot::released(revision)
        };
        // write it where the api crate embeds it from
        let path = repo_root()
            .join(SNAPSHOT_DIR)
            .join(format!("{}.json", snapshot.revision.as_str()));
        std::fs::write(&path, snapshot.to_file_text())
            .map_err(|error| format!("{} can't be written: {error}", path.display()))?;
        println!(
            "wrote {}; record schema: Some(\"{}\") in its catalog entry",
            path.display(),
            snapshot.deployment.as_str()
        );
    }
    Ok(())
}

/// Load the snapshot to compare with, and the repository path findings about removed objects
/// point at
///
/// # Arguments
///
/// * `against` - A snapshot file to use instead of the latest revision's committed snapshot
fn load_snapshot(against: Option<&Path>) -> Result<(RevisionSnapshot, String), String> {
    match against {
        // a snapshot file given on the command line
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .map_err(|error| format!("{} can't be read: {error}", path.display()))?;
            let snapshot: RevisionSnapshot = serde_json::from_str(&text)
                .map_err(|error| format!("{} isn't a snapshot: {error}", path.display()))?;
            snapshot
                .verify()
                .map_err(|error| format!("{} doesn't verify: {error:?}", path.display()))?;
            Ok((snapshot, path.display().to_string()))
        }
        // the latest catalog revision's committed snapshot
        None => {
            let revision = latest();
            let snapshot = RevisionSnapshot::committed(revision.as_str())
                .ok_or_else(|| format!("{} has no committed snapshot", revision.as_str()))?
                .map_err(|error| format!("{} has a bad snapshot: {error:?}", revision.as_str()))?;
            Ok((
                snapshot,
                format!("{SNAPSHOT_DIR}/{}.json", revision.as_str()),
            ))
        }
    }
}

/// Name a backend as the snapshots do
///
/// # Arguments
///
/// * `backend` - The backend to name
fn backend_name(backend: SchemaBackend) -> String {
    // the snapshots name backends by their serialized form
    serde_json::to_value(backend)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("{backend:?}"))
}

/// Name a change's class as the reports do
///
/// # Arguments
///
/// * `class` - The class to name
fn class_name(class: ChangeClass) -> &'static str {
    // a contract change is a kind of breaking change
    match class {
        ChangeClass::Additive => "Additive",
        ChangeClass::Breaking => "Breaking",
        ChangeClass::Contract => "Breaking (contract)",
        ChangeClass::Informational => "Informational",
    }
}

/// Describe what a change did
///
/// # Arguments
///
/// * `kind` - What changed
fn kind_text(kind: &ChangeKind) -> String {
    // describe each kind in a few words
    match kind {
        ChangeKind::Added => "added".to_owned(),
        ChangeKind::Removed => "removed".to_owned(),
        ChangeKind::TypeChanged { from, to } => format!("type changed from {from} to {to}"),
        ChangeKind::KeyChanged => "key changed".to_owned(),
        ChangeKind::MappingChanged => "mapping changed".to_owned(),
        ChangeKind::LayoutChanged {
            from,
            to,
            reads_old,
        } => {
            let reads = if *reads_old {
                "still reads"
            } else {
                "can't read"
            };
            format!("layout {from} to {to}, which {reads} the old layout")
        }
    }
}

/// Name the object or field a change is to
///
/// # Arguments
///
/// * `change` - The change
fn subject(change: &SchemaChange) -> String {
    // name the backend, the object, and the field when there is one
    let backend = backend_name(change.backend);
    match &change.field {
        Some(field) => format!("{backend} object `{}` field `{field}`", change.object),
        None => format!("{backend} object `{}`", change.object),
    }
}

/// Find the line declaring an object, as a repository path and a 1-based line
///
/// # Arguments
///
/// * `root` - The repository root
/// * `change` - The change whose object to find
fn locate(root: &Path, change: &SchemaChange) -> Option<(String, usize)> {
    // pick the files that declare the backend's objects and how each declaration reads
    let name = &change.object;
    let (files, matches): (Vec<String>, LineMatcher) = match change.backend {
        SchemaBackend::Scylla => {
            let dir = root.join("api/src/models/schema/scylla");
            let mut files = std::fs::read_dir(&dir)
                .ok()?
                .filter_map(Result::ok)
                .map(|entry| {
                    format!(
                        "api/src/models/schema/scylla/{}",
                        entry.file_name().to_string_lossy()
                    )
                })
                .collect::<Vec<_>>();
            files.sort();
            let attribute = format!("name = \"{name}\"");
            let matches = move |_: &str, line: &str| {
                (line.contains("#[thorium_table(") || line.contains("#[thorium_view("))
                    && line.contains(&attribute)
            };
            (files, Box::new(matches))
        }
        SchemaBackend::Elastic => {
            let matches = |_: &str, line: &str| line.contains("pub fn mappings(");
            (
                vec!["api/src/models/elastic.rs".to_owned()],
                Box::new(matches),
            )
        }
        SchemaBackend::Redis | SchemaBackend::S3 => {
            let backend = format!("backend: SchemaBackend::{:?},", change.backend);
            let family = format!("family: \"{name}\",");
            let matches = move |previous: &str, line: &str| {
                previous.contains(&backend) && line.contains(&family)
            };
            (
                vec!["api/src/models/upgrades/registry.rs".to_owned()],
                Box::new(matches),
            )
        }
        SchemaBackend::Unknown => return None,
    };
    // scan each file for the declaring line
    for file in files {
        let Ok(text) = std::fs::read_to_string(root.join(&file)) else {
            continue;
        };
        let mut previous = "";
        for (index, line) in text.lines().enumerate() {
            if matches(previous, line) {
                return Some((file, index + 1));
            }
            previous = line;
        }
    }
    None
}

/// Build one SARIF result
///
/// # Arguments
///
/// * `rule` - The result's rule
/// * `message` - The result's message
/// * `location` - The repository path and line the result points at
fn sarif_result(rule: &str, message: &str, location: &(String, usize)) -> Value {
    // a warning at the declaring line
    json!({
        "ruleId": rule,
        "level": "warning",
        "message": { "text": message },
        "locations": [{
            "physicalLocation": {
                "artifactLocation": { "uri": location.0 },
                "region": { "startLine": location.1 },
            }
        }],
    })
}

/// Report how the declared schema differs from the snapshot to compare with
///
/// # Arguments
///
/// * `sarif` - Where to write the SARIF report
/// * `against` - A snapshot file to compare with instead of the latest revision's
fn report(sarif: &Path, against: Option<&Path>) -> Result<(), String> {
    // diff the snapshot with the declared schema
    let (snapshot, snapshot_path) = load_snapshot(against)?;
    let changes = SchemaChange::unreleased(&snapshot);
    let root = repo_root();
    let steps = CATALOG
        .iter()
        .flat_map(|entry| entry.steps.iter())
        .collect::<Vec<_>>();
    let revision = snapshot.revision.as_str();
    // one warning per change, and a second for a breaking change nothing migrates; an
    // informational change is listed without a warning
    let mut results = Vec::new();
    let mut informational = 0;
    for change in &changes {
        if !change.class.warns() {
            informational += 1;
            println!(
                "info: Informational schema change since revision {revision}: {} ({}) needs no \
                 step, since its old data expires",
                subject(change),
                kind_text(&change.kind),
            );
            continue;
        }
        let location = locate(&root, change).unwrap_or_else(|| (snapshot_path.clone(), 1));
        let unreleased = format!(
            "Unreleased schema change since revision {revision}: {} ({}) is {}, default step {:?}",
            subject(change),
            kind_text(&change.kind),
            class_name(change.class),
            change.default_step,
        );
        println!("warning: {unreleased}");
        results.push(sarif_result(UNRELEASED_RULE, &unreleased, &location));
        if !change.is_covered(steps.iter().copied()) {
            let (lacks, needs) = if change.class == ChangeClass::Contract {
                ("a Data step", "")
            } else {
                ("a migration", " with a registered migration")
            };
            let missing = format!(
                "{} schema change without {lacks}: {} ({}) needs a {:?} step targeting it{needs}",
                class_name(change.class),
                subject(change),
                kind_text(&change.kind),
                change.default_step,
            );
            println!("warning: {missing}");
            results.push(sarif_result(NO_MIGRATION_RULE, &missing, &location));
        }
    }
    println!(
        "{} schema change(s) since revision {revision}, {informational} informational, {} \
         warning(s)",
        changes.len(),
        results.len()
    );
    // write the report
    let log = json!({
        "version": "2.1.0",
        "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
        "runs": [{
            "tool": {
                "driver": {
                    "name": "schema_report",
                    "rules": [
                        {
                            "id": UNRELEASED_RULE,
                            "shortDescription": {
                                "text": "The declared schema differs from the latest revision's snapshot"
                            },
                        },
                        {
                            "id": NO_MIGRATION_RULE,
                            "shortDescription": {
                                "text": "A breaking schema change has no Data step with a registered migration"
                            },
                        },
                    ],
                }
            },
            "results": results,
        }],
    });
    let text = serde_json::to_string_pretty(&log).map_err(|error| error.to_string())?;
    std::fs::write(sarif, text + "\n")
        .map_err(|error| format!("{} can't be written: {error}", sarif.display()))
}

/// Run the report or write the snapshots, exiting non-zero only on an error
fn main() -> ExitCode {
    // parse the command line
    let options = match parse_args(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::from(2);
        }
    };
    // do what was asked
    let result = match &options.sarif {
        Some(sarif) => report(sarif, options.against.as_deref()),
        None => write_snapshots(&options.write),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
