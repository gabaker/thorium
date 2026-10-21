//! The coverage registry against a live namespace: nothing Thorium stores goes unclassified

mod common;

use common::fixture;
use common::harness::{BUCKET_ROLES, Harness, config_bucket_roles};
use std::collections::BTreeSet;
use thoradm::backup::Backup;
use thoradm::backup::coverage::{COVERAGE, Coverage, Registry, Store};
use thoradm::backup::tables::{
    Association, Comment, Commitish, CommitishList, Entity, NetworkPolicy, Node, Notification,
    Output, OutputStream, RepoData, RepoList, S3Id, SamplesList, Tag,
};

/// The file to edit when a resource has no registry entry
const REGISTRY_FILE: &str = "thoradm/src/backup/coverage.rs";

/// The source trees of the services that build Thorium's Redis keys, from the workspace root
const KEY_BUILDERS: &[&str] = &[
    "api/src",
    "event-handler/src",
    "scaler/src",
    "search-streamer/src",
    "agent/src",
    "reactor/src",
];

/// Registry Redis entries no fixture can write, with the reason
const UNREACHABLE: &[(&str, &str)] = &[
    (
        "system_keys",
        "no service writes it any more; only its key builder remains",
    ),
    (
        "stage_logs:*",
        "the API only deletes it; stage logs are stored in the logs table",
    ),
];

/// Key templates that no code writes, so they need no registry entry, with the reason
const UNWRITTEN_TEMPLATES: &[(&str, &str)] = &[(
    "{ns}:entities:vendors:{id}:devices",
    "VendorKeys builds it, but nothing calls VendorKeys",
)];

/// Returns every Redis key template the services build, with the file that builds it
fn key_templates() -> Vec<(String, String)> {
    // walk each source tree for Rust files
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut pending: Vec<std::path::PathBuf> =
        KEY_BUILDERS.iter().map(|dir| root.join(dir)).collect();
    let mut templates = Vec::new();
    while let Some(path) = pending.pop() {
        if path.is_dir() {
            pending.extend(
                std::fs::read_dir(&path)
                    .unwrap()
                    .map(|entry| entry.unwrap().path()),
            );
            continue;
        }
        if path.extension().is_none_or(|ext| ext != "rs") {
            continue;
        }
        // collect the file's templates
        let text = std::fs::read_to_string(&path).unwrap();
        for template in templates_in(&text) {
            templates.push((template, path.display().to_string()));
        }
    }
    templates
}

/// Returns the key templates in one source file's text
///
/// A template is a string literal whose first segment is a single placeholder, the namespace
/// however it is named or positioned, and whose second segment starts with a lowercase
/// letter: `{ns}:users`, `{namespace}:jobs:{id}`, `{}:logs:{}`, or `{prefix}:census:{year}`.
/// Literals like `{}:{}` or `{group}:{name} not found` aren't keys.
///
/// # Arguments
///
/// * `text` - The source text
fn templates_in(text: &str) -> Vec<String> {
    // check each literal that opens with a placeholder
    let mut templates = Vec::new();
    for (at, _) in text.match_indices("\"{") {
        let rest = &text[at + 1..];
        let Some(end) = rest.find('"') else {
            continue;
        };
        let literal = &rest[..end];
        // one placeholder, a colon, then a family name
        let Some(close) = literal.find('}') else {
            continue;
        };
        let mut after = literal[close + 1..].chars();
        if after.next() == Some(':') && after.next().is_some_and(|c| c.is_ascii_lowercase()) {
            templates.push(literal.to_owned());
        }
    }
    templates
}

