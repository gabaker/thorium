//! The materialized views a restore drops before loading their base tables and recreates after
//!
//! Before the first table is loaded, restore asks Scylla for the `CREATE MATERIALIZED VIEW`
//! statement of every view the coverage registry lists for the tables it restores (server-side
//! `DESCRIBE`) and saves them to `views.cql` in its report directory. Each table's views are dropped
//! just before its rows are written, and once every table is loaded the statements are run again
//! from memory, each on its own, and the restore waits for the views to build. A view an earlier,
//! interrupted restore left dropped is taken from the newest earlier `views.cql` that holds its
//! statement for the target's keyspace.

use scylla::client::session::Session;
use scylla::statement::Consistency;
use scylla::statement::unprepared::Statement;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tracing::{info, warn};

use super::coverage::Registry;
use super::manifest::write_atomic;
use super::sink::SinkFactory;
use crate::Error;

/// The name of the file a restore saves its view statements in, in its report directory
pub const VIEWS_FILE: &str = "views.cql";

/// How often the build of recreated views is checked
const BUILD_POLL: Duration = Duration::from_secs(1);

/// How often a long view build is logged
const BUILD_LOG: Duration = Duration::from_secs(30);

/// The statement of each view a restore recreates, by view name
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ViewStatements {
    /// Each view's `CREATE MATERIALIZED VIEW` statement, by view name
    pub statements: BTreeMap<String, String>,
}

/// Returns the views the registry lists for some tables, in registry order
///
/// # Arguments
///
/// * `tables` - The base tables being restored
#[must_use]
pub fn registered_views(tables: &[&str]) -> Vec<&'static str> {
    // every view whose base table is restored
    tables
        .iter()
        .flat_map(|table| Registry::views_of(table))
        .collect()
}

/// Returns the names of the views a keyspace holds
///
/// # Arguments
///
/// * `scylla` - The session to ask
/// * `keyspace` - The keyspace
pub async fn existing_views(scylla: &Session, keyspace: &str) -> Result<BTreeSet<String>, Error> {
    // the schema lists every view of the keyspace
    let rows = scylla
        .query_unpaged(
            "SELECT view_name FROM system_schema.views WHERE keyspace_name = ?",
            (keyspace,),
        )
        .await?
        .into_rows_result()
        .map_err(|error| Error::new(format!("failed to read the views of {keyspace}: {error}")))?;
    let names = rows
        .rows::<(String,)>()
        .map_err(|error| Error::new(format!("failed to read the views of {keyspace}: {error}")))?
        .filter_map(Result::ok)
        .map(|(name,)| name)
        .collect();
    Ok(names)
}

/// Asks Scylla for a view's `CREATE MATERIALIZED VIEW` statement
///
/// # Arguments
///
/// * `scylla` - The session to ask
/// * `keyspace` - The keyspace
/// * `view` - The view
pub async fn describe(scylla: &Session, keyspace: &str, view: &str) -> Result<String, Error> {
    // server-side DESCRIBE returns the statement in its create_statement column
    let rows = scylla
        .query_unpaged(format!("DESCRIBE MATERIALIZED VIEW {keyspace}.{view}"), &[])
        .await?
        .into_rows_result()
        .map_err(|error| Error::new(format!("failed to describe {keyspace}.{view}: {error}")))?;
    let (_, _, _, statement) = rows
        .first_row::<(String, String, String, String)>()
        .map_err(|error| Error::new(format!("failed to describe {keyspace}.{view}: {error}")))?;
    Ok(statement)
}

/// Returns the view a `CREATE MATERIALIZED VIEW` statement creates, as `keyspace.view` (or just
/// `view` for a statement that names no keyspace)
///
/// # Arguments
///
/// * `statement` - The statement
fn created_view(statement: &str) -> Option<String> {
    // the name follows the opening words, as `keyspace.view`
    let rest = statement
        .trim_start()
        .strip_prefix("CREATE MATERIALIZED VIEW ")?;
    let name = rest
        .trim_start()
        .trim_start_matches("IF NOT EXISTS ")
        .split_whitespace()
        .next()?;
    let unquoted = name
        .split('.')
        .map(|part| part.trim_matches('"'))
        .collect::<Vec<_>>()
        .join(".");
    Some(unquoted)
}

