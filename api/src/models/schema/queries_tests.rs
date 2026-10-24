//! Tests of the generated queries: projection columns, statement text, and chunk plans

use std::collections::{BTreeMap, BTreeSet};

use super::queries::migrations::SchemaStateQueries;
use super::queries::{
    CommentsByIdQueries, CommentsQueries, SamplesListQueries, SamplesQueries, ScyllaQueries,
};
use super::scylla::{
    AssociationsTable, CommentsTable, CommitishListTable, CommitishesTable, EntitiesTable,
    LogsTable, NetworkPoliciesTable, NodesTable, NotificationsTable, RepoDataTable, ReposListTable,
    ResultsStreamTable, ResultsTable, S3IdsTable, SamplesListTable, SchemaMigrationIssuesTable,
    SchemaMigrationsTable, TagsTable,
};
use super::{InKind, InList, Projection, TableStatements, plan_in_chunks};
use crate::models::{
    AssociationListRow, BasicSubmissionRow, CommentIdRow, CommentRow, CommitishDataRow,
    CommitishListRow, CommitishRepoDataRow, EntityListRow, EntityListSupplementRow, EntityRow,
    FullTagRow, LogLineRow, NetworkPolicyGroupRow, NetworkPolicyListRow, NetworkPolicyRow,
    NodeListLine, NodeRow, NotificationRow, OutputIdRow, OutputRow, RepoAuthRow, RepoCheckout,
    RepoDataHashRow, RepoListRow, RepoRow, RepoScheme, ResultIdRow, ResultUploadedRow, S3IdRow,
    Sha256Row, SubmissionListRow, SubmissionRow, TagListRow, TagRow,
};

/// The statements whose generated form returns whether a row exists, which select the
/// restricted columns instead of the one column today's text selects
const EXISTS_ONLY: [&str; 3] = [
    "s3_ids.id_exists",
    "s3_sha256s.object_exists",
    "repo_data.data_exists",
];

/// The cursor statements whose text deliberately differs from the statement they replace, with
/// their generated text: a tie query reads one partition, so it restricts its `IN` filter to
/// one value, and every tie query limits the rows it reads with `LIMIT`
const INTENDED: [(&str, &str); 3] = [
    (
        "entities.list_ties",
        "SELECT kind, group, created, id, name FROM {ns}.entities WHERE kind = ? AND group = ? \
         AND year = ? AND bucket = ? AND created = ? AND id <= ? ORDER BY created DESC, id DESC \
         LIMIT ?",
    ),
    (
        "associations.list_ties_target",
        "SELECT group, kind, source, target, submitter, created, direction, extra_source, \
         extra_target, submissions_source, submissions_other FROM {ns}.associations WHERE group \
         = ? AND year = ? AND bucket = ? AND source = ? AND created = ? AND target = ? AND \
         direction >= ? LIMIT ?",
    ),
    (
        "commitish_list.list_ties",
        "SELECT kind, group, key, timestamp FROM {ns}.commitish_list WHERE kind = ? AND group = \
         ? AND year = ? AND bucket = ? AND repo = ? AND timestamp = ? AND key <= ? LIMIT ?",
    ),
];

/// The cursor statement no generated statement replaces: a commitish tie query reads one kind,
/// so the tie query reading the kinds sorted before the tied one is gone
const REMOVED: [&str; 1] = ["commitish_list.list_ties_below"];

