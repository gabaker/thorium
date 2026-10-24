//! Tests that the declared keyspace follows the schema rules and renders the same schema the
//! hand-written setup statements created

use std::collections::BTreeMap;

use super::scylla::{NetworkPoliciesDefaultView, ThoriumKeyspace};
use super::{
    ClusterOrder, ColumnDef, ColumnRole, CqlDesc, CqlNative, Keyspace, ObjectDef, RegistryError,
    ScyllaObject, TableDef, ViewDef, validate,
};

/// The TTL the golden DDL renders for tables with a default TTL
const GOLDEN_TTL: u64 = 2_592_000;

/// One column as Scylla's `system_schema.columns` describes it
#[derive(Debug, Clone, PartialEq, Eq)]
struct SchemaColumn {
    /// The column's type, lowercase with no whitespace
    typ: String,
    /// `partition_key`, `clustering`, `static`, or `regular`
    kind: &'static str,
    /// The column's position in its part of the key, or -1
    position: i32,
    /// `asc` or `desc` for a clustering column, otherwise `none`
    order: &'static str,
}

/// One object as Scylla's schema tables describe it
#[derive(Debug, Clone, PartialEq, Eq)]
struct SchemaObject {
    /// `table` or `view`
    kind: &'static str,
    /// A view's base table
    base: Option<String>,
    /// Every column by name
    columns: BTreeMap<String, SchemaColumn>,
    /// A view's conditions other than `IS NOT NULL`
    filter: Option<String>,
}

/// Collapse every run of whitespace to one space and lowercase a statement
///
/// # Arguments
///
/// * `cql` - The statement
fn normalize(cql: &str) -> String {
    cql.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Split a list on the commas outside of parentheses and angle brackets
///
/// # Arguments
///
/// * `list` - The list to split
fn split_top(list: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0_i32;
    let mut current = String::new();
    for c in list.chars() {
        match c {
            '(' | '<' => depth += 1,
            ')' | '>' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(current.trim().to_owned());
                current.clear();
                continue;
            }
            _ => (),
        }
        current.push(c);
    }
    if !current.trim().is_empty() {
        parts.push(current.trim().to_owned());
    }
    parts
}

/// Get the text inside the parentheses that open at a position
///
/// # Arguments
///
/// * `text` - The text holding the parentheses
/// * `open` - The position of the opening parenthesis
fn inside_parens(text: &str, open: usize) -> &str {
    let mut depth = 0;
    for (index, c) in text[open..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return &text[open + 1..open + index];
                }
            }
            _ => (),
        }
    }
    panic!("unbalanced parentheses in {text}");
}

/// Parse a `PRIMARY KEY (...)` clause's contents into partition and clustering columns
///
/// # Arguments
///
/// * `key` - The text inside the clause's parentheses
fn parse_key(key: &str) -> (Vec<String>, Vec<String>) {
    let mut parts = split_top(key).into_iter();
    let first = parts.next().expect("a primary key has a partition key");
    // a parenthesized first part is a composite partition key
    let partition = match first
        .strip_prefix('(')
        .and_then(|rest| rest.strip_suffix(')'))
    {
        Some(inner) => split_top(inner),
        None => vec![first],
    };
    (partition, parts.collect())
}

/// Parse a `CLUSTERING ORDER BY (...)` clause in some table options
///
/// # Arguments
///
/// * `options` - The text after `WITH`
fn parse_order(options: &str) -> BTreeMap<String, &'static str> {
    let Some(start) = options.find("clustering order by") else {
        return BTreeMap::new();
    };
    let open = start
        + options[start..]
            .find('(')
            .expect("an order has parentheses");
    split_top(inside_parens(options, open))
        .into_iter()
        .map(|item| {
            let mut words = item.split_whitespace();
            let column = words.next().expect("an order names a column").to_owned();
            let order = if words.next() == Some("desc") {
                "desc"
            } else {
                "asc"
            };
            (column, order)
        })
        .collect()
}

