//! Converging the live schema to a committed revision snapshot
//!
//! A snapshot holds each object's canonical form, so the statements are rendered from that form
//! instead of from the declarations this build was compiled with. Like every apply, it only
//! creates and adds, never drops: an older snapshot re-adds the columns and objects a later
//! revision's contract step dropped, and leaves everything newer in place.

use serde_json::Value;
use std::collections::BTreeSet;

use super::{
    Change, ConfigAccept, LiveSchema, config_record, execute, live_schema, plan, record_snapshot,
    redact_trigger_key, scylla_objects, wait_for_changed_views,
};
use crate::Conf;
use crate::models::schema::scylla::ThoriumKeyspace;
use crate::models::schema::scylla::{SchemaMigrationIssuesTable, SchemaMigrationsTable};
use crate::models::schema::{
    ApplyError, ApplyReport, ConfigChange, Keyspace, ObjectDef, RevisionSnapshot, SchemaObject,
    ScyllaObject,
};
use crate::models::upgrades::SchemaBackend;
use scylla::client::session::Session;

#[cfg(test)]
mod tests;

/// How one apply to a snapshot runs
#[derive(Debug, Clone, Default)]
pub struct SnapshotRun<'a> {
    /// Return the statements without executing them
    pub dry_run: bool,
    /// What to record instead of refusing
    pub accept: ConfigAccept,
    /// The trigger key this run applies for, recorded with the fingerprint without the files
    /// password's hash
    pub trigger_key: &'a str,
    /// The names of snapshot tables and views this run leaves uncreated, such as the objects no
    /// release reads any more
    pub skip: BTreeSet<String>,
}

/// One Scylla table or view of a snapshot, read from its canonical form
#[derive(Debug)]
struct Rendered<'a> {
    /// The object's name
    name: &'a str,
    /// Whether the object is a view
    view: bool,
    /// The object's canonical form
    canonical: &'a Value,
}

/// Get a string field of a canonical form
///
/// # Arguments
///
/// * `canonical` - The canonical form
/// * `field` - The field to read
fn text<'a>(canonical: &'a Value, field: &str) -> &'a str {
    // a missing or non-text field reads as empty, which the statement then shows
    canonical
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or_default()
}

/// Get a canonical form's columns as `(name, type)` pairs, in the order the form lists them
///
/// # Arguments
///
/// * `canonical` - The canonical form of a table or view
fn columns(canonical: &Value) -> Vec<(&str, &str)> {
    // each column is an object with a name and a canonical type
    canonical
        .get("columns")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|column| (text(column, "name"), text(column, "type")))
        .collect()
}

/// Get a canonical form's partition key columns, in key order
///
/// # Arguments
///
/// * `canonical` - The canonical form of a table or view
fn partition_key(canonical: &Value) -> Vec<&str> {
    // the partition key lists column names
    canonical
        .get("partition_key")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}

/// Get a canonical form's clustering columns with their orders, in key order
///
/// # Arguments
///
/// * `canonical` - The canonical form of a table or view
fn clustering_key(canonical: &Value) -> Vec<(&str, bool)> {
    // each clustering column names its order, `desc` or `asc`
    canonical
        .get("clustering_key")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|column| (text(column, "name"), text(column, "order") == "desc"))
        .collect()
}

/// Render the `PRIMARY KEY (...)` clause and the clustering order of a canonical form
///
/// # Arguments
///
/// * `canonical` - The canonical form of a table or view
fn primary_key(canonical: &Value) -> (String, Option<String>) {
    // the partition key is always parenthesized, followed by the clustering columns
    let partition = partition_key(canonical).join(", ");
    let clustering = clustering_key(canonical);
    let mut key = format!("PRIMARY KEY (({partition})");
    for (name, _) in &clustering {
        key.push_str(", ");
        key.push_str(name);
    }
    key.push(')');
    // every clustering column's order is listed, ascending ones included
    let order = (!clustering.is_empty()).then(|| {
        let orders = clustering
            .iter()
            .map(|(name, desc)| format!("{name} {}", if *desc { "DESC" } else { "ASC" }))
            .collect::<Vec<_>>()
            .join(", ");
        format!("CLUSTERING ORDER BY ({orders})")
    });
    (key, order)
}

/// Render the `CREATE TABLE IF NOT EXISTS` statement of a table's canonical form
///
/// # Arguments
///
/// * `keyspace` - The keyspace to create the table in
/// * `canonical` - The table's canonical form
/// * `ttl` - The table's default TTL in seconds, read from its TTL source's setting
#[must_use]
pub fn table_cql(keyspace: &str, canonical: &Value, ttl: Option<u64>) -> String {
    // the static columns are listed by name beside the columns
    let statics = canonical
        .get("static")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<BTreeSet<_>>();
    // declare every column with its type, then the primary key
    let declared = columns(canonical)
        .into_iter()
        .map(|(name, ty)| {
            let marker = if statics.contains(name) {
                " STATIC"
            } else {
                ""
            };
            format!("{name} {ty}{marker}")
        })
        .collect::<Vec<_>>()
        .join(", ");
    let (key, order) = primary_key(canonical);
    // add the clustering order and default TTL as table options
    let options = order
        .into_iter()
        .chain(ttl.map(|ttl| format!("default_time_to_live = {ttl}")))
        .collect::<Vec<_>>();
    let with = if options.is_empty() {
        String::new()
    } else {
        format!(" WITH {}", options.join(" AND "))
    };
    format!(
        "CREATE TABLE IF NOT EXISTS {keyspace}.{} ({declared}, {key}){with}",
        text(canonical, "name")
    )
}

