//! Bulk imports rule files into Thorium and prints a compatibility report

use owo_colors::OwoColorize;
use regex::RegexSet;
use serde::Serialize;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::io::{IsTerminal, Write};
use std::path::Path;
use std::sync::Mutex;
use tabled::settings::object::Cell;
use tabled::settings::peaker::Priority;
use tabled::settings::{Color, Style, Width};
use tabled::{Table, Tabled};
use thorium::models::{EntityKinds, EntityListOpts, EntityMetadata, EntityRequest, EntityUpdate};
use thorium::{Error, Thorium};
use uuid::Uuid;

use super::{build_metadata, read_rule_file};
use crate::args::rules::{ImportRules, RuleEngine};
use crate::handlers::progress::Bar;
use crate::utils;

/// The outcome of importing a single rule file
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ImportStatus {
    /// This rule file was imported
    Imported,
    /// A rule with the same engine and identical source already exists in
    /// every target group, so this file was not re-imported
    AlreadyExists,
    /// A rule with the same engine and identical source already existed in
    /// some target groups, so it was extended into the missing ones instead
    /// of being duplicated
    Updated,
    /// This rule file failed to import
    Failed,
    /// This file was skipped because it is not a recognized rule file
    Skipped,
    /// This rule file passed local validation in a dry run
    Valid,
}

impl ImportStatus {
    /// Get the label to show for this status in the import report table
    fn label(self) -> &'static str {
        // map each status to its table label
        match self {
            ImportStatus::Imported => "IMPORTED",
            ImportStatus::AlreadyExists => "EXISTS",
            ImportStatus::Updated => "UPDATED",
            ImportStatus::Failed => "FAILED",
            ImportStatus::Skipped => "SKIPPED",
            ImportStatus::Valid => "VALID",
        }
    }

    /// Get the color to paint this status's table cell with
    fn color(self) -> Color {
        // map each status to its table cell color
        match self {
            ImportStatus::Imported => Color::FG_BRIGHT_GREEN,
            ImportStatus::AlreadyExists => Color::FG_BRIGHT_BLUE,
            ImportStatus::Updated => Color::FG_BRIGHT_GREEN,
            ImportStatus::Failed => Color::FG_BRIGHT_RED,
            ImportStatus::Skipped => Color::FG_BRIGHT_YELLOW,
            ImportStatus::Valid => Color::FG_BRIGHT_GREEN,
        }
    }
}

/// A single file's outcome in an import report
#[derive(Debug, Clone, Serialize)]
struct ImportRecord {
    /// The path of the file this record is for
    file: String,
    /// The rule entity name derived from the file name
    name: String,
    /// The outcome of importing this file
    status: ImportStatus,
    /// The full error if this file failed to import
    error: Option<String>,
    /// The single line message shown in the report table (the JSON report
    /// keeps the full `error` instead)
    #[serde(skip)]
    message: String,
}

impl ImportRecord {
    /// Create a new import record
    ///
    /// # Arguments
    ///
    /// * `path` - The path of the file this record is for
    /// * `name` - The rule entity name derived from the file name
    /// * `status` - The outcome of importing this file
    /// * `error` - The full error if this file failed to import
    /// * `message` - The single line message shown in the report table
    fn new(
        path: &Path,
        name: String,
        status: ImportStatus,
        error: Option<String>,
        message: String,
    ) -> Self {
        ImportRecord {
            file: path.to_string_lossy().to_string(),
            name,
            status,
            error,
            message,
        }
    }
}

/// Get the first line of an error's message for single-line table rows
///
/// # Arguments
///
/// * `error` - The error to get the first line of
fn first_line(error: &Error) -> String {
    // get this error's message or a placeholder
    let msg = utils::api_error_text(error).unwrap_or_else(|| "-".to_owned());
    // keep only the first line so table rows stay single-line
    msg.lines().next().unwrap_or("-").to_owned()
}