/// Build the described columns from typed columns and a primary key
///
/// # Arguments
///
/// * `types` - Each column's name and type, in declaration order
/// * `key` - The text inside the `PRIMARY KEY` clause's parentheses
/// * `options` - The text after `WITH`, if any
fn schema_columns(
    types: &[(String, String)],
    key: &str,
    options: &str,
) -> BTreeMap<String, SchemaColumn> {
    let (partition, clustering) = parse_key(key);
    let orders = parse_order(options);
    types
        .iter()
        .map(|(name, typ)| {
            let partition_position = partition.iter().position(|column| column == name);
            let cluster_position = clustering.iter().position(|column| column == name);
            // unlisted clustering columns are ascending, as Scylla stores them
            let column = match (partition_position, cluster_position) {
                (Some(position), _) => SchemaColumn {
                    typ: typ.clone(),
                    kind: "partition_key",
                    position: i32::try_from(position).unwrap(),
                    order: "none",
                },
                (None, Some(position)) => SchemaColumn {
                    typ: typ.clone(),
                    kind: "clustering",
                    position: i32::try_from(position).unwrap(),
                    order: orders.get(name).copied().unwrap_or("asc"),
                },
                (None, None) => SchemaColumn {
                    typ: typ.clone(),
                    kind: "regular",
                    position: -1,
                    order: "none",
                },
            };
            (name.clone(), column)
        })
        .collect()
}

/// Get the object name following `{ns}.` at the start of some text
///
/// # Arguments
///
/// * `text` - The text starting with the qualified name
fn object_name(text: &str) -> String {
    let rest = text
        .trim_start()
        .strip_prefix("{ns}.")
        .expect("names are qualified");
    rest.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .next()
        .expect("a name")
        .to_owned()
}

/// Parse a `CREATE TABLE` or `CREATE MATERIALIZED VIEW` statement into the schema it creates
///
/// # Arguments
///
/// * `cql` - The statement, with a `{ns}` keyspace
/// * `tables` - The tables parsed so far, which give a view's column types
fn parse_create(cql: &str, tables: &BTreeMap<String, SchemaObject>) -> (String, SchemaObject) {
    let cql = normalize(cql);
    if let Some(rest) = cql.strip_prefix("create table if not exists ") {
        let name = object_name(rest);
        // the column list runs from the first parenthesis to its match
        let open = rest.find('(').expect("a table has columns");
        let body = inside_parens(rest, open);
        let options = rest[open + body.len() + 2..]
            .trim()
            .strip_prefix("with ")
            .unwrap_or("");
        let mut types = Vec::new();
        let mut key = String::new();
        for item in split_top(body) {
            if let Some(clause) = item.strip_prefix("primary key") {
                let open = clause.find('(').expect("a key has parentheses");
                key = inside_parens(clause, open).to_owned();
            } else {
                let (column, typ) = item.split_once(' ').expect("a column has a type");
                types.push((column.to_owned(), typ.replace(' ', "")));
            }
        }
        let columns = schema_columns(&types, &key, options);
        let object = SchemaObject {
            kind: "table",
            base: None,
            columns,
            filter: None,
        };
        return (name, object);
    }
    let rest = cql
        .strip_prefix("create materialized view if not exists ")
        .unwrap_or_else(|| panic!("not a create statement: {cql}"));
    let name = object_name(rest);
    let select = &rest[rest.find(" as select ").expect("a view selects") + 11..];
    let (selected, after) = select
        .split_once(" from ")
        .expect("a view selects from its base");
    let base = object_name(after);
    let (_, conditions) = after
        .split_once(" where ")
        .expect("a view filters its base");
    let key_start = conditions.find("primary key").expect("a view has a key");
    let (conditions, key_clause) = conditions.split_at(key_start);
    let open = key_clause.find('(').expect("a key has parentheses");
    let key = inside_parens(key_clause, open);
    let options = key_clause[open + key.len() + 2..]
        .trim()
        .strip_prefix("with ")
        .unwrap_or("");
    // a view's column types are its base's
    let base_columns = &tables
        .get(&base)
        .unwrap_or_else(|| panic!("{name}'s base {base} wasn't parsed first"))
        .columns;
    // Scylla stores every key column in a view, selected or not
    let (partition, clustering) = parse_key(key);
    let mut columns = split_top(selected);
    for column in partition.into_iter().chain(clustering) {
        if !columns.contains(&column) {
            columns.push(column);
        }
    }
    let types = columns
        .into_iter()
        .map(|column| {
            let typ = base_columns
                .get(&column)
                .unwrap_or_else(|| panic!("{name}'s column {column} isn't in {base}"))
                .typ
                .clone();
            (column, typ)
        })
        .collect::<Vec<_>>();
    // the `IS NOT NULL` conditions are implied by the key, so only the rest is kept
    let filter = conditions
        .split(" and ")
        .map(str::trim)
        .filter(|condition| !condition.ends_with("is not null"))
        .collect::<Vec<_>>();
    let object = SchemaObject {
        kind: "view",
        base: Some(base),
        columns: schema_columns(&types, key, options),
        filter: (!filter.is_empty()).then(|| filter.join(" and ")),
    };
    (name, object)
}