/// Every converted projection reads the same columns, in the same order, as today's statement
#[test]
// the test lists every converted projection's columns
#[allow(clippy::too_many_lines)]
fn projections_match_todays_selects() {
    // the columns each row struct's statement selected before it became a projection
    let expected: [(&str, &[&str], &[&str]); 32] = [
        (
            "SubmissionRow",
            SubmissionRow::COLUMNS,
            &[
                "sha256",
                "sha1",
                "md5",
                "id",
                "name",
                "description",
                "group",
                "submitter",
                "origin",
                "uploaded",
            ],
        ),
        ("Sha256Row", Sha256Row::COLUMNS, &["sha256"]),
        (
            "BasicSubmissionRow",
            BasicSubmissionRow::COLUMNS,
            &["group", "submitter", "id"],
        ),
        ("S3IdRow", S3IdRow::COLUMNS, &["id"]),
        (
            "CommentRow",
            CommentRow::COLUMNS,
            &[
                "group", "sha256", "uploaded", "id", "author", "comment", "files",
            ],
        ),
        ("CommentIdRow", CommentIdRow::COLUMNS, &["id"]),
        (
            "RepoRow",
            RepoRow::COLUMNS,
            &[
                "group",
                "provider",
                "user",
                "name",
                "url",
                "id",
                "creator",
                "uploaded",
                "scheme",
                "default_checkout",
                "earliest",
            ],
        ),
        ("RepoAuthRow", RepoAuthRow::COLUMNS, &["url"]),
        ("RepoDataHashRow", RepoDataHashRow::COLUMNS, &["hash"]),
        ("CommitishDataRow", CommitishDataRow::COLUMNS, &["data"]),
        (
            "CommitishRepoDataRow",
            CommitishRepoDataRow::COLUMNS,
            &["kind", "repo_data"],
        ),
        (
            "OutputRow",
            OutputRow::COLUMNS,
            &[
                "id",
                "tool",
                "tool_version",
                "cmd",
                "uploaded",
                "result",
                "files",
                "entities",
                "display_type",
                "children",
            ],
        ),
        (
            "ResultUploadedRow",
            ResultUploadedRow::COLUMNS,
            &["id", "uploaded"],
        ),
        ("ResultIdRow", ResultIdRow::COLUMNS, &["id"]),
        (
            "OutputIdRow",
            OutputIdRow::COLUMNS,
            &["id", "tool", "display_type", "cmd", "group", "uploaded"],
        ),
        (
            "TagRow",
            TagRow::COLUMNS,
            &["group", "item", "key", "value"],
        ),
        (
            "EntityRow",
            EntityRow::COLUMNS,
            &[
                "id",
                "group",
                "kind",
                "created",
                "name",
                "submitter",
                "kind_data",
                "description",
                "image",
            ],
        ),
        (
            "EntityListSupplementRow",
            EntityListSupplementRow::COLUMNS,
            &["id", "name", "kind"],
        ),
        (
            "NetworkPolicyRow",
            NetworkPolicyRow::COLUMNS,
            &[
                "group",
                "name",
                "id",
                "k8s_name",
                "created",
                "ingress",
                "egress",
                "forced_policy",
                "default_policy",
            ],
        ),
        (
            "NetworkPolicyListRow",
            NetworkPolicyListRow::COLUMNS,
            &["group", "name", "id"],
        ),
        (
            "NetworkPolicyGroupRow",
            NetworkPolicyGroupRow::COLUMNS,
            &["name", "group"],
        ),
        (
            "NodeRow",
            NodeRow::COLUMNS,
            &["cluster", "node", "health", "resources", "heart_beat"],
        ),
        (
            "NotificationRow",
            NotificationRow::COLUMNS,
            &["key", "created", "id", "msg", "level", "ban_id"],
        ),
        ("LogLineRow", LogLineRow::COLUMNS, &["position", "line"]),
        (
            "FullTagRow",
            FullTagRow::COLUMNS,
            &[
                "group", "year", "bucket", "key", "value", "uploaded", "item",
            ],
        ),
        (
            "SubmissionListRow",
            SubmissionListRow::COLUMNS,
            &["group", "sha256", "id", "uploaded"],
        ),
        (
            "RepoListRow",
            RepoListRow::COLUMNS,
            &["group", "url", "id", "uploaded"],
        ),
        (
            "CommitishListRow",
            CommitishListRow::COLUMNS,
            &["kind", "group", "key", "timestamp"],
        ),
        (
            "EntityListRow",
            EntityListRow::COLUMNS,
            &["kind", "group", "created", "id", "name"],
        ),
        (
            "AssociationListRow",
            AssociationListRow::COLUMNS,
            &[
                "group",
                "kind",
                "source",
                "target",
                "submitter",
                "created",
                "direction",
                "extra_source",
                "extra_target",
                "submissions_source",
                "submissions_other",
            ],
        ),
        (
            "TagListRow",
            TagListRow::COLUMNS,
            &["group", "item", "uploaded"],
        ),
        ("NodeListLine", NodeListLine::COLUMNS, &["cluster", "node"]),
    ];
    // each projection must select exactly today's list
    for (name, actual, expected) in expected {
        assert_eq!(actual, expected, "{name}");
    }
}