/// Splits a `views.cql` file into its statements, by the view each creates as `keyspace.view`
///
/// # Arguments
///
/// * `text` - The file's contents
#[must_use]
pub fn parse(text: &str) -> BTreeMap<String, String> {
    // statements end with a semicolon at the end of a line
    let mut statements = BTreeMap::new();
    let mut current = String::new();
    for line in text.lines() {
        current.push_str(line);
        current.push('\n');
        if line.trim_end().ends_with(';') {
            let statement = std::mem::take(&mut current);
            if let Some(view) = created_view(&statement) {
                statements.insert(view, statement.trim().to_owned());
            }
        }
    }
    statements
}

/// Returns every earlier `views.cql` a re-run may take a missing view from, newest first: those of
/// the backup's earlier restores, and those in and beside a report directory named for this run
///
/// # Arguments
///
/// * `backup` - The backup being restored
/// * `report_dir` - This run's report directory
async fn earlier_files(backup: &Path, report_dir: &Path) -> Vec<(std::time::SystemTime, PathBuf)> {
    // the directories earlier runs wrote their reports into
    let mut dirs = Vec::new();
    if let Ok(mut listing) = tokio::fs::read_dir(backup.join("reports")).await {
        while let Ok(Some(entry)) = listing.next_entry().await {
            if entry.file_name().to_string_lossy().starts_with("restore-") {
                dirs.push(entry.path());
            }
        }
    }
    dirs.push(report_dir.to_path_buf());
    if let Some(parent) = report_dir.parent()
        && let Ok(mut listing) = tokio::fs::read_dir(parent).await
    {
        while let Ok(Some(entry)) = listing.next_entry().await {
            if entry.path() != report_dir && entry.path().is_dir() {
                dirs.push(entry.path());
            }
        }
    }
    // each one's views.cql, newest first
    let mut files = Vec::new();
    for dir in dirs {
        let file = dir.join(VIEWS_FILE);
        if let Ok(metadata) = tokio::fs::metadata(&file).await
            && let Ok(modified) = metadata.modified()
        {
            files.push((modified, file));
        }
    }
    files.sort();
    files.dedup();
    files.reverse();
    files
}

impl ViewStatements {
    /// Captures the statement of every view a restore of some tables drops: each view that exists
    /// from Scylla, and each that is missing from the newest earlier `views.cql` holding its
    /// statement for this keyspace (a restore of the same backup into another namespace leaves
    /// statements for that namespace's keyspace, which are never taken)
    ///
    /// # Arguments
    ///
    /// * `scylla` - The session to the target
    /// * `keyspace` - The target's keyspace
    /// * `tables` - The tables being restored
    /// * `backup` - The backup being restored
    /// * `report_dir` - This run's report directory
    ///
    /// # Returns
    ///
    /// The statements, and the views no statement could be found for
    pub async fn capture(
        scylla: &Session,
        keyspace: &str,
        tables: &[&str],
        backup: &Path,
        report_dir: &Path,
    ) -> Result<(ViewStatements, Vec<String>), Error> {
        // every registered view of the restored tables
        let existing = existing_views(scylla, keyspace).await?;
        let mut statements = BTreeMap::new();
        let mut missing = Vec::new();
        for view in registered_views(tables) {
            if existing.contains(view) {
                statements.insert(view.to_owned(), describe(scylla, keyspace, view).await?);
            } else {
                missing.push(view.to_owned());
            }
        }
        // a view an earlier restore dropped comes from the newest file that holds it
        let mut unfound = Vec::new();
        if !missing.is_empty() {
            let files = earlier_files(backup, report_dir).await;
            for view in missing {
                let mut found = false;
                for (_, file) in &files {
                    let Ok(text) = tokio::fs::read_to_string(file).await else {
                        continue;
                    };
                    if let Some(statement) = parse(&text).remove(&format!("{keyspace}.{view}")) {
                        warn!(
                            "{keyspace}.{view} is missing, as an interrupted restore leaves it; \
                             its statement comes from {}",
                            file.display()
                        );
                        statements.insert(view.clone(), statement);
                        found = true;
                        break;
                    }
                }
                if !found {
                    unfound.push(format!("{keyspace}.{view}"));
                }
            }
        }
        Ok((ViewStatements { statements }, unfound))
    }