/// Parse a list of statements separated by semicolons, in creation order
///
/// # Arguments
///
/// * `cql` - The statements
fn parse_all(cql: &str) -> BTreeMap<String, SchemaObject> {
    let mut objects = BTreeMap::new();
    for statement in cql
        .split(';')
        .filter(|statement| !statement.trim().is_empty())
    {
        let (name, object) = parse_create(statement, &objects);
        objects.insert(name, object);
    }
    objects
}

/// Describe a declared object the way the schema tables would
///
/// # Arguments
///
/// * `object` - The declared object
fn describe(object: &ObjectDef) -> SchemaObject {
    let columns = object
        .columns()
        .iter()
        .map(|column| {
            let typ = column.desc.render().replace(' ', "");
            let described = match column.role {
                ColumnRole::Partition(position) => SchemaColumn {
                    typ,
                    kind: "partition_key",
                    position: i32::from(position),
                    order: "none",
                },
                ColumnRole::Cluster(position, order) => SchemaColumn {
                    typ,
                    kind: "clustering",
                    position: i32::from(position),
                    order: if order == ClusterOrder::Desc {
                        "desc"
                    } else {
                        "asc"
                    },
                },
                ColumnRole::Static => SchemaColumn {
                    typ,
                    kind: "static",
                    position: -1,
                    order: "none",
                },
                ColumnRole::Regular => SchemaColumn {
                    typ,
                    kind: "regular",
                    position: -1,
                    order: "none",
                },
            };
            (column.name.to_owned(), described)
        })
        .collect();
    match object {
        ObjectDef::Table(_) => SchemaObject {
            kind: "table",
            base: None,
            columns,
            filter: None,
        },
        ObjectDef::View(view) => SchemaObject {
            kind: "view",
            base: Some(view.base.to_owned()),
            columns,
            filter: view.filter.map(normalize),
        },
    }
}

/// Render every declared object's create statement with a `{ns}` keyspace
fn declared_ddl() -> String {
    <ThoriumKeyspace as Keyspace>::OBJECTS
        .iter()
        .map(|object| {
            // tables with a default TTL render one
            let ttl = match object {
                ObjectDef::Table(table) => table.ttl.map(|_| GOLDEN_TTL),
                ObjectDef::View(_) => None,
            };
            format!("{};\n", object.create_cql("{ns}", ttl))
        })
        .collect::<Vec<_>>()
        .concat()
}

/// Read a golden file under `api/src/models/schema/goldens`, or rewrite it when
/// `THORIUM_BLESS=1`
///
/// # Arguments
///
/// * `name` - The golden file's name
/// * `actual` - The generated text, written when blessing
fn golden(name: &str, actual: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/models/schema/goldens")
        .join(name);
    if std::env::var("THORIUM_BLESS").is_ok_and(|value| value == "1") {
        std::fs::write(&path, actual).expect("golden file is written");
    }
    std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("{} can't be read: {err}", path.display()))
}