/// Render the `CREATE MATERIALIZED VIEW IF NOT EXISTS` statement of a view's canonical form
///
/// # Arguments
///
/// * `keyspace` - The keyspace to create the view in
/// * `canonical` - The view's canonical form
#[must_use]
pub fn view_cql(keyspace: &str, canonical: &Value) -> String {
    // select the view's columns explicitly
    let selected = columns(canonical)
        .into_iter()
        .map(|(name, _)| name)
        .collect::<Vec<_>>()
        .join(", ");
    // the filter replaces the `IS NOT NULL` of the one key column it restricts
    let filter = canonical.get("filter").and_then(Value::as_str);
    let restricted = filter.and_then(|filter| {
        filter
            .split(|c: char| !(c.is_alphanumeric() || c == '_'))
            .find(|part| !part.is_empty())
    });
    // every other key column must be set
    let mut conditions = partition_key(canonical)
        .into_iter()
        .chain(clustering_key(canonical).into_iter().map(|(name, _)| name))
        .filter(|name| Some(*name) != restricted)
        .map(|name| format!("{name} IS NOT NULL"))
        .collect::<Vec<_>>();
    conditions.extend(filter.map(str::to_owned));
    let (key, order) = primary_key(canonical);
    let with = order.map_or_else(String::new, |order| format!(" WITH {order}"));
    format!(
        "CREATE MATERIALIZED VIEW IF NOT EXISTS {keyspace}.{} AS SELECT {selected} FROM \
         {keyspace}.{} WHERE {} {key}{with}",
        text(canonical, "name"),
        text(canonical, "base"),
        conditions.join(" AND ")
    )
}

/// Get the default TTL a table's configured setting gives, when this build declares the table
/// with a TTL source
///
/// # Arguments
///
/// * `conf` - The Thorium config
/// * `table` - The table's name
fn configured_ttl(conf: &Conf, table: &str) -> Option<u64> {
    // the snapshot holds no TTL, so the declaration of the same name names the setting
    ThoriumKeyspace::OBJECTS.iter().find_map(|def| match def {
        ObjectDef::Table(def) if def.name == table => def
            .ttl
            .and_then(|source| conf.table_ttl(source.config_path)),
        _ => None,
    })
}

/// Read the Scylla tables and views of a snapshot that a run creates or adds to: every table
/// first, then every view, leaving out the migration-state tables (which are always converged
/// from this build's declaration) and the names a run skips
///
/// # Arguments
///
/// * `snapshot` - The snapshot to converge to
/// * `skip` - The names to leave out
fn scylla_of<'a>(snapshot: &'a RevisionSnapshot, skip: &BTreeSet<String>) -> Vec<Rendered<'a>> {
    // the state tables are converged from the declaration, never from a snapshot
    let state = [
        <SchemaMigrationsTable as ScyllaObject>::NAME,
        <SchemaMigrationIssuesTable as ScyllaObject>::NAME,
    ];
    let mut objects = snapshot
        .objects
        .iter()
        .filter(|object| object.backend == SchemaBackend::Scylla)
        .filter(|object| !state.contains(&object.name.as_str()) && !skip.contains(&object.name))
        .map(|object| Rendered {
            name: &object.name,
            view: text(&object.canonical, "kind") == "view",
            canonical: &object.canonical,
        })
        .collect::<Vec<_>>();
    // a view reads its base table, so every table is created before any view
    objects.sort_by_key(|object| object.view);
    objects
}