    /// Writes the statements to `views.cql` in a report directory, so an admin can replay them
    /// after an interrupted restore
    ///
    /// # Arguments
    ///
    /// * `report_dir` - The report directory
    /// * `sinks` - The file-system seam to write through
    pub async fn save(&self, report_dir: &Path, sinks: &dyn SinkFactory) -> Result<(), Error> {
        // one statement per view, each ending its own paragraph
        let mut text = String::new();
        for statement in self.statements.values() {
            text.push_str(statement.trim_end());
            if !statement.trim_end().ends_with(';') {
                text.push(';');
            }
            text.push_str("\n\n");
        }
        write_atomic(report_dir, VIEWS_FILE, text.as_bytes(), sinks).await?;
        Ok(())
    }

    /// Recreates every view that is missing, from the statements held in memory, and waits for
    /// those recreated to finish building
    ///
    /// # Arguments
    ///
    /// * `scylla` - The session to the target
    /// * `keyspace` - The target's keyspace
    pub async fn recreate(&self, scylla: &Session, keyspace: &str) -> Result<Recreated, Error> {
        // a view whose table wasn't restored was never dropped
        let existing = existing_views(scylla, keyspace).await?;
        self.recreate_missing(scylla, keyspace, &existing).await
    }

    /// Recreates every view not among some existing views, each on its own so one that fails
    /// leaves the rest recreated, and waits for those recreated to finish building
    ///
    /// A view counts as recreated once the target keyspace holds it, so one another client (an
    /// API pod's schema setup) created first counts too, as long as its definition matches the
    /// saved statement's; one with another definition, and a statement that ran without creating
    /// the view in the target keyspace, count as failed.
    ///
    /// # Arguments
    ///
    /// * `scylla` - The session to the target
    /// * `keyspace` - The target's keyspace
    /// * `existing` - The views the keyspace held before
    pub async fn recreate_missing(
        &self,
        scylla: &Session,
        keyspace: &str,
        existing: &BTreeSet<String>,
    ) -> Result<Recreated, Error> {
        let mut done = Recreated::default();
        for (view, statement) in &self.statements {
            if existing.contains(view) {
                continue;
            }
            // run the statement, then check the target keyspace for the view either way
            let ran = scylla.query_unpaged(statement.as_str(), &[]).await;
            let holds = existing_views(scylla, keyspace).await?.contains(view);
            match (ran, holds) {
                (Ok(_), true) => {
                    info!("Recreated view {keyspace}.{view}");
                    done.views.push(view.clone());
                }
                (Err(error), true) => {
                    // another client created it first, which counts only with the same definition
                    let found = describe(scylla, keyspace, view).await?;
                    if same_definition(&found, statement) {
                        info!(
                            "View {keyspace}.{view} was already back, created by another client: \
                             {error}"
                        );
                        done.views.push(view.clone());
                    } else {
                        let error = format!(
                            "another client created {keyspace}.{view} first, with another \
                             definition than the saved statement's: {found}"
                        );
                        warn!("Couldn't recreate view {keyspace}.{view}: {error}");
                        done.failed.push((view.clone(), error));
                    }
                }
                (Ok(_), false) => {
                    let error = format!("its statement ran but didn't create {keyspace}.{view}");
                    warn!("Couldn't recreate view {keyspace}.{view}: {error}");
                    done.failed.push((view.clone(), error));
                }
                (Err(error), false) => {
                    warn!("Couldn't recreate view {keyspace}.{view}: {error}");
                    done.failed.push((view.clone(), error.to_string()));
                }
            }
        }
        // wait for each recreated view to finish building, so reads through it see every row
        wait_for_builds(scylla, keyspace, &done.views).await?;
        Ok(done)
    }
}