/// A row in the import report table
#[derive(Tabled)]
struct ImportRow {
    /// The outcome of importing this file
    #[tabled(rename = "STATUS")]
    status: String,
    /// The path of the file this row is for
    #[tabled(rename = "PATH")]
    path: String,
    /// The rule entity name derived from the file name
    #[tabled(rename = "NAME")]
    name: String,
    /// The single line message for this outcome
    #[tabled(rename = "MESSAGE")]
    message: String,
}

impl From<&ImportRecord> for ImportRow {
    /// Build a table row from an import record
    ///
    /// # Arguments
    ///
    /// * `record` - The import record to build a table row from
    fn from(record: &ImportRecord) -> Self {
        ImportRow {
            status: record.status.label().to_owned(),
            path: record.file.clone(),
            name: record.name.clone(),
            message: record.message.clone(),
        }
    }
}

/// Print the import report table
///
/// # Arguments
///
/// * `records` - The outcome of every file in this import
fn print_table(records: &[ImportRecord]) {
    // build a content sized table with one row per record
    let mut table = Table::new(records.iter().map(ImportRow::from));
    // match the style of thorctl's other tables
    table.with(Style::psql());
    // color each row's status cell by its outcome (row 0 is the header)
    for (row, record) in records.iter().enumerate() {
        table.modify(Cell::new(row + 1, 0), record.status.color());
    }
    // wrap long messages inside their cells when printing to a terminal
    if std::io::stdout().is_terminal()
        && let Ok((cols, _)) = crossterm::terminal::size()
    {
        // shrink the widest column first so short columns stay intact
        table.with(
            Width::wrap(cols as usize)
                .keep_words(true)
                .priority(Priority::max(true)),
        );
    }
    println!("{table}");
}

/// Build the engine-qualified key identifying a rule source
///
/// # Arguments
///
/// * `engine` - The engine this rule source targets
/// * `source` - The rule source text
fn source_key(engine: RuleEngine, source: &str) -> String {
    // qualify the source with its engine so identical text across engines stays distinct
    format!("{}:{source}", engine.as_str())
}

/// A rule source that already exists in Thorium and where it lives
struct ExistingRule {
    /// The id of the entity carrying this source
    ///
    /// Nil for sources reserved by an in-flight create earlier in this same
    /// import run, which carry every target group by construction.
    id: Uuid,
    /// The groups this source is already in
    groups: HashSet<String>,
}

/// Collect the engine-qualified sources of every rule already in some groups
///
/// The entities create route has no duplicate detection, so imports dedupe
/// client-side against the rules that already exist in the target groups. The
/// groups each source is in ride along so a source present in only part of
/// the target groups can be extended instead of skipped or duplicated.
///
/// # Arguments
///
/// * `thorium` - A Thorium client
/// * `groups` - The groups this import targets
async fn fetch_existing_sources(
    thorium: &Thorium,
    groups: &[String],
) -> Result<HashMap<String, ExistingRule>, Error> {
    // list every detection rule in the target groups
    let opts = EntityListOpts::default()
        .kinds([EntityKinds::YaraRule, EntityKinds::SigmaRule])
        .groups(groups.to_vec())
        .page_size(100);
    // get a cursor over these rules with their full details
    let mut cursor = thorium.entities.list_details(&opts).await?;
    // collect each rule's engine-qualified source and its groups
    let mut existing: HashMap<String, ExistingRule> = HashMap::default();
    loop {
        // key each of this page's rules by engine and source
        for entity in cursor.data.drain(..) {
            // keep this rules identity before its metadata is taken
            let id = entity.id;
            let groups: HashSet<String> = entity.groups.into_iter().collect();
            // key this rule by its engine and source
            let key = match entity.metadata {
                EntityMetadata::YaraRule(rule) => source_key(RuleEngine::Yara, &rule.rule),
                EntityMetadata::SigmaRule(rule) => source_key(RuleEngine::Sigma, &rule.rule),
                // only rule kinds were listed so other kinds cannot appear
                _ => continue,
            };
            // track where this source already lives
            match existing.entry(key) {
                // the same source in several entities: union their groups so
                // coverage checks see every group that already has it
                Entry::Occupied(mut occupied) => {
                    occupied.get_mut().groups.extend(groups);
                }
                // the first entity seen with this source defines its home
                Entry::Vacant(vacant) => {
                    vacant.insert(ExistingRule { id, groups });
                }
            }
        }
        // stop when the cursor is exhausted
        if cursor.exhausted() {
            break;
        }
        // get the next page of rules
        cursor.refill().await?;
    }
    Ok(existing)
}