/// The migration-state tables, declared after the setup statements were replaced, so they have no
/// setup statement to match
const STATE_TABLES: [&str; 2] = ["schema_migrations", "schema_migration_issues"];

/// Every declared table and view creates the same schema as the setup statements Thorium ran
/// before its schema was declared, compared as Scylla's schema tables describe it; the
/// migration-state tables, which no setup statement created, are left out
#[test]
fn declared_schema_matches_setup_statements() {
    // the setup statements as they were written before the schema was declared
    let legacy_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/models/schema/goldens/legacy_ddl.cql");
    let legacy = parse_all(&std::fs::read_to_string(legacy_path).expect("legacy DDL reads"));
    let mut generated = parse_all(&declared_ddl());
    // the migration-state tables have no setup statement: only `schema apply` creates them
    for name in STATE_TABLES {
        assert!(generated.remove(name).is_some(), "{name} isn't declared");
    }
    let objects = <ThoriumKeyspace as Keyspace>::OBJECTS
        .iter()
        .filter(|object| !STATE_TABLES.contains(&object.name()))
        .collect::<Vec<_>>();
    // the same 16 tables and 12 views on both sides
    assert_eq!(
        objects
            .iter()
            .filter(|object| matches!(object, ObjectDef::Table(_)))
            .count(),
        16
    );
    assert_eq!(
        objects
            .iter()
            .filter(|object| matches!(object, ObjectDef::View(_)))
            .count(),
        12
    );
    assert_eq!(
        legacy.keys().collect::<Vec<_>>(),
        generated.keys().collect::<Vec<_>>()
    );
    for object in objects {
        let name = object.name();
        // the rendered statement creates what the setup statement created
        assert_eq!(
            generated[name], legacy[name],
            "{name} differs from its setup statement"
        );
        // and what the declaration describes
        assert_eq!(
            describe(object),
            generated[name],
            "{name} renders a different schema"
        );
    }
    // the stored partial clustering orders of the samples and repos views are reproduced
    let order = |object: &str, column: &str| generated[object].columns[column].order;
    assert_eq!(
        ["group", "uploaded", "id", "year", "bucket"].map(|column| order("samples", column)),
        ["asc", "desc", "desc", "asc", "asc"]
    );
    assert_eq!(
        ["group", "uploaded", "year", "bucket", "id"].map(|column| order("repos", column)),
        ["asc", "desc", "desc", "desc", "desc"]
    );
}

/// The rendered DDL of every declared object matches its committed text
#[test]
fn declared_ddl_matches_golden() {
    let rendered = declared_ddl();
    let expected = golden("ddl.cql", &rendered);
    assert_eq!(
        normalize(&rendered),
        normalize(&expected),
        "the declared DDL changed; rerun with THORIUM_BLESS=1 to accept the new text"
    );
}

/// The text type
const TEXT: CqlDesc = CqlDesc::Native(CqlNative::Text);

/// The int type
const INT: CqlDesc = CqlDesc::Native(CqlNative::Int);

/// Build a fixture column
///
/// # Arguments
///
/// * `name` - The column's name
/// * `desc` - The column's type
/// * `role` - The column's role
fn column(name: &'static str, desc: CqlDesc, role: ColumnRole) -> ColumnDef {
    ColumnDef {
        name,
        desc,
        role,
        security_sensitive: false,
    }
}

/// Build a fixture table
///
/// # Arguments
///
/// * `name` - The table's name
/// * `columns` - The table's columns
fn table(name: &'static str, columns: Vec<ColumnDef>) -> ObjectDef {
    ObjectDef::Table(TableDef {
        name,
        columns: Box::leak(columns.into_boxed_slice()),
        ttl: None,
        partition_size: None,
    })
}