/// Returns whether two `CREATE MATERIALIZED VIEW` statements define the same view, ignoring
/// whitespace and a trailing semicolon
///
/// # Arguments
///
/// * `found` - The statement Scylla gives for the existing view
/// * `saved` - The saved statement
fn same_definition(found: &str, saved: &str) -> bool {
    // compare the words, so line breaks and indentation don't matter
    let words = |statement: &str| {
        statement
            .trim()
            .trim_end_matches(';')
            .split_whitespace()
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>()
    };
    words(found) == words(saved)
}

/// What recreating a restore's views did
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Recreated {
    /// The views back in the target keyspace, built
    pub views: Vec<String>,
    /// Each view that couldn't be recreated, with why
    pub failed: Vec<(String, String)>,
}

/// Drops the views the registry lists for a table, before its rows are written
///
/// # Arguments
///
/// * `scylla` - The session to the target
/// * `keyspace` - The target's keyspace
/// * `table` - The table about to be restored
pub async fn drop_views_of(scylla: &Session, keyspace: &str, table: &str) -> Result<(), Error> {
    // exactly the registered views, each logged so the drop is visible
    for view in Registry::views_of(table) {
        super::utils::drop_materialized_view(keyspace, view, scylla).await?;
        info!("Dropped view {keyspace}.{view}");
    }
    Ok(())
}

/// Waits until every node reports each view built, with no time limit
///
/// Each view's per-node status rows are read once per poll; a view is logged when it finishes, and
/// every so often a line says which views are built and which are still building.
///
/// # Arguments
///
/// * `scylla` - The session to the target
/// * `keyspace` - The target's keyspace
/// * `views` - The views to wait for
async fn wait_for_builds(scylla: &Session, keyspace: &str, views: &[String]) -> Result<(), Error> {
    // a large table's view can take long to build, so the wait is logged rather than bounded
    let started = Instant::now();
    let mut logged = Instant::now();
    let mut states: BTreeMap<&str, BuildStatus> = BTreeMap::new();
    let mut pending: Vec<&String> = views.iter().collect();
    while !pending.is_empty() {
        // read each unfinished view's status once, logging the views that just finished
        let mut still = Vec::new();
        for view in pending {
            let build = view_status(scylla, keyspace, view).await?;
            states.insert(view, build);
            if build.built() {
                info!(
                    "View {keyspace}.{view} built after {}s",
                    started.elapsed().as_secs()
                );
            } else {
                still.push(view);
            }
        }
        pending = still;
        if pending.is_empty() {
            break;
        }
        // say which views are built and which are building every so often, then check again
        if logged.elapsed() >= BUILD_LOG {
            let all: Vec<(&str, BuildStatus)> = states.iter().map(|(v, s)| (*v, *s)).collect();
            info!("{}", progress_line(&all, started.elapsed()));
            logged = Instant::now();
        }
        tokio::time::sleep(BUILD_POLL).await;
    }
    // every view is built
    if !states.is_empty() {
        let all: Vec<(&str, BuildStatus)> = states.iter().map(|(v, s)| (*v, *s)).collect();
        info!("{}", built_line(&all, started.elapsed()));
    }
    Ok(())
}

/// Returns whether every node reports a view built
///
/// Each node writes its own status row, so the table is read at consistency one and a view counts
/// as built once every node the session knows has reported success; the table's replication would
/// need several nodes for the default consistency.
///
/// # Arguments
///
/// * `scylla` - The session to the target
/// * `keyspace` - The view's keyspace
/// * `view` - The view
pub async fn view_built(scylla: &Session, keyspace: &str, view: &str) -> Result<bool, Error> {
    // built once every known node reports success
    Ok(view_status(scylla, keyspace, view).await?.built())
}

/// How far one view's build has come, from the status rows the nodes wrote
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildStatus {
    /// The nodes whose row says the build succeeded
    pub succeeded: usize,
    /// The nodes that have written a status row for the view
    pub reported: usize,
    /// The nodes the session knows
    pub nodes: usize,
}