/// Infer the rule engine for a file from its extension
///
/// # Arguments
///
/// * `path` - The path of the file to infer an engine for
fn infer_engine(path: &Path) -> Option<RuleEngine> {
    // get this file's extension as a utf-8 string
    let ext = path.extension()?.to_str()?;
    // map known rule extensions to their engines
    if ext.eq_ignore_ascii_case("yar") || ext.eq_ignore_ascii_case("yara") {
        Some(RuleEngine::Yara)
    } else if ext.eq_ignore_ascii_case("yml") || ext.eq_ignore_ascii_case("yaml") {
        Some(RuleEngine::Sigma)
    } else {
        None
    }
}

/// Import a single rule file into Thorium
///
/// # Arguments
///
/// * `thorium` - A Thorium client
/// * `cmd` - The import rules command to execute
/// * `path` - The path to the rule file to import
/// * `existing` - The engine-qualified sources already in Thorium and their groups
async fn import_file(
    thorium: &Thorium,
    cmd: &ImportRules,
    path: &Path,
    existing: &Mutex<HashMap<String, ExistingRule>>,
) -> ImportRecord {
    // derive this rule entity's name from the file name
    let name = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string());
    // use the forced engine or infer one from this file's extension
    let engine = match cmd.engine.or_else(|| infer_engine(path)) {
        Some(engine) => engine,
        None => {
            // this is not a recognized rule file so skip it
            let msg = "Not a recognized rule extension (.yar/.yara/.yml/.yaml)";
            return ImportRecord::new(
                path,
                name,
                ImportStatus::Skipped,
                Some(msg.to_owned()),
                msg.to_owned(),
            );
        }
    };
    // read this rule file's source
    let rule_src = match read_rule_file(path).await {
        Ok(rule_src) => rule_src,
        Err(error) => {
            // record that this file could not be read
            return ImportRecord::new(
                path,
                name,
                ImportStatus::Failed,
                utils::api_error_text(&error),
                first_line(&error),
            );
        }
    };
    // key this rule's source before the metadata build takes ownership of it
    let key = source_key(engine, &rule_src);
    // build this rule's metadata which also validates it locally
    let metadata = match build_metadata(engine, rule_src, &[], None) {
        Ok(metadata) => metadata,
        Err(error) => {
            // record that this rule failed local validation
            return ImportRecord::new(
                path,
                name,
                ImportStatus::Failed,
                utils::api_error_text(&error),
                first_line(&error),
            );
        }
    };
    // stop before any API calls on a dry run
    if cmd.dry_run {
        // record that this rule passed local validation
        return ImportRecord::new(
            path,
            name,
            ImportStatus::Valid,
            None,
            format!("Would import as a {} rule", engine.as_str()),
        );
    }
    // check this source against the rules already in Thorium (the create route
    // has no duplicate detection, so a re-import would silently duplicate the
    // rule); sources covered in only part of the target groups are extended
    // into the missing ones instead of being skipped or duplicated
    let extend: Option<(Uuid, Vec<String>)> = {
        // claim the duplicate map while checking and reserving this source
        let mut existing = existing.lock().expect("existing rule lock was poisoned");
        match existing.get(&key) {
            Some(rule) => {
                // collect the target groups this source is not in yet
                let missing: Vec<String> = cmd
                    .groups
                    .iter()
                    .filter(|group| !rule.groups.contains(*group))
                    .cloned()
                    .collect();
                // a source already in every target group needs nothing; a nil
                // id is a reservation from this run whose create carried every
                // target group, so twins of an in-flight create skip here too
                if missing.is_empty() || rule.id.is_nil() {
                    // record that an identical rule already exists
                    return ImportRecord::new(
                        path,
                        name,
                        ImportStatus::AlreadyExists,
                        None,
                        "-".to_owned(),
                    );
                }
                Some((rule.id, missing))
            }
            // this source is new so reserve it before the create round trip;
            // concurrent twins in this run then skip instead of double creating
            None => {
                existing.insert(
                    key.clone(),
                    ExistingRule {
                        id: Uuid::nil(),
                        groups: cmd.groups.iter().cloned().collect(),
                    },
                );
                None
            }
        }
    };
    // extend a partially covered rule into the target groups it is missing
    if let Some((id, missing)) = extend {
        // build an update that only adds the missing groups
        let mut update = EntityUpdate::default();
        for group in &missing {
            update = update.group(group.clone());
        }
        // send the group extension to Thorium
        return match thorium.entities.update(id, update).await {
            Ok(_) => {
                // remember the broader coverage so later twins skip cleanly
                if let Some(rule) = existing
                    .lock()
                    .expect("existing rule lock was poisoned")
                    .get_mut(&key)
                {
                    rule.groups.extend(missing.iter().cloned());
                }
                // record that this rule was extended into its missing groups
                ImportRecord::new(
                    path,
                    name,
                    ImportStatus::Updated,
                    None,
                    format!("added to groups: {}", missing.join(", ")),
                )
            }
            Err(error) => {
                // record the full error for the report and its first line for the table
                ImportRecord::new(
                    path,
                    name,
                    ImportStatus::Failed,
                    utils::api_error_text(&error),
                    first_line(&error),
                )
            }
        };
    }
    // build the entity request for this rule
    let req = EntityRequest::new(name.clone(), metadata, cmd.groups.clone());
    // try to create this rule in Thorium
    match thorium.entities.create(req).await {
        Ok(resp) => {
            // record that this rule was imported with its new id as the message
            ImportRecord::new(
                path,
                name,
                ImportStatus::Imported,
                None,
                resp.id.to_string(),
            )
        }
        Err(error) => {
            // release this sources reservation so an identical file later in
            // the run can retry instead of being misreported as EXISTS
            existing
                .lock()
                .expect("existing rule lock was poisoned")
                .remove(&key);
            // record the full error for the report and its first line for the table
            ImportRecord::new(
                path,
                name,
                ImportStatus::Failed,
                utils::api_error_text(&error),
                first_line(&error),
            )
        }
    }
}