/// Build a fixture view
///
/// # Arguments
///
/// * `name` - The view's name
/// * `base` - The view's base table
/// * `columns` - The view's columns
/// * `filter` - The view's key filter
fn view(
    name: &'static str,
    base: &'static str,
    columns: Vec<ColumnDef>,
    filter: Option<&'static str>,
) -> ObjectDef {
    ObjectDef::View(ViewDef {
        name,
        base,
        columns: Box::leak(columns.into_boxed_slice()),
        filter,
    })
}

/// A valid fixture base table: partition `group`, clustering `id`, and regular `sha256` and
/// `name`
fn base() -> ObjectDef {
    table(
        "base",
        vec![
            column("group", TEXT, ColumnRole::Partition(0)),
            column("id", TEXT, ColumnRole::Cluster(0, ClusterOrder::Asc)),
            column("sha256", TEXT, ColumnRole::Regular),
            column("name", TEXT, ColumnRole::Regular),
        ],
    )
}

/// Validate fixture objects, returning the broken rules
///
/// # Arguments
///
/// * `objects` - The fixture objects in creation order
fn errors(objects: &[ObjectDef]) -> Vec<RegistryError> {
    validate(objects).err().unwrap_or_default()
}

/// The declared keyspace follows every rule, and each table rule rejects a fixture that breaks
/// it
#[test]
fn registry_rules_reject_broken_tables() {
    validate(<ThoriumKeyspace as Keyspace>::OBJECTS).expect("the declared keyspace validates");
    assert_eq!(errors(&[base()]), Vec::new());
    // a table needs a partition key
    assert_eq!(
        errors(&[table(
            "bare",
            vec![column("name", TEXT, ColumnRole::Regular)]
        )]),
        vec![RegistryError::NoPartitionKey { object: "bare" }]
    );
    // clustering columns come after partition columns
    let early = table(
        "early",
        vec![
            column("id", TEXT, ColumnRole::Cluster(0, ClusterOrder::Asc)),
            column("group", TEXT, ColumnRole::Partition(0)),
        ],
    );
    assert_eq!(
        errors(&[early]),
        vec![RegistryError::ClusterBeforePartition {
            object: "early",
            column: "id"
        }]
    );
    // key positions count up from 0
    let gap = table("gap", vec![column("group", TEXT, ColumnRole::Partition(1))]);
    assert_eq!(
        errors(&[gap]),
        vec![RegistryError::KeyPositions { object: "gap" }]
    );
    // names are unique
    assert_eq!(
        errors(&[base(), base()]),
        vec![RegistryError::DuplicateName { object: "base" }]
    );
}

/// Each view rule rejects a fixture that breaks it
#[test]
fn registry_rules_reject_broken_views() {
    let key = |name| column(name, TEXT, ColumnRole::Partition(0));
    let cluster =
        |name, position| column(name, TEXT, ColumnRole::Cluster(position, ClusterOrder::Asc));
    // a valid view keyed by one non-key base column
    let by_sha256 = view(
        "by_sha256",
        "base",
        vec![key("sha256"), cluster("group", 0), cluster("id", 1)],
        None,
    );
    assert_eq!(errors(&[base(), by_sha256]), Vec::new());
    // the view's key holds every base key column
    let missing = view(
        "missing",
        "base",
        vec![key("sha256"), cluster("group", 0)],
        None,
    );
    assert_eq!(
        errors(&[base(), missing]),
        vec![RegistryError::BaseKeyMissing {
            view: "missing",
            column: "id"
        }]
    );
    // view columns exist in the base with the same type
    let mismatch = view(
        "mismatch",
        "base",
        vec![
            key("group"),
            cluster("id", 0),
            column("name", INT, ColumnRole::Regular),
        ],
        None,
    );
    assert_eq!(
        errors(&[base(), mismatch]),
        vec![RegistryError::ColumnTypeMismatch {
            view: "mismatch",
            column: "name"
        }]
    );
    let absent = view(
        "absent",
        "base",
        vec![
            key("group"),
            cluster("id", 0),
            column("nope", TEXT, ColumnRole::Regular),
        ],
        None,
    );
    assert_eq!(
        errors(&[base(), absent]),
        vec![RegistryError::ColumnNotInBase {
            view: "absent",
            column: "nope"
        }]
    );
    // at most one non-key base column in the view's key
    let two = view(
        "two",
        "base",
        vec![
            key("sha256"),
            cluster("name", 0),
            cluster("group", 1),
            cluster("id", 2),
        ],
        None,
    );
    assert_eq!(
        errors(&[base(), two]),
        vec![RegistryError::TooManyNonKeyColumns {
            view: "two",
            columns: vec!["sha256", "name"]
        }]
    );
    // a view comes after its base
    assert_eq!(
        errors(&[by_sha256, base()]),
        vec![RegistryError::UnknownBase {
            view: "by_sha256",
            base: "base"
        }]
    );
    // a filter restricts one of the view's key columns
    let filtered = view(
        "filtered",
        "base",
        vec![key("group"), cluster("id", 0)],
        Some("name = 'corn'"),
    );
    assert_eq!(
        errors(&[base(), filtered]),
        vec![RegistryError::FilterNotOnKey {
            view: "filtered",
            filter: "name = 'corn'"
        }]
    );
}