impl BuildStatus {
    /// Counts the nodes' status rows of one view
    ///
    /// # Arguments
    ///
    /// * `statuses` - Each reporting node's status
    /// * `nodes` - The nodes the session knows
    #[must_use]
    pub fn from_statuses(statuses: &[String], nodes: usize) -> Self {
        // each row is one node's report; those saying SUCCESS are done
        let succeeded = statuses
            .iter()
            .filter(|status| *status == "SUCCESS")
            .count();
        BuildStatus {
            succeeded,
            reported: statuses.len(),
            nodes,
        }
    }

    /// Returns whether every known node reports the build succeeded
    #[must_use]
    pub fn built(&self) -> bool {
        // every known node has reported, and every report is a success
        self.reported >= self.nodes && self.succeeded == self.reported
    }

    /// Describes the state of the build in words
    #[must_use]
    pub fn describe(&self) -> String {
        // built, building with the nodes' progress, or not yet reported on
        if self.built() {
            "built".to_owned()
        } else if self.reported == 0 {
            "building (no node has reported yet)".to_owned()
        } else {
            format!(
                "building ({} of {} nodes done)",
                self.succeeded, self.reported
            )
        }
    }
}

/// Builds the line that says every view is built
///
/// # Arguments
///
/// * `views` - Each view with its latest status
/// * `elapsed` - The time since the wait began
#[must_use]
pub fn built_line(views: &[(&str, BuildStatus)], elapsed: Duration) -> String {
    // every view with its state
    let states: Vec<String> = views
        .iter()
        .map(|(view, status)| format!("{view} {}", status.describe()))
        .collect();
    format!(
        "All {} views built after {}s: {}",
        views.len(),
        elapsed.as_secs(),
        states.join(", ")
    )
}

/// Reads each node's build status of a view
///
/// # Arguments
///
/// * `scylla` - The session to the target
/// * `keyspace` - The view's keyspace
/// * `view` - The view
pub async fn view_status(
    scylla: &Session,
    keyspace: &str,
    view: &str,
) -> Result<BuildStatus, Error> {
    // each node writes its own row, so one replica answers
    let mut query = Statement::new(
        "SELECT status FROM system_distributed.view_build_status WHERE keyspace_name = ? AND view_name = ?",
    );
    query.set_consistency(Consistency::One);
    let rows = scylla
        .query_unpaged(query, (keyspace, view))
        .await?
        .into_rows_result()
        .map_err(|error| {
            Error::new(format!(
                "failed to read the build of {keyspace}.{view}: {error}"
            ))
        })?;
    // every reporting node's status
    let statuses: Vec<String> = rows
        .rows::<(String,)>()
        .map_err(|error| {
            Error::new(format!(
                "failed to read the build of {keyspace}.{view}: {error}"
            ))
        })?
        .filter_map(Result::ok)
        .map(|(status,)| status)
        .collect();
    // the nodes the session knows are the ones expected to report
    let nodes = scylla.get_cluster_state().get_nodes_info().len().max(1);
    Ok(BuildStatus::from_statuses(&statuses, nodes))
}