/// Print the final compatibility report for an import
///
/// # Arguments
///
/// * `records` - The outcome of every file in this import
/// * `dry_run` - Whether this import was a dry run
fn print_report(records: &[ImportRecord], dry_run: bool) {
    // tally each outcome
    let imported = records
        .iter()
        .filter(|record| record.status == ImportStatus::Imported)
        .count();
    let valid = records
        .iter()
        .filter(|record| record.status == ImportStatus::Valid)
        .count();
    let exists = records
        .iter()
        .filter(|record| record.status == ImportStatus::AlreadyExists)
        .count();
    let updated = records
        .iter()
        .filter(|record| record.status == ImportStatus::Updated)
        .count();
    let failed = records
        .iter()
        .filter(|record| record.status == ImportStatus::Failed)
        .count();
    let skipped = records
        .iter()
        .filter(|record| record.status == ImportStatus::Skipped)
        .count();
    // separate the report from the per-file rows
    println!();
    // print the right summary for this run mode
    if dry_run {
        println!(
            "Compatibility report (dry run): {} valid, {} failed, {} skipped",
            valid.to_string().bright_green(),
            failed.to_string().bright_red(),
            skipped.to_string().bright_yellow()
        );
    } else {
        println!(
            "Compatibility report: {} imported, {} extended into new groups, {} already existed, {} failed, {} skipped",
            imported.to_string().bright_green(),
            updated.to_string().bright_green(),
            exists.to_string().bright_blue(),
            failed.to_string().bright_red(),
            skipped.to_string().bright_yellow()
        );
    }
    // print each failure's full error
    for record in records
        .iter()
        .filter(|record| record.status == ImportStatus::Failed)
    {
        // print which file failed
        println!("\n{}: {}", "Failed".bright_red(), record.file);
        // print this failure's full error indented
        if let Some(error) = &record.error {
            for line in error.lines() {
                println!("  {line}");
            }
        }
    }
}