/// Compare generated text with a committed golden file, rewriting it when `THORIUM_BLESS=1`
///
/// # Arguments
///
/// * `name` - The golden file's name under `api/src/models/schema/goldens`
/// * `actual` - The generated text
fn assert_golden(name: &str, actual: &str) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/models/schema/goldens")
        .join(name);
    // rewrite the golden file on request instead of comparing
    if std::env::var("THORIUM_BLESS").is_ok_and(|value| value == "1") {
        std::fs::write(&path, actual).expect("golden file is written");
        return;
    }
    let expected = std::fs::read_to_string(&path).expect("golden file is readable");
    assert_eq!(
        expected, actual,
        "{name} changed; rerun with THORIUM_BLESS=1 to accept the new text"
    );
}

/// Render a table's full-row statements as golden entries
///
/// # Arguments
///
/// * `table` - The table's name
fn level_one<T: TableStatements>(table: &str) -> Vec<(String, String)> {
    vec![
        (format!("{table}.INSERT"), T::INSERT.to_owned()),
        (format!("{table}.SELECT_ALL"), T::SELECT_ALL.to_owned()),
        (format!("{table}.SCAN"), T::SCAN.to_owned()),
        (format!("{table}.CENSUS"), T::CENSUS.to_owned()),
    ]
}

/// Every generated statement, the full-row statements of each converted table first
fn generated_statements() -> Vec<(String, String)> {
    let mut statements = Vec::new();
    statements.extend(level_one::<SchemaMigrationsTable>("schema_migrations"));
    statements.extend(level_one::<SchemaMigrationIssuesTable>(
        "schema_migration_issues",
    ));
    statements.extend(level_one::<SamplesListTable>("samples_list"));
    statements.extend(level_one::<S3IdsTable>("s3_ids"));
    statements.extend(level_one::<CommentsTable>("comments"));
    statements.extend(level_one::<ReposListTable>("repos_list"));
    statements.extend(level_one::<RepoDataTable>("repo_data"));
    statements.extend(level_one::<CommitishesTable>("commitishes"));
    statements.extend(level_one::<CommitishListTable>("commitish_list"));
    statements.extend(level_one::<ResultsTable>("results"));
    statements.extend(level_one::<ResultsStreamTable>("results_stream"));
    statements.extend(level_one::<TagsTable>("tags"));
    statements.extend(level_one::<EntitiesTable>("entities"));
    statements.extend(level_one::<AssociationsTable>("associations"));
    statements.extend(level_one::<NetworkPoliciesTable>("network_policies"));
    statements.extend(level_one::<NodesTable>("nodes"));
    statements.extend(level_one::<NotificationsTable>("notifications"));
    statements.extend(level_one::<LogsTable>("logs"));
    statements.extend(ScyllaQueries::statements("{ns}"));
    statements.extend(SchemaStateQueries::statements("{ns}"));
    statements
}

/// Every generated statement matches its committed golden text
#[test]
fn statements_match_goldens() {
    // render each statement under its name
    let rendered = generated_statements()
        .into_iter()
        .map(|(name, cql)| format!("-- {name}\n{cql}\n"))
        .collect::<Vec<_>>()
        .concat();
    assert_golden("statements.cql", &rendered);
}

/// A statement split into the parts whose meaning doesn't depend on their order or spelling
#[derive(Debug, PartialEq, Eq)]
struct Normalized {
    /// The statement's verb
    verb: String,
    /// The object it targets
    target: String,
    /// The selected columns, in order, or the inserted or set columns, sorted
    columns: Vec<String>,
    /// The `WHERE` predicates, sorted
    predicates: BTreeSet<String>,
    /// The clauses after `WHERE`, such as `group by sha256` and `limit 1`
    tail: Vec<String>,
}

/// Split text at the first of some keywords, returning what precedes it and the rest
///
/// # Arguments
///
/// * `text` - The lowercased text
/// * `keyword` - The keyword to split at, with surrounding spaces
fn split_at<'a>(text: &'a str, keyword: &str) -> (&'a str, Option<&'a str>) {
    match text.find(keyword) {
        Some(index) => (&text[..index], Some(&text[index + keyword.len()..])),
        None => (text, None),
    }
}