/// Builds the line that says which views are built and which are still building
///
/// # Arguments
///
/// * `views` - Each view with its latest status
/// * `elapsed` - The time since the wait began
#[must_use]
pub fn progress_line(views: &[(&str, BuildStatus)], elapsed: Duration) -> String {
    // how many of the views are still building, then each view's state
    let building = views.iter().filter(|(_, status)| !status.built()).count();
    let states: Vec<String> = views
        .iter()
        .map(|(view, status)| format!("{view} {}", status.describe()))
        .collect();
    format!(
        "{building} of {} views still building after {}s: {}",
        views.len(),
        elapsed.as_secs(),
        states.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::{BuildStatus, parse, progress_line, same_definition};
    use std::time::Duration;

    /// Statements that differ only in whitespace and a final semicolon define the same view
    #[test]
    fn statements_differing_in_layout_define_the_same_view() {
        // DESCRIBE's layout, and the same statement on one line with a semicolon
        let described = "CREATE MATERIALIZED VIEW ns.samples AS\n    SELECT *\n    FROM ns.samples_list\n    PRIMARY KEY (sha256, group)";
        let saved = "CREATE MATERIALIZED VIEW ns.samples AS SELECT * FROM ns.samples_list PRIMARY KEY (sha256, group);";
        assert!(same_definition(described, saved));
        // another key or base table is another definition
        assert!(!same_definition(
            described,
            "CREATE MATERIALIZED VIEW ns.samples AS SELECT * FROM ns.samples_list PRIMARY KEY (group, sha256);"
        ));
        assert!(!same_definition(
            described,
            "CREATE MATERIALIZED VIEW ns.samples AS SELECT * FROM ns.tags PRIMARY KEY (sha256, group);"
        ));
    }

    /// A saved file splits into one statement per view, by the view each creates
    #[test]
    fn saved_statements_split_by_view() {
        // two statements as DESCRIBE gives them, one spanning several lines
        let text = "CREATE MATERIALIZED VIEW ns.samples AS\n    SELECT *\n    FROM ns.samples_list\n    WHERE sha256 IS NOT NULL\n    PRIMARY KEY (sha256, group);\n\nCREATE MATERIALIZED VIEW ns.repos AS SELECT * FROM ns.repos_list PRIMARY KEY (url);\n";
        // each statement is kept whole under its view
        let statements = parse(text);
        assert_eq!(statements.len(), 2, "{statements:?}");
        assert!(statements["ns.samples"].starts_with("CREATE MATERIALIZED VIEW ns.samples AS"));
        assert!(statements["ns.samples"].ends_with("PRIMARY KEY (sha256, group);"));
        assert!(statements["ns.repos"].contains("ns.repos_list"));
        // the same view of another keyspace is its own statement, and quotes are dropped
        let other = parse(
            "CREATE MATERIALIZED VIEW \"other\".\"samples\" AS SELECT * FROM other.samples_list PRIMARY KEY (sha256);\n",
        );
        assert!(other.contains_key("other.samples"), "{other:?}");
    }

    /// Statuses for some nodes
    ///
    /// # Arguments
    ///
    /// * `success` - How many nodes report success
    /// * `started` - How many nodes report the build as started
    fn rows(success: usize, started: usize) -> Vec<String> {
        // each success row, then each started row
        let mut rows = vec!["SUCCESS".to_owned(); success];
        rows.extend(vec!["STARTED".to_owned(); started]);
        rows
    }

    /// A view is built only once every known node reports success
    #[test]
    fn built_needs_every_node_to_report_success() {
        // all three nodes succeeded
        assert!(BuildStatus::from_statuses(&rows(3, 0), 3).built());
        // one still started, one yet to report, and none at all
        assert!(!BuildStatus::from_statuses(&rows(2, 1), 3).built());
        assert!(!BuildStatus::from_statuses(&rows(2, 0), 3).built());
        assert!(!BuildStatus::from_statuses(&[], 3).built());
        // the counts come from the rows
        let status = BuildStatus::from_statuses(&rows(1, 2), 3);
        assert_eq!((status.succeeded, status.reported, status.nodes), (1, 3, 3));
    }

    /// The progress line names each view with its state and the elapsed time
    #[test]
    fn progress_line_names_each_view_and_its_state() {
        // one view built, one half done, one nobody has reported on
        let views = [
            ("samples", BuildStatus::from_statuses(&rows(3, 0), 3)),
            ("tags", BuildStatus::from_statuses(&rows(1, 1), 3)),
            ("repos", BuildStatus::from_statuses(&[], 3)),
        ];
        let line = progress_line(&views, Duration::from_secs(42));
        assert!(line.contains("42s"), "{line}");
        assert!(line.contains("samples built"), "{line}");
        assert!(line.contains("tags building (1 of 2 nodes done)"), "{line}");
        assert!(
            line.contains("repos building (no node has reported yet)"),
            "{line}"
        );
        assert!(line.contains("2 of 3"), "{line}");
    }
}