/// Plan the statements that bring the live schema to a snapshot's tables and views: create each
/// missing object, add each missing table column, and set a default TTL that differs from the
/// config; nothing is ever dropped
///
/// # Arguments
///
/// * `conf` - The Thorium config, for the keyspace and table TTLs
/// * `live` - What the keyspace holds
/// * `objects` - The snapshot's tables and views, tables first
fn plan_snapshot(conf: &Conf, live: &LiveSchema, objects: &[Rendered<'_>]) -> Vec<Change> {
    let keyspace = &conf.thorium.namespace;
    let mut changes = Vec::new();
    for object in objects {
        let name = object.name;
        // a view that exists is never altered
        if object.view {
            if !live.has(name) {
                changes.push(Change::Create {
                    name: name.to_owned(),
                    view: true,
                    cql: view_cql(keyspace, object.canonical),
                });
            }
            continue;
        }
        match live.tables.get(name) {
            // a missing table is created with its configured default TTL
            None if !live.has(name) => changes.push(Change::Create {
                name: name.to_owned(),
                view: false,
                cql: table_cql(keyspace, object.canonical, configured_ttl(conf, name)),
            }),
            // a view holding the table's name is drift for the probe, never replaced here
            None => (),
            // an existing table gains each snapshot column it lacks
            Some(live_columns) => {
                for (column, ty) in columns(object.canonical) {
                    if !live_columns.contains(column) {
                        changes.push(Change::Add {
                            table: name.to_owned(),
                            column: column.to_owned(),
                            cql: format!("ALTER TABLE {keyspace}.{name} ADD {column} {ty}"),
                        });
                    }
                }
                // a configured default TTL that differs from the live one is altered
                if let (Some(to), Some(from)) = (configured_ttl(conf, name), live.ttls.get(name))
                    && i64::try_from(to).ok() != Some(*from)
                {
                    changes.push(Change::Ttl {
                        table: name.to_owned(),
                        from: *from,
                        to,
                        cql: format!(
                            "ALTER TABLE {keyspace}.{name} WITH default_time_to_live = {to}"
                        ),
                    });
                }
            }
        }
    }
    changes
}

/// Converge the live schema to a revision snapshot, and record the snapshot's fingerprints
///
/// The migration-state tables are converged from this build's declaration first, since the record
/// lives in them. The data-shaping settings are checked and recorded exactly as `apply` does, so
/// a changed setting with its data present is refused. The fingerprints recorded are the
/// snapshot's, not this build's.
///
/// # Arguments
///
/// * `session` - The Scylla session to apply with
/// * `conf` - The Thorium config
/// * `snapshot` - The snapshot to converge to
/// * `run` - Whether to execute, what to accept, the trigger key, and what to leave uncreated
/// * `progress` - Receives a line for each progress report while views build
pub async fn apply_snapshot(
    session: &Session,
    conf: &Conf,
    snapshot: &RevisionSnapshot,
    run: &SnapshotRun<'_>,
    progress: &(dyn Fn(&str) + Sync),
) -> Result<ApplyReport, ApplyError> {
    // report and record the key without the files password's hash
    let trigger_key = redact_trigger_key(run.trigger_key);
    // plan against what the keyspace holds now
    let keyspace = conf.thorium.namespace.to_lowercase();
    let live = live_schema(session, &keyspace).await?;
    // check the data-shaping settings against the record before changing anything
    let declared = scylla_objects(&SchemaObject::declared());
    let has_table = |table: &str| live.tables.contains_key(table);
    let config = config_record::plan(
        session,
        conf,
        &declared,
        &has_table,
        &run.accept,
        run.dry_run,
        progress,
    )
    .await?;
    // the record tables come from the declaration, then the snapshot's own objects
    let record_tables = declared
        .iter()
        .filter(|def| {
            [
                <SchemaMigrationsTable as ScyllaObject>::NAME,
                <SchemaMigrationIssuesTable as ScyllaObject>::NAME,
            ]
            .contains(&def.name())
        })
        .copied()
        .collect::<Vec<_>>();
    let objects = scylla_of(snapshot, &run.skip);
    let mut changes = plan(conf, &live, &record_tables);
    // a table a snapshot creates, or a column it adds, can't exist in the record tables' plan
    changes.extend(plan_snapshot(conf, &live, &objects));
    let mut report = ApplyReport {
        statements: changes
            .iter()
            .map(|change| change.cql().to_owned())
            .collect(),
        created: Vec::new(),
        added: Vec::new(),
        config_changes: Vec::new(),
        fingerprint: snapshot.deployment.clone(),
        trigger_key: trigger_key.clone(),
        views_built: Vec::new(),
        warnings: config.warnings.clone(),
        dry_run: run.dry_run,
    };
    // sort each change into what the report lists
    for change in &changes {
        match change {
            Change::Keyspace(_) => (),
            Change::Create { name, .. } => report.created.push(name.clone()),
            Change::Add { table, column, .. } => report.added.push(format!("{table}.{column}")),
            Change::Ttl {
                table, from, to, ..
            } => report.config_changes.push(ConfigChange {
                table: table.clone(),
                setting: "default_time_to_live".to_owned(),
                from: from.to_string(),
                to: to.to_string(),
            }),
        }
    }
    // a dry run stops before changing anything
    if run.dry_run {
        return Ok(report);
    }
    // issue each change in order
    for change in &changes {
        execute(session, &keyspace, change).await?;
    }
    // wait for the views this run created, and for any a stopped earlier run left building
    let views = objects
        .iter()
        .filter(|object| object.view)
        .map(|object| object.name)
        .collect::<Vec<_>>();
    report.views_built =
        wait_for_changed_views(session, &keyspace, &views, &changes, progress).await?;
    // record the snapshot's fingerprints and layouts with the data-shaping settings
    record_snapshot(session, conf, snapshot, &trigger_key, &config.rows).await?;
    Ok(report)
}