/// The keys each backed-up Redis family with a wildcard holds, rendered without the namespace
///
/// The Redis backup reads only the keys it knows, so a key nested deeper under one of these
/// families is a new family the backup doesn't carry, even though the wildcard matches it.
const BACKED_UP_SHAPES: &[(&str, &[&str])] = &[
    ("user_data:*", &["user_data:x"]),
    ("user_groups:*", &["user_groups:x"]),
    ("oauth:alias_to_user:*", &["oauth:alias_to_user:x"]),
    ("groups_data:*", &["groups_data:x"]),
    ("groups_direct:*", &["groups_direct:x:x"]),
    ("groups_metagroups:*", &["groups_metagroups:x:x"]),
    ("groups_combined:*", &["groups_combined:x:x"]),
    ("images:*", &["images:x"]),
    ("image_data:*", &["image_data:x:x"]),
    ("image_used_by:*", &["image_used_by:x:x"]),
    ("pipelines:*", &["pipelines:x"]),
    ("pipeline_data:*", &["pipeline_data:x:x"]),
    ("scoped_tokens:*", &["scoped_tokens:x"]),
    ("netpols:*", &["netpols:x"]),
];

/// Returns whether a key template's keys belong to exactly one registry entry, and to one of
/// the known shapes when that entry is a backed-up family with a wildcard
///
/// # Arguments
///
/// * `template` - The template
fn is_classified(template: &str) -> bool {
    // the rendered key must match one entry
    let key = render(template);
    let entries: Vec<_> = Registry::matches(Store::Redis, &key).collect();
    let [entry] = entries.as_slice() else {
        return false;
    };
    // a backed-up wildcard family holds only the shapes the backup reads
    match entry.coverage {
        Coverage::BackedUp(_) if entry.name.ends_with('*') => BACKED_UP_SHAPES
            .iter()
            .any(|(family, shapes)| *family == entry.name && shapes.contains(&key.as_str())),
        _ => true,
    }
}

/// Renders a key template as a key without its namespace, filling every placeholder
///
/// # Arguments
///
/// * `template` - The template, starting with its namespace placeholder
fn render(template: &str) -> String {
    // drop the namespace, then replace each `{...}` with one segment's worth of text
    let key = template.split_once(':').map_or(template, |(_, key)| key);
    let mut rendered = String::new();
    let mut inside = false;
    for c in key.chars() {
        match (c, inside) {
            ('{', _) => inside = true,
            ('}', true) => {
                inside = false;
                rendered.push('x');
            }
            (_, true) => (),
            (c, false) => rendered.push(c),
        }
    }
    rendered
}

/// Lists the names of every table or view in a keyspace
///
/// # Arguments
///
/// * `harness` - The harness naming the keyspace
/// * `views` - Whether to list views instead of tables
async fn schema_names(harness: &Harness, views: bool) -> Vec<String> {
    // read the keyspace's schema rows
    let scylla = harness.scylla().await;
    let (table, column) = if views {
        ("views", "view_name")
    } else {
        ("tables", "table_name")
    };
    let query = format!("SELECT {column} FROM system_schema.{table} WHERE keyspace_name = ?");
    scylla
        .query_unpaged(query, (harness.conf.thorium.namespace.as_str(),))
        .await
        .unwrap()
        .into_rows_result()
        .unwrap()
        .rows::<(String,)>()
        .unwrap()
        .map(|row| row.unwrap().0)
        .collect()
}

/// Lists every Redis key of the harness's namespace, without the namespace prefix
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
async fn redis_keys(harness: &Harness) -> Vec<String> {
    // scan the namespace's keys
    let pool = common::harness::redis_pool(&harness.conf).await.unwrap();
    let mut conn = pool.get().await.unwrap();
    let prefix = format!("{}:", harness.conf.thorium.namespace);
    let keys: Vec<String> = redis::cmd("KEYS")
        .arg(format!("{prefix}*"))
        .query_async(&mut *conn)
        .await
        .unwrap();
    // drop the namespace so the keys line up with registry patterns
    keys.into_iter()
        .map(|key| key.strip_prefix(&prefix).unwrap().to_owned())
        .collect()
}