/// Crawl the target rule files and import them into Thorium
///
/// # Arguments
///
/// * `thorium` - A Thorium client
/// * `cmd` - The import rules command to execute
pub(super) async fn import(thorium: &Thorium, cmd: &ImportRules) -> Result<(), Error> {
    // build the sets of regexes used to determine which files to include or skip
    let filter = RegexSet::new(&cmd.filter)?;
    let skip = RegexSet::new(&cmd.skip)?;
    // collect the rules already in the target groups so re-imports converge
    // instead of duplicating rules (skipped on dry runs which make no API calls)
    let existing = if cmd.dry_run {
        Mutex::new(HashMap::default())
    } else {
        Mutex::new(fetch_existing_sources(thorium, &cmd.groups).await?)
    };
    // show a progress bar while rule files import since rows only print at the end
    let bar = Bar::new_unbounded("Importing rules", "");
    // track every file's outcome for the compatibility report
    let records = Mutex::new(Vec::default());
    // crawl our targets and import each rule file
    utils::fs::process_async_walk(
        cmd.targets.iter().cloned(),
        |target| {
            // get references to our shared state that we can move into the async block
            let records_ref = &records;
            let existing_ref = &existing;
            let bar_ref = &bar;
            async move {
                // import this rule file
                let record = import_file(thorium, cmd, &target, existing_ref).await;
                // save this file's outcome for the compatibility report
                records_ref
                    .lock()
                    .expect("import records lock was poisoned")
                    .push(record);
                // tick the progress bar for this completed file
                bar_ref.inc(1);
            }
        },
        |path: &Path, error: &Error| {
            // record this walk error as a failure
            records
                .lock()
                .expect("import records lock was poisoned")
                .push(ImportRecord::new(
                    path,
                    path.to_string_lossy().to_string(),
                    ImportStatus::Failed,
                    utils::api_error_text(error),
                    first_line(error),
                ));
        },
        &filter,
        &skip,
        cmd.include_hidden,
        10,
    )
    .await;
    // clear the progress bar before rendering the report table
    bar.finish_and_clear();
    // unwrap our records now that all imports are done
    let mut records = records
        .into_inner()
        .expect("import records lock was poisoned");
    // sort by path so the report is deterministic regardless of completion order
    records.sort_unstable_by(|a, b| a.file.cmp(&b.file));
    // print the import report table
    print_table(&records);
    // print the compatibility report
    print_report(&records, cmd.dry_run);
    // write the JSON report if one was requested
    if let Some(report_path) = &cmd.report {
        // create the report file
        let mut report_file = std::fs::File::create(report_path)?;
        // write our records as pretty JSON
        serde_json::to_writer_pretty(&mut report_file, &records)?;
        writeln!(&mut report_file)?;
        // show where the report was written
        println!("Wrote import report to {}", report_path.to_string_lossy());
    }
    // count how many rule files failed to import
    let failed = records
        .iter()
        .filter(|record| record.status == ImportStatus::Failed)
        .count();
    // fail the command if any rule files failed
    if failed > 0 {
        return Err(Error::new(format!(
            "{failed} rule file(s) failed to import"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rule extensions must map to the right engine and everything else to none
    #[test]
    fn is_rule_ext() {
        // yara extensions map to the yara engine
        assert_eq!(
            infer_engine(Path::new("rules/demo.yar")),
            Some(RuleEngine::Yara)
        );
        assert_eq!(infer_engine(Path::new("demo.yara")), Some(RuleEngine::Yara));
        // extensions are matched case-insensitively
        assert_eq!(infer_engine(Path::new("DEMO.YAR")), Some(RuleEngine::Yara));
        // sigma extensions map to the sigma engine
        assert_eq!(
            infer_engine(Path::new("rules/win_susp.yml")),
            Some(RuleEngine::Sigma)
        );
        assert_eq!(
            infer_engine(Path::new("win_susp.YAML")),
            Some(RuleEngine::Sigma)
        );
        // other extensions are not rule files
        assert_eq!(infer_engine(Path::new("README.md")), None);
        assert_eq!(infer_engine(Path::new("rules.yar.txt")), None);
        // files without extensions are not rule files
        assert_eq!(infer_engine(Path::new("no_extension")), None);
        assert_eq!(infer_engine(Path::new(".yar")), None);
    }

    /// Statuses must map to short all-caps labels and their table colors
    #[test]
    fn status_labels_and_colors() {
        // labels must stay short and all-caps so the status column stays tight
        assert_eq!(ImportStatus::Imported.label(), "IMPORTED");
        assert_eq!(ImportStatus::AlreadyExists.label(), "EXISTS");
        assert_eq!(ImportStatus::Updated.label(), "UPDATED");
        assert_eq!(ImportStatus::Failed.label(), "FAILED");
        assert_eq!(ImportStatus::Skipped.label(), "SKIPPED");
        assert_eq!(ImportStatus::Valid.label(), "VALID");
        // each status paints its table cell with the right color
        assert_eq!(ImportStatus::Imported.color(), Color::FG_BRIGHT_GREEN);
        assert_eq!(ImportStatus::AlreadyExists.color(), Color::FG_BRIGHT_BLUE);
        assert_eq!(ImportStatus::Failed.color(), Color::FG_BRIGHT_RED);
        assert_eq!(ImportStatus::Skipped.color(), Color::FG_BRIGHT_YELLOW);
        assert_eq!(ImportStatus::Valid.color(), Color::FG_BRIGHT_GREEN);
    }

    /// The import report table must size columns to content and stay rectangular
    #[test]
    fn table_sizes_columns_to_content() {
        // build records whose cell widths differ wildly
        let records = vec![
            ImportRecord::new(
                Path::new("rules/import/demo_persistence_check.yar"),
                "demo_persistence_check".to_owned(),
                ImportStatus::Imported,
                None,
                "58e4c799-8e06-4ff2-a31c-9a3bb3210768".to_owned(),
            ),
            ImportRecord::new(
                Path::new("rules/x.yar"),
                "x".to_owned(),
                ImportStatus::Skipped,
                Some("not a rule".to_owned()),
                "not a rule".to_owned(),
            ),
        ];
        // render these records the same way the report table does
        let table = Table::new(records.iter().map(ImportRow::from))
            .with(Style::psql())
            .to_string();
        // every line must be the same width since columns are content sized
        let lines: Vec<&str> = table.lines().collect();
        let width = lines[0].chars().count();
        assert!(lines.iter().all(|line| line.chars().count() == width));
        // the longest cell must fit inside its column without truncation
        assert!(table.contains("rules/import/demo_persistence_check.yar"));
        assert!(table.contains("58e4c799-8e06-4ff2-a31c-9a3bb3210768"));
        // the header labels must all be present
        for label in ["STATUS", "PATH", "NAME", "MESSAGE"] {
            assert!(table.contains(label));
        }
    }

    /// Import records must sort by path for a deterministic report
    #[test]
    fn records_sort_by_path() {
        // build records in a shuffled completion order
        let mut records = vec![
            ImportRecord::new(
                Path::new("rules/b.yar"),
                "b".to_owned(),
                ImportStatus::Imported,
                None,
                "-".to_owned(),
            ),
            ImportRecord::new(
                Path::new("rules/a.yar"),
                "a".to_owned(),
                ImportStatus::Failed,
                Some("boom".to_owned()),
                "boom".to_owned(),
            ),
        ];
        // sort by path exactly like the import report does
        records.sort_unstable_by(|a, b| a.file.cmp(&b.file));
        // the report must now list records in path order
        assert_eq!(records[0].file, "rules/a.yar");
        assert_eq!(records[1].file, "rules/b.yar");
    }
}
