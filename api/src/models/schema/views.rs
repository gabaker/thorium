//! Recreating declared materialized views. Scylla can't alter a view's definition, so a declared
//! view whose live definition differs from this build's (an older release's base table, key, or
//! filter) is dropped and created again by `schema apply` itself, and a view whose definition is
//! right but which is out of step with its base table is repaired the same way on request
//! (`--recreate-view`). Views hold no data of their own: Scylla rebuilds every row from the base
//! table. These paths drop only a declared view; any name the registry doesn't declare as a view
//! is refused before anything is changed. The one other object thoradm ever drops is a
//! registered legacy object, on `thoradm schema drop-legacy`.

use scylla::client::session::Session;
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

use super::apply::{ApplyError, read_texts, wait_for_drop, wait_for_views};
use super::probe::{SchemaRows, view_differences};
use super::{ObjectDef, SchemaDefinition, SchemaObject, ViewDef};
use crate::Conf;

/// Why recreating or checking views stopped
#[derive(Debug)]
pub enum ViewError {
    /// The name isn't a view this build declares, so nothing of that name is dropped
    Undeclared {
        /// The name given
        name: String,
    },
    /// The view's base table doesn't exist, so the view can't be created again until
    /// `schema apply` creates the table
    MissingBase {
        /// The view
        view: String,
        /// Its missing base table
        base: String,
    },
    /// The token ranges asked for can't be checked, such as a sample of none
    BadRanges {
        /// Why they can't
        reason: String,
    },
    /// A view was dropped but creating it again failed, so it is missing until `schema apply`
    /// (or the same recreate) creates it
    NotCreated {
        /// The view
        view: String,
        /// Why the create failed
        error: ApplyError,
    },
    /// A statement or a read failed
    Apply(ApplyError),
}