/// Returns every stored resource that doesn't match exactly one registry entry
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
async fn unclassified(harness: &Harness) -> Vec<String> {
    // gather every table, view, Redis key, and bucket role that exists
    let mut found: Vec<(Store, String)> = Vec::new();
    found.extend(
        schema_names(harness, false)
            .await
            .into_iter()
            .map(|name| (Store::ScyllaTable, name)),
    );
    found.extend(
        schema_names(harness, true)
            .await
            .into_iter()
            .map(|name| (Store::ScyllaView, name)),
    );
    found.extend(
        redis_keys(harness)
            .await
            .into_iter()
            .map(|name| (Store::Redis, name)),
    );
    found.extend(
        config_bucket_roles(&harness.conf)
            .into_iter()
            .map(|role| (Store::S3, role)),
    );
    // report anything with no entry or more than one
    found
        .into_iter()
        .filter_map(|(store, name)| {
            let matches = Registry::matches(store, &name).count();
            (matches != 1).then(|| {
                format!(
                    "{} {name:?} matches {matches} entries; classify it in {REGISTRY_FILE}",
                    store.label()
                )
            })
        })
        .collect()
}

/// With the fixture loaded, every table, view, Redis key, and bucket role matches exactly one
/// registry entry, and a stray table or key is reported by name
#[tokio::test]
async fn every_stored_resource_is_classified() {
    // seed one of every resource
    let harness = Harness::get().await;
    fixture::broad(harness).await;
    // the API created every table and view the registry knows
    let tables: BTreeSet<String> = schema_names(harness, false).await.into_iter().collect();
    let views: BTreeSet<String> = schema_names(harness, true).await.into_iter().collect();
    for entry in COVERAGE {
        match entry.store {
            Store::ScyllaTable => assert!(tables.contains(entry.name), "no table {}", entry.name),
            Store::ScyllaView => assert!(views.contains(entry.name), "no view {}", entry.name),
            Store::Redis | Store::S3 => (),
        }
    }
    // nothing the namespace holds is unclassified
    assert_eq!(unclassified(harness).await, Vec::<String>::new());
    // every registry Redis entry holds a key, so the check above saw each family
    let keys = redis_keys(harness).await;
    let unreached: Vec<&str> = COVERAGE
        .iter()
        .filter(|entry| entry.store == Store::Redis)
        .filter(|entry| !UNREACHABLE.iter().any(|(name, _)| *name == entry.name))
        .filter(|entry| {
            !keys.iter().any(|key| {
                Registry::matches(Store::Redis, key).any(|found| found.name == entry.name)
            })
        })
        .map(|entry| entry.name)
        .collect();
    assert!(
        unreached.is_empty(),
        "the fixture writes no key of {unreached:?}"
    );
    // a table and a Redis key nobody classified are both reported by name
    let ns = harness.conf.thorium.namespace.clone();
    let scylla = harness.scylla().await;
    scylla
        .query_unpaged(
            format!("CREATE TABLE {ns}.stray_table (id int PRIMARY KEY)"),
            &[],
        )
        .await
        .unwrap();
    let pool = common::harness::redis_pool(&harness.conf).await.unwrap();
    let mut conn = pool.get().await.unwrap();
    let _: () = redis::cmd("SET")
        .arg(format!("{ns}:stray_family:io"))
        .arg("x")
        .query_async(&mut *conn)
        .await
        .unwrap();
    let reported = unclassified(harness).await;
    assert_eq!(reported.len(), 2, "{reported:?}");
    assert!(
        reported
            .iter()
            .any(|line| line.contains("\"stray_table\"") && line.contains(REGISTRY_FILE))
    );
    assert!(
        reported
            .iter()
            .any(|line| line.contains("\"stray_family:io\""))
    );
    // remove the strays again
    scylla
        .query_unpaged(format!("DROP TABLE {ns}.stray_table"), &[])
        .await
        .unwrap();
    let _: () = redis::cmd("DEL")
        .arg(format!("{ns}:stray_family:io"))
        .query_async(&mut *conn)
        .await
        .unwrap();
}

