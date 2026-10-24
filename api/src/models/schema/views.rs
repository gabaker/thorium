//! Recreating declared materialized views: `schema apply` never alters or drops an existing view,
//! so a view that is present but out of step with its base table is repaired by dropping it and
//! creating it again from the registry. This path drops only a declared view; any name the registry
//! doesn't declare as a view is refused before anything is changed. The one other object thoradm
//! ever drops is a registered legacy object, on `thoradm schema drop-legacy`.

use scylla::client::session::Session;
use serde::{Deserialize, Serialize};

use super::apply::{ApplyError, read_texts, wait_for_views};
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
/// its build finishes, which scans the whole base table.
///
/// # Arguments
///
/// * `session` - The Scylla session to recreate with
/// * `conf` - The Thorium config, for the keyspace
/// * `views` - The declared views to recreate
/// * `dry_run` - Return the statements without executing them
/// * `progress` - Receives the warning for each view and the progress of the builds
pub async fn recreate_views(
    session: &Session,
    conf: &Conf,
    views: &[ViewDef],
    dry_run: bool,
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
    // drop and create each view in turn, saying first what it costs
    for view in views {
        progress(&recreate_warning(view));
        // drop the view, which tolerates one already gone
        let [drop_view, create_view] = recreate_statements(keyspace, view);
        session
            .query_unpaged(drop_view.as_str(), ())
            .await
            .map_err(|err| ApplyError::query(&drop_view, err))?;
        // a create failing after the drop leaves the view missing, which the error says
        session
            .query_unpaged(create_view.as_str(), ())
            .await
            .map_err(|err| ViewError::NotCreated {
                view: view.name.to_owned(),
                error: ApplyError::query(&create_view, err),
            })?;
    }
    // wait until every recreated view has built
    let names = views.iter().map(|view| view.name).collect::<Vec<_>>();
    wait_for_views(session, &stored, &names, progress).await?;
    Ok(report)
}