impl std::fmt::Display for ViewError {
    /// Describe why recreating or checking views stopped
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // say which view is wrong and why
        match self {
            ViewError::Undeclared { name } => write!(
                f,
                "`{name}` isn't a materialized view this build declares, so `--recreate-view` \
                 never drops it (`thoradm schema drop-legacy` drops registered legacy objects); \
                 declared views: {}",
                declared_views()
                    .iter()
                    .map(|view| view.name)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            ViewError::MissingBase { view, base } => write!(
                f,
                "the base table `{base}` of view `{view}` doesn't exist; run `thoradm schema \
                 apply` to create it"
            ),
            ViewError::NotCreated { view, error } => write!(
                f,
                "view `{view}` is now missing: it was dropped, then {error}; run `thoradm schema \
                 apply` (or `thoradm schema apply --recreate-view {view}` again) to create it"
            ),
            ViewError::BadRanges { reason } => write!(f, "{reason}"),
            ViewError::Apply(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for ViewError {}

impl From<ApplyError> for ViewError {
    /// Wrap a failed statement or read
    ///
    /// # Arguments
    ///
    /// * `err` - The applier's error
    fn from(err: ApplyError) -> Self {
        ViewError::Apply(err)
    }
}

/// What recreating views did, or would do on a dry run
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecreateReport {
    /// Every statement issued (or, on a dry run, that would be), in order
    pub statements: Vec<String>,
    /// The views dropped and created again, each listed only once its build finished (on a dry
    /// run, the views that would be)
    pub recreated: Vec<String>,
    /// Whether nothing was executed
    pub dry_run: bool,
}

/// A declared view whose live definition differs from the one this build declares, which
/// `schema apply` drops and creates again
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StaleView {
    /// The view's name
    pub name: String,
    /// How the live definition differs, each as `part: live -> declared`
    pub differences: Vec<String>,
}

/// What comparing the declared views with their live definitions found
#[derive(Debug, Default)]
pub(super) struct StaleViews {
    /// The views whose live definitions differ, which `schema apply` drops and creates again
    pub(super) stale: Vec<StaleView>,
    /// A warning for each declared view whose live definition can't be read, which is left as it
    /// is: what it holds can't be judged, so it is never dropped
    pub(super) unreadable: Vec<String>,
}

/// Find the views of a list whose live definitions differ from what this build declares
///
/// Only declared views that exist as views are compared: a declared view the keyspace lacks is
/// created instead, and a view this build doesn't declare is never listed. A live view whose
/// definition can't be read is reported instead of listed, so it is never dropped.
///
/// # Arguments
///
/// * `session` - The Scylla session to read with
/// * `keyspace` - The keyspace, as Scylla stores its name
/// * `views` - The views to compare
pub(super) async fn stale_views(
    session: &Session,
    keyspace: &str,
    views: &[ViewDef],
) -> Result<StaleViews, ApplyError> {
    // read the keyspace's schema rows once for every view, then compare them
    let rows = SchemaRows::read(session, keyspace)
        .await
        .map_err(|err| ApplyError::query("read the live view definitions", err))?;
    Ok(judge_views(&rows, views))
}

/// Compare the views of a list with their live definitions in a keyspace's schema rows
///
/// # Arguments
///
/// * `rows` - The keyspace's `system_schema` rows
/// * `views` - The views to compare
fn judge_views(rows: &SchemaRows, views: &[ViewDef]) -> StaleViews {
    // compare each declared view that exists with its declared form
    let declared = declared_views();
    let mut found = StaleViews::default();
    for view in views.iter().filter(|view| declared.contains(view)) {
        let Some(live) = rows.view_form(view.name) else {
            continue;
        };
        // a definition that can't be read is reported and left in place
        let live = match live {
            Ok(live) => live,
            Err(err) => {
                found.unreadable.push(format!(
                    "view `{}`'s live definition can't be read ({err}), so it was left as it is; \
                     `thoradm schema apply --recreate-view {}` replaces it if it is out of date",
                    view.name, view.name
                ));
                continue;
            }
        };
        let want = SchemaDefinition::Scylla(ObjectDef::View(*view)).canonical();
        let differences = view_differences(&want, &live);
        if !differences.is_empty() {
            found.stale.push(StaleView {
                name: view.name.to_owned(),
                differences,
            });
        }
    }
    found
}

/// Get every materialized view this build declares, in declaration order
#[must_use]
pub fn declared_views() -> Vec<ViewDef> {
    // keep the views among the declared Scylla objects
    SchemaObject::declared()
        .iter()
        .filter_map(|object| match object.definition {
            SchemaDefinition::Scylla(ObjectDef::View(view)) => Some(view),
            _ => None,
        })
        .collect()
}

/// Look up each named view among the declared ones, refusing the whole list when any name isn't a
/// declared view; a name given twice is recreated once
///
/// # Arguments
///
/// * `names` - The views to look up
pub fn resolve_views<S: AsRef<str>>(names: &[S]) -> Result<Vec<ViewDef>, ViewError> {
    // find each name among the declared views, in the order given
    let declared = declared_views();
    let mut views: Vec<ViewDef> = Vec::with_capacity(names.len());
    for name in names {
        let name = name.as_ref();
        let Some(view) = declared.iter().find(|view| view.name == name) else {
            return Err(ViewError::Undeclared {
                name: name.to_owned(),
            });
        };
        if !views.contains(view) {
            views.push(*view);
        }
    }
    Ok(views)
}

/// Render the two statements that recreate a view: drop it, then create it from the registry
///
/// # Arguments
///
/// * `keyspace` - The keyspace the view is in
/// * `view` - The view
#[must_use]
pub fn recreate_statements(keyspace: &str, view: &ViewDef) -> [String; 2] {
    // the drop tolerates a view that is already gone, and the create is the registry's own
    [
        format!("DROP MATERIALIZED VIEW IF EXISTS {keyspace}.{}", view.name),
        view.create_cql(keyspace),
    ]
}

/// Describe what recreating a view costs, said before it starts
///
/// # Arguments
///
/// * `view` - The view to recreate
#[must_use]
pub fn recreate_warning(view: &ViewDef) -> String {
    // name the view, its base table, and what reads see meanwhile
    format!(
        "recreating view `{}`: routes reading it fail while it is dropped, then see missing rows \
         until its build finishes, which scans the whole `{}` table",
        view.name, view.base
    )
}

/// Drop each view and create it again from the registry, then wait until every one has built
///
/// Each view must be declared and its base table must exist: otherwise the call refuses before
/// anything is dropped. Routes reading a view fail while it is dropped, then see missing rows until
/// its build finishes, which scans the whole base table. Each view is created only once Scylla
/// has cleared the dropped view's build state, so the wait can't take it for the new build.
///
/// # Arguments
///
/// * `session` - The Scylla session to recreate with
/// * `conf` - The Thorium config, for the keyspace
/// * `views` - The declared views to recreate
/// * `dry_run` - Return the statements without executing them
/// * `view_wait` - The longest to wait, in all, for Scylla to clear each dropped view's build
///   state and then for the builds, before failing with the view left dropped or with the views
///   and hosts still building; `None` waits for as long as they take
/// * `progress` - Receives the warning for each view and the progress of the builds
pub async fn recreate_views(
    session: &Session,
    conf: &Conf,
    views: &[ViewDef],
    dry_run: bool,
    view_wait: Option<Duration>,
    progress: &(dyn Fn(&str) + Sync),
) -> Result<RecreateReport, ViewError> {
    // refuse any definition the registry doesn't declare, however it was built
    let declared = declared_views();
    if let Some(view) = views.iter().find(|view| !declared.contains(view)) {
        return Err(ViewError::Undeclared {
            name: view.name.to_owned(),
        });
    }
    // list what the run issues
    let keyspace = &conf.thorium.namespace;
    let report = RecreateReport {
        statements: views
            .iter()
            .flat_map(|view| recreate_statements(keyspace, view))
            .collect(),
        recreated: views.iter().map(|view| view.name.to_owned()).collect(),
        dry_run,
    };
    // a dry run stops before reading or changing anything
    if dry_run {
        return Ok(report);
    }
    // refuse a view whose name a table holds before any view is dropped, since a drop of it fails
    let stored = keyspace.to_lowercase();
    let mut held = Vec::new();
    for view in views {
        let tables = read_texts::<1>(
            session,
            "SELECT table_name FROM system_schema.tables WHERE keyspace_name = ? AND table_name = ?",
            &[&stored, view.name],
        )
        .await?;
        if !tables.is_empty() {
            held.push(view.name.to_owned());
        }
    }
    if !held.is_empty() {
        return Err(ApplyError::ViewHeldByTable { views: held }.into());
    }
    // refuse a view whose base table is missing before any view is dropped
    for view in views {
        let base = read_texts::<1>(
            session,
            "SELECT table_name FROM system_schema.tables WHERE keyspace_name = ? AND table_name = ?",
            &[&stored, view.base],
        )
        .await?;
        if base.is_empty() {
            return Err(ViewError::MissingBase {
                view: view.name.to_owned(),
                base: view.base.to_owned(),
            });
        }
    }
    // drop and create each view in turn, saying first what it costs, all within one limit
    let started = Instant::now();
    for view in views {
        progress(&recreate_warning(view));
        // drop the view, which tolerates one already gone
        let [drop_view, create_view] = recreate_statements(keyspace, view);
        session
            .query_unpaged(drop_view.as_str(), ())
            .await
            .map_err(|err| ApplyError::query(&drop_view, err))?;
        // create it only once Scylla has cleared the dropped view's build state, which the wait
        // below would otherwise take as the new view's
        wait_for_drop(session, &stored, view.name, started, view_wait)
            .await
            .map_err(|error| ViewError::NotCreated {
                view: view.name.to_owned(),
                error,
            })?;
        // a create failing after the drop leaves the view missing, which the error says
        session
            .query_unpaged(create_view.as_str(), ())
            .await
            .map_err(|err| ViewError::NotCreated {
                view: view.name.to_owned(),
                error: ApplyError::query(&create_view, err),
            })?;
    }
    // wait until every recreated view has built, within what's left of the limit
    let names = views.iter().map(|view| view.name).collect::<Vec<_>>();
    let left = view_wait.map(|limit| limit.saturating_sub(started.elapsed()));
    wait_for_views(session, &stored, &names, left, progress).await?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::{SchemaRows, judge_views, resolve_views};
    use crate::models::schema::probe::{ColumnRow, ViewRow};

    /// One column row of a view
    ///
    /// # Arguments
    ///
    /// * `view` - The view the column belongs to
    /// * `name` - The column's name
    /// * `cql_type` - The column's CQL type as Scylla renders it
    fn column(view: &str, name: &str, cql_type: &str) -> ColumnRow {
        // a regular column outside the key
        ColumnRow {
            table_name: view.to_owned(),
            column_name: name.to_owned(),
            kind: "regular".to_owned(),
            position: -1,
            clustering_order: "NONE".to_owned(),
            cql_type: cql_type.to_owned(),
        }
    }

    /// One view row selecting from a base table
    ///
    /// # Arguments
    ///
    /// * `view` - The view's name
    /// * `base` - Its base table
    fn view_row(view: &str, base: &str) -> ViewRow {
        // a view with no filter beyond its keys
        ViewRow {
            view_name: view.to_owned(),
            base_table_name: base.to_owned(),
            where_clause: String::new(),
            include_all_columns: false,
        }
    }

    /// A declared view on another base is listed as stale with how it differs, a declared view
    /// whose definition can't be read is reported and never listed for a drop, and a declared view
    /// the keyspace lacks is neither
    #[test]
    fn unreadable_views_are_reported_and_never_dropped() {
        // committed_repo_data on the old base, samples with a column type that can't be parsed
        let rows = SchemaRows {
            columns: vec![
                column("committed_repo_data", "hash", "text"),
                column("samples", "sha256", "map<text"),
            ],
            views: vec![
                view_row("committed_repo_data", "commits_list"),
                view_row("samples", "samples_list"),
            ],
            ..SchemaRows::default()
        };
        let views = resolve_views(&["committed_repo_data", "samples", "tags_by_item"])
            .expect("declared views");
        let found = judge_views(&rows, &views);
        // only the readable stale view is dropped and created again
        let names = found
            .stale
            .iter()
            .map(|stale| stale.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["committed_repo_data"]);
        assert!(
            found.stale[0]
                .differences
                .iter()
                .any(|line| line == "base: commits_list -> commitish_list"),
            "{:?}",
            found.stale[0].differences
        );
        // the unreadable one is named in a warning and left alone
        assert_eq!(found.unreadable.len(), 1, "{:?}", found.unreadable);
        assert!(
            found.unreadable[0].contains("view `samples`"),
            "{:?}",
            found.unreadable
        );
    }
}