/// Split a comma-separated list into trimmed items
///
/// # Arguments
///
/// * `list` - The list
fn items(list: &str) -> Vec<String> {
    list.split(',')
        .map(|item| item.trim().to_owned())
        .filter(|item| !item.is_empty())
        .collect()
}

/// Normalize the `WHERE` clause and the clauses after it
///
/// # Arguments
///
/// * `rest` - The text after `WHERE`, if the statement has one
fn predicates_and_tail(rest: Option<&str>) -> (BTreeSet<String>, Vec<String>) {
    let Some(rest) = rest else {
        return (BTreeSet::new(), Vec::new());
    };
    // the clauses after the predicates start with one of these keywords
    let mut cut = rest.len();
    for keyword in [
        " order by ",
        " group by ",
        " per partition limit ",
        " limit ",
    ] {
        if let Some(index) = rest.find(keyword) {
            cut = cut.min(index);
        }
    }
    let (predicates, tail) = rest.split_at(cut);
    // split the predicates on `and`, normalizing the spacing around each operator
    let predicates = predicates
        .split(" and ")
        .map(|predicate| predicate.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect();
    // keep each trailing clause as its own entry
    let mut clauses = Vec::new();
    let mut tail = tail.trim();
    while !tail.is_empty() {
        let next = ["order by ", "group by ", "per partition limit ", "limit "]
            .iter()
            .filter_map(|keyword| tail[1..].find(&format!(" {keyword}")).map(|i| i + 1))
            .min()
            .unwrap_or(tail.len());
        clauses.push(tail[..next].trim().to_owned());
        tail = tail[next..].trim();
    }
    (predicates, clauses)
}

/// Normalize one statement
///
/// # Arguments
///
/// * `cql` - The statement's text
fn normalize(cql: &str) -> Normalized {
    // compare case-insensitively with single spaces, with each bind marker its own word
    let text = cql
        .replace('?', " ? ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let text = text.to_lowercase();
    let (verb, rest) = text.split_once(' ').expect("a statement has a verb");
    match verb {
        "select" => {
            let (columns, rest) = split_at(rest, " from ");
            let rest = rest.expect("a select has a from");
            let (target, rest) = split_at(rest, " where ");
            let (predicates, tail) = predicates_and_tail(rest);
            // an alias only names a selected expression's column for reads by name
            let columns = items(columns)
                .into_iter()
                .map(|item| match item.split_once(" as ") {
                    Some((expr, _)) => expr.trim().to_owned(),
                    None => item,
                })
                .collect();
            Normalized {
                verb: verb.to_owned(),
                target: target.trim().to_owned(),
                columns,
                predicates,
                tail,
            }
        }
        "insert" => {
            let rest = rest.strip_prefix("into ").expect("an insert has into");
            let (target, rest) = rest.split_once(" (").expect("an insert lists columns");
            let (columns, rest) = rest.split_once(')').expect("the column list closes");
            let mut columns = items(columns);
            columns.sort();
            let tail = rest
                .split_once(" using ")
                .map_or_else(Vec::new, |(_, using)| {
                    vec![format!("using {}", using.trim())]
                });
            Normalized {
                verb: verb.to_owned(),
                target: target.trim().to_owned(),
                columns,
                predicates: BTreeSet::new(),
                tail,
            }
        }
        "delete" => {
            let rest = rest.strip_prefix("from ").expect("a delete has from");
            let (target, rest) = split_at(rest, " where ");
            let (predicates, tail) = predicates_and_tail(rest);
            Normalized {
                verb: verb.to_owned(),
                target: target.trim().to_owned(),
                columns: Vec::new(),
                predicates,
                tail,
            }
        }
        "update" => {
            let (target, rest) = split_at(rest, " set ");
            let rest = rest.expect("an update has set");
            let (set, rest) = split_at(rest, " where ");
            let mut columns = items(set)
                .into_iter()
                .map(|item| item.split_whitespace().collect::<Vec<_>>().join(" "))
                .collect::<Vec<_>>();
            columns.sort();
            let (predicates, tail) = predicates_and_tail(rest);
            Normalized {
                verb: verb.to_owned(),
                target: target.trim().to_owned(),
                columns,
                predicates,
                tail,
            }
        }
        other => panic!("unexpected statement verb {other}"),
    }
}

/// Parse a statements file of `-- <name>` lines each followed by one statement
///
/// # Arguments
///
/// * `text` - The file's text
fn parse_statements(text: &str) -> BTreeMap<String, String> {
    let mut statements = BTreeMap::new();
    let mut lines = text.lines();
    // each name line is followed by its statement
    while let Some(line) = lines.next() {
        if let Some(name) = line.strip_prefix("-- ") {
            let cql = lines.next().expect("a name is followed by its statement");
            statements.insert(name.to_owned(), cql.to_owned());
        }
    }
    statements
}

/// Every generated statement means the same as the statement it replaced: the same target,
/// columns, predicates in any order, and trailing clauses
#[test]
fn statements_match_todays_text() {
    let legacy = parse_statements(include_str!("goldens/legacy_statements.cql"));
    let generated = ScyllaQueries::statements("{ns}")
        .into_iter()
        .collect::<BTreeMap<_, _>>();
    // every statement replaced so far has a generated form, and the other way around
    assert_eq!(
        legacy.keys().collect::<Vec<_>>(),
        generated.keys().collect::<Vec<_>>()
    );
    for (name, old) in &legacy {
        // the deliberate changes are checked against their own text
        if INTENDED.iter().any(|(intended, _)| intended == name) {
            continue;
        }
        let mut old = normalize(old);
        let mut new = normalize(&generated[name]);
        // an existence check only counts rows, so the columns it selects don't matter
        if EXISTS_ONLY.contains(&name.as_str()) {
            old.columns.clear();
            new.columns.clear();
        }
        assert_eq!(old, new, "{name}");
    }
}

/// The cursor statements that deliberately change have exactly their intended text, and the
/// statements their tie queries replace are gone
#[test]
fn cursor_statements_change_only_as_intended() {
    let generated = ScyllaQueries::statements("{ns}")
        .into_iter()
        .collect::<BTreeMap<_, _>>();
    // each changed statement reads what it read before, in the form the change gives it
    for (name, intended) in INTENDED {
        let intended = intended.split_whitespace().collect::<Vec<_>>().join(" ");
        assert_eq!(generated.get(name), Some(&intended), "{name}");
    }
    // the replaced statements are no longer prepared
    for name in REMOVED {
        assert!(!generated.contains_key(name), "{name} is still generated");
    }
}

/// The lengths of each execution's ranges in a chunk plan
///
/// # Arguments
///
/// * `lists` - The statement's `IN` lists
fn chunk_lengths(lists: &[InList]) -> Vec<Vec<usize>> {
    plan_in_chunks(lists)
        .into_iter()
        .map(|ranges| ranges.iter().map(ExactSizeIterator::len).collect())
        .collect()
}

/// Each statement's `IN` lists carry the chunk size and key kind it was declared with
#[test]
fn generated_chunk_sizes_follow_the_declarations() {
    // a sha256 on the view's partition key times a group on its clustering key, 50 at a time
    let lists = SamplesQueries::get_many_in_lists([120, 60]);
    assert_eq!(lists[0].kind, InKind::PartitionKey);
    assert_eq!(lists[1].kind, InKind::Clustering);
    let plan = chunk_lengths(&lists);
    assert_eq!(plan.len(), 6);
    assert!(plan.iter().all(|lens| lens[0] <= 50 && lens[1] <= 50));
    assert_eq!(
        chunk_lengths(&SamplesQueries::auth_in_lists([120, 60])).len(),
        6
    );
    // groups on the view's clustering key are chunked at 100
    assert_eq!(
        chunk_lengths(&SamplesQueries::get_in_lists([101])),
        vec![vec![100], vec![1]]
    );
    // a partition-key list of groups is chunked at 100
    let lists = SamplesListQueries::delete_multiple_groups_in_lists([250]);
    assert_eq!(lists[0].kind, InKind::PartitionKey);
    assert_eq!(chunk_lengths(&lists).len(), 3);
    assert_eq!(
        chunk_lengths(&CommentsQueries::get_in_lists([101])).len(),
        2
    );
    assert_eq!(
        chunk_lengths(&CommentsByIdQueries::exists_in_lists([0])),
        Vec::<Vec<usize>>::new()
    );
}

/// A subset insert's bind struct prints `<redacted>` for a redacted column and every other
/// value as it is
#[test]
fn subset_insert_rows_redact_credentials() {
    let scheme = RepoScheme::HttpsAuthed {
        username: "farmer".to_owned(),
        password: "hunter2".to_owned(),
    };
    let checkout = RepoCheckout::Branch("main".to_owned());
    let row = super::queries::ReposListInsert {
        group: "minerva",
        year: 2026,
        bucket: 3,
        uploaded: chrono::Utc::now(),
        id: uuid::Uuid::nil(),
        url: "github.com/curl/curl",
        provider: "github.com",
        user: "curl",
        name: "curl",
        creator: "europa",
        scheme: &scheme,
        default_checkout: Some(&checkout),
    };
    let printed = format!("{row:?}");
    assert!(printed.contains("scheme: <redacted>"), "{printed}");
    assert!(!printed.contains("hunter2"), "{printed}");
    assert!(printed.contains("github.com/curl/curl"), "{printed}");
}

/// The repos statements' `IN` lists keep the chunk sizes their callers used
#[test]
fn repos_chunk_sizes_follow_the_declarations() {
    use super::queries::{CommitishesQueries, ReposQueries};
    // a url on the view's partition key times a group on its clustering key, 50 at a time
    assert_eq!(
        chunk_lengths(&ReposQueries::get_many_in_lists([120, 60])).len(),
        6
    );
    assert_eq!(
        chunk_lengths(&ReposQueries::auth_in_lists([60, 60])).len(),
        4
    );
    // a commitish's kinds and groups are both partition-key lists, so their product is
    // kept within 100
    let plan = chunk_lengths(&CommitishesQueries::get_repo_data_in_lists([3, 40]));
    assert!(plan.iter().all(|lens| lens[0] * lens[1] <= 100), "{plan:?}");
}

/// The results and tags statements' `IN` lists keep the chunk sizes their callers used
#[test]
fn results_chunk_sizes_follow_the_declarations() {
    use super::queries::{ResultsAuthQueries, ResultsQueries, TagsByItemQueries};
    // groups times tools on the view's clustering key, 10 of each at a time
    let plan = chunk_lengths(&ResultsAuthQueries::get_with_key_and_tool_in_lists([
        25, 25,
    ]));
    assert_eq!(plan.len(), 9);
    assert!(plan.iter().all(|lens| lens[0] <= 10 && lens[1] <= 10));
    // result ids and tag groups are chunked at 100
    assert_eq!(chunk_lengths(&ResultsQueries::get_in_lists([250])).len(), 3);
    assert_eq!(
        chunk_lengths(&ResultsAuthQueries::get_id_in_lists([101])).len(),
        2
    );
    assert_eq!(
        chunk_lengths(&TagsByItemQueries::get_rows_in_lists([101])).len(),
        2
    );
}

/// The entity, network policy, node, and log statements' `IN` lists keep the chunk sizes their
/// callers used
#[test]
fn entities_chunk_sizes_follow_the_declarations() {
    use super::queries::{
        AssociationsQueries, EntitiesByIdQueries, EntitiesQueries, LogsQueries,
        NetworkPoliciesByNameQueries, NetworkPoliciesQueries, NodesQueries,
    };
    // ids on the view's partition key times groups on its clustering key, 50 of each at a time
    assert_eq!(
        chunk_lengths(&EntitiesByIdQueries::get_many_in_lists([120, 60])).len(),
        6
    );
    assert_eq!(
        chunk_lengths(&NetworkPoliciesByNameQueries::get_many_in_lists([120, 60])).len(),
        6
    );
    // group lists are chunked at 100
    assert_eq!(
        chunk_lengths(&EntitiesQueries::delete_in_lists([101])).len(),
        2
    );
    assert_eq!(
        chunk_lengths(&AssociationsQueries::delete_in_lists([101])).len(),
        2
    );
    assert_eq!(
        chunk_lengths(&NetworkPoliciesQueries::delete_in_lists([101])).len(),
        2
    );
    assert_eq!(
        chunk_lengths(&EntitiesByIdQueries::get_in_lists([101])).len(),
        2
    );
    // clusters and nodes are both partition-key and clustering lists
    let plan = chunk_lengths(&NodesQueries::get_many_in_lists([2, 150]));
    assert_eq!(plan.len(), 2);
    assert!(
        plan.iter().all(|lens| lens[0] <= 100 && lens[1] <= 100),
        "{plan:?}"
    );
    // log buckets are chunked at 100
    assert_eq!(chunk_lengths(&LogsQueries::get_in_lists([101])).len(), 2);
}

/// Each generated tie comparator orders ties the way the tie query reads them from Scylla
#[test]
fn generated_comparators_follow_scylla_order() {
    use super::TimeCursorStatements;
    use super::queries::{
        AssociationsListCursor, CommitishListListCursor, EntitiesListCursor, SamplesListListCursor,
    };
    use crate::models::Directionality;
    use std::cmp::Ordering;
    use uuid::Uuid;
    // the order Scylla returned these ids from one partition clustered by them ascending
    let scylla_order = [
        "10000000-0000-1000-8000-000000000000",
        "00000000-0000-4000-8000-000000000000",
        "10000000-0000-4000-0f00-000000000000",
        "10000000-0000-4000-ff00-000000000000",
        "7fffffff-0000-4000-8000-000000000000",
        "80000000-0000-4000-8000-000000000000",
        "ffffffff-0000-4000-8000-000000000000",
    ]
    .map(|id| Uuid::parse_str(id).unwrap());
    // submission ids are clustered descending, so they're consumed largest first
    let mut ids = scylla_order.to_vec();
    ids.sort_by(SamplesListListCursor::cmp_ties);
    assert_eq!(ids, scylla_order.iter().rev().copied().collect::<Vec<_>>());
    // entity ids are clustered ascending but consumed newest first, so also largest first
    let mut ids = scylla_order.to_vec();
    ids.sort_by(EntitiesListCursor::cmp_ties);
    assert_eq!(ids, scylla_order.iter().rev().copied().collect::<Vec<_>>());
    // commitish keys are consumed largest first
    assert_eq!(
        CommitishListListCursor::cmp_ties(&"main".to_owned(), &"c9".to_owned()),
        Ordering::Less
    );
    // association targets and then directions are consumed in ascending text order
    let tie = |target: &str, direction| (target.to_owned(), direction);
    assert_eq!(
        AssociationsListCursor::cmp_ties(
            &tie("a", Directionality::To),
            &tie("b", Directionality::From)
        ),
        Ordering::Less
    );
    assert_eq!(
        AssociationsListCursor::cmp_ties(
            &tie("a", Directionality::To),
            &tie("a", Directionality::From)
        ),
        Directionality::To
            .as_str()
            .cmp(Directionality::From.as_str())
    );
    // commitish scopes compare their kind before their group, the partition key's order
    let scope = |kind, group: &str| (group.to_owned(), kind);
    assert_eq!(
        CommitishListListCursor::cmp_scopes(
            &scope(crate::models::CommitishKinds::Branch, "z"),
            &scope(crate::models::CommitishKinds::Commit, "a")
        ),
        Ordering::Less
    );
}

/// Every search streamer statement matches its committed golden text
#[cfg(feature = "search-streamer")]
#[test]
fn search_statements_match_goldens() {
    // render each statement under its name
    let rendered = super::queries::search::statements("{ns}")
        .into_iter()
        .map(|(name, cql)| format!("-- {name}\n{cql}\n"))
        .collect::<Vec<_>>()
        .concat();
    assert_golden("search_statements.cql", &rendered);
}

/// Every generated search streamer statement means the same as the statement the streamer
/// prepared itself: the same target, columns, predicates in any order, and trailing clauses
#[cfg(feature = "search-streamer")]
#[test]
fn search_statements_match_todays_text() {
    let legacy = parse_statements(include_str!("goldens/legacy_search_statements.cql"));
    let generated = super::queries::search::statements("{ns}")
        .into_iter()
        .collect::<BTreeMap<_, _>>();
    // every statement the streamer prepared has a generated form, and the other way around
    assert_eq!(
        legacy.keys().collect::<Vec<_>>(),
        generated.keys().collect::<Vec<_>>()
    );
    for (name, old) in &legacy {
        assert_eq!(normalize(old), normalize(&generated[name]), "{name}");
    }
}
