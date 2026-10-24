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
use serde_json::Value;
use std::time::{Duration, Instant};

use super::apply::{ApplyError, read_texts, wait_for_drop, wait_for_views};
use super::probe::{ProbeError, SchemaRows, view_differences};
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

/// A declared view whose live definition can't be read, which `schema apply` leaves as it is
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct UnreadableView {
    /// The view's name
    pub(super) name: String,
    /// Why its live definition can't be read
    pub(super) reason: String,
}

/// How the live definitions of a list of declared views compare with this build's
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct ViewComparison {
    /// The views whose live definitions differ, which `schema apply` drops and creates again
    pub(super) stale: Vec<StaleView>,
    /// The views whose live definitions can't be read, which `schema apply` reports and leaves
    pub(super) unreadable: Vec<UnreadableView>,
}

impl ViewComparison {
    /// Sort one declared view into the comparison by its live definition: listed as stale when
    /// it differs from the declared form, as unreadable when it can't be read, and not at all
    /// when it matches
    ///
    /// A definition that can't be read is never taken as stale: a gap in reading what a newer
    /// Scylla stores is likelier than an out-of-date view, and recreating it on every apply would
    /// rebuild it from its whole base table each time.
    ///
    /// # Arguments
    ///
    /// * `name` - The view's name
    /// * `want` - The view's declared canonical form
    /// * `live` - The view's live canonical form, or why it can't be read
    pub(super) fn add(&mut self, name: &str, want: &Value, live: Result<Value, ProbeError>) {
        match live {
            // a live definition that reads is compared with the declared one
            Ok(live) => {
                let differences = view_differences(want, &live);
                if !differences.is_empty() {
                    self.stale.push(StaleView {
                        name: name.to_owned(),
                        differences,
                    });
                }
            }
            // one that can't be read is set aside with the reason
            Err(err) => self.unreadable.push(UnreadableView {
                name: name.to_owned(),
                reason: err.to_string(),
            }),
        }
    }
}

/// Compare the live definitions of a list of views with what this build declares
///
/// Only declared views that exist as views are compared: a declared view the keyspace lacks is
/// created instead, and a view this build doesn't declare is never listed. A live view whose
/// definition can't be read is listed apart, so the apply leaves it alone.
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
) -> Result<ViewComparison, ApplyError> {
    // read the keyspace's schema rows once for every view
    let rows = SchemaRows::read(session, keyspace)
        .await
        .map_err(|err| ApplyError::query("read the live view definitions", err))?;
    // compare each declared view that exists with its declared form
    let declared = declared_views();
    let mut compared = ViewComparison::default();
    for view in views.iter().filter(|view| declared.contains(view)) {
        let Some(live) = rows.view_form(view.name) else {
            continue;
        };
        let want = SchemaDefinition::Scylla(ObjectDef::View(*view)).canonical();
        compared.add(view.name, &want, live);
    }
    Ok(compared)
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
    use super::{ProbeError, UnreadableView, ViewComparison, declared_views};
    use crate::models::schema::{ObjectDef, SchemaDefinition};

    /// A live view that matches is left out, one that differs is stale, and one whose definition
    /// can't be read is set aside as unreadable instead of being taken as stale
    #[test]
    fn an_unreadable_live_view_is_set_aside_not_recreated() {
        // the declared samples view's form, and a live form on another base table
        let samples = declared_views()
            .into_iter()
            .find(|view| view.name == "samples")
            .expect("samples is declared");
        let want = SchemaDefinition::Scylla(ObjectDef::View(samples)).canonical();
        let mut old = want.clone();
        old["base"] = serde_json::json!("samples_old");
        let mut compared = ViewComparison::default();
        // a matching live view is neither stale nor unreadable
        compared.add("samples", &want, Ok(want.clone()));
        assert_eq!(compared, ViewComparison::default());
        // a live view on another base table is stale, with how it differs
        compared.add("samples", &want, Ok(old));
        assert_eq!(compared.stale.len(), 1);
        assert_eq!(
            compared.stale[0].differences,
            ["base: samples_old -> samples_list"]
        );
        // a live view the probe can't read is reported, never listed as stale
        let unreadable = ProbeError::Unreadable {
            object: "tags_by_item".to_owned(),
            reason: "unterminated quote in the filter".to_owned(),
        };
        compared.add("tags_by_item", &want, Err(unreadable));
        assert_eq!(compared.stale.len(), 1);
        assert_eq!(compared.unreadable.len(), 1);
        let UnreadableView { name, reason } = &compared.unreadable[0];
        assert_eq!(name, "tags_by_item");
        assert!(reason.contains("unterminated quote"), "{reason}");
    }
}