/// The bucket roles the harness and the registry use are exactly the ones the config has
#[test]
fn bucket_roles_follow_the_config() {
    // a new bucket in the config fails here until it is listed and classified
    let conf = thorium::Conf::new("../api/tests/thorium-testing.yml").unwrap();
    let mut listed: Vec<String> = BUCKET_ROLES.iter().map(|role| (*role).to_owned()).collect();
    listed.sort();
    assert_eq!(listed, config_bucket_roles(&conf));
    for role in config_bucket_roles(&conf) {
        assert_eq!(
            Registry::matches(Store::S3, &role).count(),
            1,
            "bucket role {role} isn't classified in {REGISTRY_FILE}"
        );
    }
}

/// Every Redis key a service builds belongs to exactly one registry entry, so a new family
/// fails here even before any fixture writes it
#[test]
fn every_redis_key_the_services_build_is_classified() {
    // render each key template and classify it
    let templates = key_templates();
    assert!(
        templates.len() > 50,
        "found only {} key templates",
        templates.len()
    );
    let unclassified: Vec<String> = templates
        .iter()
        .filter(|(template, _)| {
            !UNWRITTEN_TEMPLATES
                .iter()
                .any(|(unwritten, _)| unwritten == template)
        })
        .filter(|(template, _)| !is_classified(template))
        .map(|(template, file)| format!("{template} ({file}); classify it in {REGISTRY_FILE}"))
        .collect();
    assert!(unclassified.is_empty(), "{unclassified:#?}");
    // an unwritten template that's gone no longer needs its exception
    for (unwritten, _) in UNWRITTEN_TEMPLATES {
        assert!(
            templates.iter().any(|(template, _)| template == unwritten),
            "{unwritten} is no longer built; remove it from UNWRITTEN_TEMPLATES"
        );
    }
    // a listed shape no template builds any more is stale
    for (family, shapes) in BACKED_UP_SHAPES {
        for shape in *shapes {
            assert!(
                templates
                    .iter()
                    .any(|(template, _)| render(template) == *shape),
                "no template builds {shape}; remove it from {family}'s BACKED_UP_SHAPES"
            );
        }
    }
}

/// Key builders the scan must catch: a new family, one nested under a backed-up family, and
/// ones written with a positional or another named namespace placeholder
#[test]
fn new_key_families_are_unclassified_in_every_builder_form() {
    // each snippet builds one key of a family the registry doesn't have
    let snippets = [
        "let key = format!(\"{ns}:new_family:{user}\");",
        "let key = format!(\"{ns}:user_data:{user}:secrets\");",
        "let key = format!(\"{}:new_family:{}\", ns, user);",
        "let key = format!(\"{prefix}:new_family:{user}\");",
    ];
    let missed: Vec<&str> = snippets
        .iter()
        .filter(|snippet| {
            let templates = templates_in(snippet);
            templates.len() != 1 || is_classified(&templates[0])
        })
        .copied()
        .collect();
    assert!(missed.is_empty(), "{missed:#?}");
    // an existing family's key is still classified, and text that isn't a key isn't a template
    assert_eq!(templates_in("format!(\"{ns}:user_data:{user}\")").len(), 1);
    assert!(is_classified("{ns}:user_data:{user}"));
    for text in [
        "format!(\"{}:{}\", host, port)",
        "format!(\"{group}:{name} not in group level of image cache\")",
        "format!(\"{ERROR_PREFIX}: '{url}' does not contain a username!\")",
        "format!(\"{}: {}\", a, b)",
    ] {
        assert!(templates_in(text).is_empty(), "{text}");
    }
}

/// Reads one table's columns from the live schema
///
/// # Arguments
///
/// * `harness` - The harness naming the keyspace
/// * `table` - The table to read
async fn live_columns(harness: &Harness, table: &str) -> BTreeSet<String> {
    // read the table's column names
    let scylla = harness.scylla().await;
    scylla
        .query_unpaged(
            "SELECT column_name FROM system_schema.columns WHERE keyspace_name = ? AND table_name = ?",
            (harness.conf.thorium.namespace.as_str(), table),
        )
        .await
        .unwrap()
        .into_rows_result()
        .unwrap()
        .rows::<(String,)>()
        .unwrap()
        .map(|row| row.unwrap().0)
        .collect()
}