/// A view renders `IS NOT NULL` for every key column except the one its key filter replaces
#[test]
fn views_filter_every_key_column() {
    // the default network policies view sets every key column but the filtered one
    let ObjectDef::View(defaults) = <NetworkPoliciesDefaultView as ScyllaObject>::DEF else {
        panic!("the default network policies object is a view");
    };
    let cql = normalize(&defaults.create_cql("{ns}"));
    let conditions = cql
        .split(" where ")
        .nth(1)
        .and_then(|rest| rest.split(" primary key").next())
        .expect("the view has conditions");
    assert_eq!(
        conditions,
        "group is not null and name is not null and default_policy = true"
    );
}

/// The repos list table, its borrowed row, and the repos view never print the credentials a
/// repo's clone scheme holds
#[test]
fn repo_credentials_never_print() {
    use super::scylla::{ReposListTable, ReposView};
    use crate::models::{RepoCheckout, RepoScheme};
    use chrono::Utc;
    use uuid::Uuid;
    // a repo cloned with a username and password
    let scheme = RepoScheme::HttpsAuthed {
        username: "europa".to_owned(),
        password: "hunter2".to_owned(),
    };
    let listed = ReposListTable {
        group: "minerva".to_owned(),
        year: 2026,
        bucket: 7,
        uploaded: Utc::now(),
        id: Uuid::from_u128(7),
        url: "github.com/curl/curl".to_owned(),
        provider: "github.com".to_owned(),
        user: "curl".to_owned(),
        name: "curl".to_owned(),
        creator: "io".to_owned(),
        scheme: scheme.clone(),
        default_checkout: Some(RepoCheckout::Branch("main".to_owned())),
        earliest: None,
    };
    let viewed = ReposView {
        url: listed.url.clone(),
        group: listed.group.clone(),
        uploaded: listed.uploaded,
        year: listed.year,
        bucket: listed.bucket,
        id: listed.id,
        provider: listed.provider.clone(),
        user: listed.user.clone(),
        name: listed.name.clone(),
        creator: listed.creator.clone(),
        scheme,
        default_checkout: None,
        earliest: None,
    };
    // every generated Debug hides the scheme and prints the rest
    let printed = [
        format!("{listed:?}"),
        format!("{:?}", listed.as_ref()),
        format!("{viewed:?}"),
    ];
    for printed in printed {
        assert!(printed.contains("<redacted>"), "{printed}");
        assert!(!printed.contains("hunter2"), "{printed}");
        assert!(!printed.contains("europa"), "{printed}");
        assert!(printed.contains("github.com/curl/curl"), "{printed}");
    }
}