/// Returns the columns a table's backup statement reads
///
/// # Arguments
///
/// * `harness` - The harness naming the keyspace
async fn read_columns<T: Backup>(harness: &Harness) -> (String, BTreeSet<String>) {
    // prepare the statement the backup worker runs and read its result columns
    let scylla = harness.scylla().await;
    let prepared = T::prepared_statement(&scylla, &harness.conf.thorium.namespace)
        .await
        .unwrap();
    let specs = prepared.get_current_result_set_col_specs();
    let columns = specs
        .get()
        .iter()
        .map(|spec| spec.name().to_owned())
        .collect();
    (T::name().to_owned(), columns)
}

/// For every backed-up table, the live columns are exactly its archived and excluded columns, the
/// archive reads exactly the archived ones, and the column lookup agrees
#[tokio::test]
async fn backed_up_columns_match_the_schema_and_the_archives() {
    // check every backed-up table's split against the live schema
    let harness = Harness::get().await;
    for entry in Registry::backed_up(Store::ScyllaTable) {
        let columns = entry.columns.unwrap();
        let archived: BTreeSet<String> = columns.archived.iter().map(|c| (*c).to_owned()).collect();
        let excluded: BTreeSet<String> = columns
            .excluded
            .iter()
            .map(|(c, _)| (*c).to_owned())
            .collect();
        assert!(
            archived.is_disjoint(&excluded),
            "{} lists a column twice",
            entry.name
        );
        let listed: BTreeSet<String> = archived.union(&excluded).cloned().collect();
        assert_eq!(
            live_columns(harness, entry.name).await,
            listed,
            "{}",
            entry.name
        );
        // the lookup answers true for archived columns and false for excluded ones
        for column in columns.archived {
            assert!(
                Registry::column_backed_up(entry.name, column),
                "{}.{column}",
                entry.name
            );
        }
        for (column, reason) in columns.excluded {
            assert_ne!(*reason, "", "{}.{column} has no reason", entry.name);
            assert!(
                !Registry::column_backed_up(entry.name, column),
                "{}.{column}",
                entry.name
            );
        }
    }
    // an excluded table backs up none of its columns
    for entry in Registry::excluded().filter(|entry| entry.store == Store::ScyllaTable) {
        for column in live_columns(harness, entry.name).await {
            assert!(
                !Registry::column_backed_up(entry.name, &column),
                "{}.{column}",
                entry.name
            );
        }
    }
    // every archive struct reads exactly the columns its entry lists as archived
    let statements = vec![
        read_columns::<SamplesList>(harness).await,
        read_columns::<S3Id>(harness).await,
        read_columns::<Comment>(harness).await,
        read_columns::<Output>(harness).await,
        read_columns::<OutputStream>(harness).await,
        read_columns::<Tag>(harness).await,
        read_columns::<RepoData>(harness).await,
        read_columns::<RepoList>(harness).await,
        read_columns::<Commitish>(harness).await,
        read_columns::<CommitishList>(harness).await,
        read_columns::<NetworkPolicy>(harness).await,
        read_columns::<Entity>(harness).await,
        read_columns::<Association>(harness).await,
        read_columns::<Notification>(harness).await,
        read_columns::<Node>(harness).await,
    ];
    assert_eq!(
        statements.len(),
        Registry::backed_up(Store::ScyllaTable).count()
    );
    for (table, mut read) in statements {
        // a row's remaining TTL, which notifications read beside their columns, isn't a column
        read.retain(|column| !column.starts_with("ttl("));
        let entry = Registry::classify(Store::ScyllaTable, &table)
            .unwrap_or_else(|| panic!("{table} has an archive but no entry"));
        assert!(matches!(entry.coverage, Coverage::BackedUp(_)), "{table}");
        let archived: BTreeSet<String> = entry
            .columns
            .unwrap()
            .archived
            .iter()
            .map(|c| (*c).to_owned())
            .collect();
        assert_eq!(read, archived, "{table}");
    }
}
