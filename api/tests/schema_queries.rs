//! Tests that every generated statement runs, binds each value to its own column, and reads and
//! writes the same rows as the statements it replaced

use chrono::{TimeZone, Utc};
use scylla::client::session::Session;
use scylla::value::{CqlValue, Row};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::time::Duration;
use thorium::Error;
use thorium::models::schema::queries::{
    NodesInsert, ReposListInsert, ResultsInsert, ScyllaQueries,
};
use thorium::models::schema::scylla::{
    AssociationsTable, AssociationsTableRef, CommentsTable, CommentsTableRef, CommitishListTable,
    CommitishListTableRef, CommitishesTable, CommitishesTableRef, EntitiesTable, EntitiesTableRef,
    LogsTable, LogsTableRef, NetworkPoliciesTable, NetworkPoliciesTableRef, NodesTable,
    NotificationsTable, NotificationsTableRef, RepoDataTable, RepoDataTableRef, ReposListTable,
    ResultsStreamTable, ResultsStreamTableRef, ResultsTable, S3IdsTable, S3IdsTableRef,
    SamplesListTable, SamplesListTableRef, TagsTable, TagsTableRef, ThoriumKeyspace,
};
use thorium::models::schema::{
    JsonText, Keyspace as SchemaKeyspace, ObjectDef, ScyllaObject, ScyllaTable, text_of,
};
use thorium::models::{
    AssociationKind, CommitishKinds, Directionality, EntityKinds, EntityMetadata, ImageKey,
    ImageVersion, NetworkPolicyRule, NodeHealth, NotificationLevel, NotificationType,
    OutputDisplayType, OutputKind, RepoCheckout, RepoScheme, Resources, S3Objects, TagType,
    VendorEntity,
};
use thorium::test_utilities::{self, CONF};
use uuid::Uuid;

/// Turn any error into a test error
///
/// # Arguments
///
/// * `err` - The error to convert
#[allow(clippy::needless_pass_by_value)]
fn fail<E: std::fmt::Display>(err: E) -> Error {
    Error::new(err.to_string())
}

/// Run a CQL statement, failing with its text on error
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `cql` - The statement to run
async fn exec(session: &Session, cql: &str) -> Result<(), Error> {
    // run the statement, naming it in any error
    session
        .query_unpaged(cql, &())
        .await
        .map_err(|err| Error::new(format!("{cql}: {err}")))?;
    Ok(())
}

/// A keyspace holding some declared objects, with every generated statement prepared in it
struct Keyspace {
    /// The Scylla session
    session: Session,
    /// The keyspace's name
    name: String,
    /// The generated statements
    queries: ScyllaQueries,
}

impl Keyspace {
    /// Create a fresh keyspace with every declared object, from its generated DDL, and prepare
    /// every generated statement in it
    ///
    /// # Arguments
    ///
    /// * `suffix` - What makes this test's keyspace name unique
    async fn new(suffix: &str) -> Result<Self, Error> {
        let name = format!("{}_sq_{suffix}", CONF.thorium.namespace);
        let session = test_utilities::scylla_session().await?;
        // replace any keyspace an earlier run left behind
        exec(&session, &format!("DROP KEYSPACE IF EXISTS {name}")).await?;
        exec(
            &session,
            &format!(
                "CREATE KEYSPACE {name} WITH replication = \
                {{'class': 'SimpleStrategy', 'replication_factor': 1}}"
            ),
        )
        .await?;
        // create every declared object from its declaration, in creation order
        for object in <ThoriumKeyspace as SchemaKeyspace>::OBJECTS {
            // tables with a default TTL get an hour, so TTLs can be checked
            let ttl = match object {
                ObjectDef::Table(table) => table.ttl.map(|_| 3600),
                ObjectDef::View(_) => None,
            };
            exec(&session, &object.create_cql(&name, ttl)).await?;
        }
        let queries = Box::pin(ScyllaQueries::prepare(&session, &name))
            .await
            .map_err(fail)?;
        Ok(Keyspace {
            session,
            name,
            queries,
        })
    }

    /// Read every row of an object as raw values, in the object's declared column order
    ///
    /// # Arguments
    ///
    /// * `object` - The object to read
    async fn rows(&self, object: &ObjectDef) -> Result<Vec<Row>, Error> {
        let columns = object
            .columns()
            .iter()
            .map(|column| column.name)
            .collect::<Vec<_>>()
            .join(", ");
        let cql = format!("SELECT {columns} FROM {}.{}", self.name, object.name());
        self.session
            .query_unpaged(cql, &())
            .await
            .map_err(fail)?
            .into_rows_result()
            .map_err(fail)?
            .rows::<Row>()
            .map_err(fail)?
            .collect::<Result<Vec<Row>, _>>()
            .map_err(fail)
    }

    /// Remove this keyspace
    async fn drop(&self) -> Result<(), Error> {
        exec(
            &self.session,
            &format!("DROP KEYSPACE IF EXISTS {}", self.name),
        )
        .await
    }
}

/// Wait until a check passes, since materialized views update after their base table
///
/// # Arguments
///
/// * `check` - Returns whether the expected state has been reached
async fn eventually<F, Fut>(mut check: F) -> Result<(), Error>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<bool, Error>>,
{
    // retry for up to ten seconds
    for _ in 0..100 {
        if check().await? {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(Error::new("a view never reached the expected state"))
}

/// A submission with a distinct value in every column
///
/// # Arguments
///
/// * `group` - The submission's group
/// * `sha256` - The submission's sha256
/// * `id` - The submission's id
fn submission(group: &str, sha256: &str, id: u128) -> SamplesListTable {
    SamplesListTable {
        group: group.to_owned(),
        year: 2026,
        bucket: 41,
        uploaded: Utc.with_ymd_and_hms(2026, 10, 19, 4, 5, 6).unwrap()
            + chrono::Duration::milliseconds(i64::try_from(id % 1000).unwrap()),
        id: Uuid::from_u128(id),
        sha256: sha256.to_owned(),
        sha1: format!("sha1-{sha256}"),
        md5: format!("md5-{sha256}"),
        name: Some(format!("name-{id}")),
        description: Some(format!("description-{id}")),
        submitter: format!("submitter-{id}"),
        origin: Some(format!("{{\"origin\":{id}}}")),
    }
}

/// A comment with a distinct value in every column
///
/// # Arguments
///
/// * `group` - The comment's group
/// * `sha256` - The sha256 the comment is on
/// * `id` - The comment's id
fn comment(group: &str, sha256: &str, id: u128) -> CommentsTable {
    CommentsTable {
        group: group.to_owned(),
        sha256: sha256.to_owned(),
        uploaded: Utc.with_ymd_and_hms(2026, 3, 4, 5, 6, 7).unwrap(),
        id: Uuid::from_u128(id),
        author: format!("author-{id}"),
        comment: format!("comment-{id}"),
        files: JsonText(HashMap::from([(
            format!("file-{id}.txt"),
            Uuid::from_u128(id + 1),
        )])),
    }
}

/// The first of a statement's rows, failing if it doesn't deserialize
///
/// # Arguments
///
/// * `rows` - The rows a generated method returned
fn first<P>(
    rows: Vec<Result<P, scylla::deserialize::DeserializationError>>,
) -> Result<Option<P>, Error> {
    rows.into_iter().next().transpose().map_err(fail)
}

/// Collect a statement's rows, failing on the first row that doesn't deserialize
///
/// # Arguments
///
/// * `rows` - The rows a generated method returned
fn collect<P>(
    rows: Vec<Result<P, scylla::deserialize::DeserializationError>>,
) -> Result<Vec<P>, Error> {
    rows.into_iter()
        .collect::<Result<Vec<P>, _>>()
        .map_err(fail)
}

/// Every samples statement runs through its generated method and binds each value to its own
/// column
#[tokio::test]
// the test walks every statement of its tables in one keyspace
#[allow(clippy::too_many_lines)]
async fn samples_statements_round_trip() -> Result<(), Error> {
    let ks = Keyspace::new("samples_rt").await?;
    let (session, queries) = (&ks.session, &ks.queries);
    let sha256 = "a".repeat(64);
    // write one submission to two groups and a second submission of the same sha256
    let first = submission("minerva", &sha256, 101);
    let second = submission("mars", &sha256, 202);
    queries
        .samples_list
        .insert(session, &first.as_ref())
        .await
        .map_err(fail)?;
    queries
        .samples_list
        .insert(
            session,
            &SamplesListTableRef {
                group: "vesta",
                ..first.as_ref()
            },
        )
        .await
        .map_err(fail)?;
    queries
        .samples_list
        .insert(session, &second.as_ref())
        .await
        .map_err(fail)?;
    let groups = vec!["minerva".to_owned(), "mars".to_owned(), "vesta".to_owned()];
    // the view returns every column of each submission as written
    eventually(|| async {
        let rows = queries
            .samples
            .get(session, &sha256, &groups)
            .await
            .map_err(fail)?;
        Ok(rows.len() == 3)
    })
    .await?;
    let rows = collect(
        queries
            .samples
            .get(session, &sha256, &groups)
            .await
            .map_err(fail)?,
    )?;
    let row = rows
        .iter()
        .find(|row| row.group == "minerva")
        .expect("minerva's row");
    assert_eq!(row.sha256, first.sha256);
    assert_eq!(row.sha1, first.sha1);
    assert_eq!(row.md5, first.md5);
    assert_eq!(row.id, first.id);
    assert_eq!(row.name, first.name);
    assert_eq!(row.description, first.description);
    assert_eq!(row.submitter, first.submitter);
    assert_eq!(row.origin, first.origin);
    assert_eq!(row.uploaded, first.uploaded);
    // get_many returns the same rows for a list of sha256s
    let many = collect(
        queries
            .samples
            .get_many(session, std::slice::from_ref(&sha256), &groups)
            .await
            .map_err(fail)?,
    )?;
    assert_eq!(many.len(), 3);
    // auth groups the visible rows by sha256
    let authed = collect(
        queries
            .samples
            .auth(session, std::slice::from_ref(&sha256), &groups)
            .await
            .map_err(fail)?,
    )?;
    assert_eq!(
        authed
            .iter()
            .map(|row| row.sha256.as_str())
            .collect::<Vec<_>>(),
        [sha256.as_str()]
    );
    assert!(
        queries
            .samples
            .auth_any(session, std::slice::from_ref(&sha256), &groups)
            .await
            .map_err(fail)?
    );
    // the basic info keeps group, submitter, and id apart
    let basic = collect(
        queries
            .samples
            .get_basic_submission_info(session, &sha256)
            .await
            .map_err(fail)?,
    )?;
    let mars = basic
        .iter()
        .find(|row| row.group == "mars")
        .expect("mars' row");
    assert_eq!(mars.submitter, second.submitter);
    assert_eq!(mars.id, second.id);
    // deleting the first submission from two groups leaves the other rows
    queries
        .samples_list
        .delete_multiple_groups(
            session,
            &["minerva".to_owned(), "vesta".to_owned()],
            first.year,
            first.bucket,
            first.uploaded,
            first.id,
        )
        .await
        .map_err(fail)?;
    let remaining = ks.rows(&<SamplesListTable as ScyllaObject>::DEF).await?;
    assert_eq!(remaining.len(), 1, "{remaining:?}");
    ks.drop().await
}

/// Every S3 id statement runs through its generated method
#[tokio::test]
async fn s3_statements_round_trip() -> Result<(), Error> {
    let ks = Keyspace::new("s3_rt").await?;
    let (session, queries) = (&ks.session, &ks.queries);
    let (file_id, repo_id) = (Uuid::from_u128(11), Uuid::from_u128(22));
    let repo_key = "github.com/curl/curl/abc";
    // write a file id and a repo id
    let file = S3IdsTable {
        object_type: S3Objects::File,
        id: file_id,
        sha256: "b".repeat(64),
    };
    queries
        .s3_ids
        .insert(session, &file.as_ref())
        .await
        .map_err(fail)?;
    queries
        .s3_ids
        .insert(
            session,
            &S3IdsTableRef {
                object_type: &S3Objects::Repo,
                id: repo_id,
                sha256: repo_key,
            },
        )
        .await
        .map_err(fail)?;
    // each id exists under its own type
    assert!(
        queries
            .s3_ids
            .id_exists(session, &S3Objects::File, file_id)
            .await
            .map_err(fail)?
    );
    assert!(
        queries
            .s3_ids
            .id_exists(session, &S3Objects::Repo, repo_id)
            .await
            .map_err(fail)?
    );
    // each key maps back to its id through the view
    eventually(|| async {
        let found = queries
            .s3_sha256s
            .get(session, repo_key, &S3Objects::Repo)
            .await
            .map_err(fail)?;
        Ok(found.is_some_and(|row| row.id == repo_id))
    })
    .await?;
    let found = queries
        .s3_sha256s
        .get(session, &file.sha256, &S3Objects::File)
        .await
        .map_err(fail)?;
    assert_eq!(found.map(|row| row.id), Some(file_id));
    assert!(
        queries
            .s3_sha256s
            .object_exists(session, &file.sha256, &S3Objects::File)
            .await
            .map_err(fail)?
    );
    // deleting the file id removes it from the table and then the view
    queries
        .s3_ids
        .delete(session, &S3Objects::File, file_id)
        .await
        .map_err(fail)?;
    assert!(
        !queries
            .s3_ids
            .id_exists(session, &S3Objects::File, file_id)
            .await
            .map_err(fail)?
    );
    eventually(|| async {
        let exists = queries
            .s3_sha256s
            .object_exists(session, &file.sha256, &S3Objects::File)
            .await
            .map_err(fail)?;
        Ok(!exists)
    })
    .await?;
    assert!(
        queries
            .s3_ids
            .id_exists(session, &S3Objects::Repo, repo_id)
            .await
            .map_err(fail)?
    );
    ks.drop().await
}

/// Every comments statement runs through its generated method and binds each value to its own
/// column
#[tokio::test]
async fn comments_statements_round_trip() -> Result<(), Error> {
    let ks = Keyspace::new("comments_rt").await?;
    let (session, queries) = (&ks.session, &ks.queries);
    let sha256 = "c".repeat(64);
    // write one comment to two groups
    let first = comment("ceres", &sha256, 7);
    queries
        .comments
        .insert(session, &first.as_ref())
        .await
        .map_err(fail)?;
    queries
        .comments
        .insert(
            session,
            &CommentsTableRef {
                group: "pallas",
                ..first.as_ref()
            },
        )
        .await
        .map_err(fail)?;
    let groups = vec!["ceres".to_owned(), "pallas".to_owned()];
    let rows = collect(
        queries
            .comments
            .get(session, &groups, &sha256)
            .await
            .map_err(fail)?,
    )?;
    assert_eq!(rows.len(), 2);
    let row = &rows[0];
    assert_eq!(row.sha256, first.sha256);
    assert_eq!(row.uploaded, first.uploaded);
    assert_eq!(row.id, first.id);
    assert_eq!(row.author, first.author);
    assert_eq!(row.comment, first.comment);
    assert_eq!(
        row.files,
        serde_json::to_string(&first.files.0).map_err(fail)?
    );
    // the comment exists by id until it's deleted from both groups
    eventually(|| async {
        let found = queries
            .comments_by_id
            .exists(session, &[first.id])
            .await
            .map_err(fail)?;
        Ok(found.len() == 1)
    })
    .await?;
    for group in &groups {
        queries
            .comments
            .delete(session, group, &sha256, first.uploaded, first.id)
            .await
            .map_err(fail)?;
    }
    assert!(
        collect(
            queries
                .comments
                .get(session, &groups, &sha256)
                .await
                .map_err(fail)?
        )?
        .is_empty()
    );
    eventually(|| async {
        let found = queries
            .comments_by_id
            .exists(session, &[first.id])
            .await
            .map_err(fail)?;
        Ok(found.is_empty())
    })
    .await?;
    ks.drop().await
}

/// `IN` lists longer than one chunk return the same rows as a single statement would
#[tokio::test]
async fn long_in_lists_return_every_row() -> Result<(), Error> {
    let ks = Keyspace::new("chunks").await?;
    let (session, queries) = (&ks.session, &ks.queries);
    let sha256 = "d".repeat(64);
    // 101 groups, with a submission and a comment in the first and the last
    let groups = (0..101).map(|i| format!("group{i:03}")).collect::<Vec<_>>();
    for (i, group) in [(1, &groups[0]), (2, &groups[100])] {
        queries
            .samples_list
            .insert(session, &submission(group, &sha256, i).as_ref())
            .await
            .map_err(fail)?;
        queries
            .comments
            .insert(session, &comment(group, &sha256, 10 + i).as_ref())
            .await
            .map_err(fail)?;
    }
    // reads over both chunks of groups see both rows
    eventually(|| async {
        let rows = queries
            .samples
            .get(session, &sha256, &groups)
            .await
            .map_err(fail)?;
        Ok(rows.len() == 2)
    })
    .await?;
    let comments = queries
        .comments
        .get(session, &groups, &sha256)
        .await
        .map_err(fail)?;
    assert_eq!(comments.len(), 2);
    // 101 sha256s, with this one the last, find it in the second chunk
    let mut sha256s = (0..100).map(|i| format!("{i:064}")).collect::<Vec<_>>();
    sha256s.push(sha256.clone());
    let many = queries
        .samples
        .get_many(session, &sha256s, &groups)
        .await
        .map_err(fail)?;
    assert_eq!(many.len(), 2);
    // each chunk of groups groups its own rows, so the sha256 is found once per chunk
    let authed = collect(
        queries
            .samples
            .auth(session, &sha256s, &groups)
            .await
            .map_err(fail)?,
    )?;
    let authed = authed
        .into_iter()
        .map(|row| row.sha256)
        .collect::<HashSet<_>>();
    assert_eq!(authed, HashSet::from([sha256.clone()]));
    // 101 comment ids, with a real one last
    let mut ids = (100..200).map(Uuid::from_u128).collect::<Vec<_>>();
    ids.push(Uuid::from_u128(12));
    eventually(|| async {
        let found = queries
            .comments_by_id
            .exists(session, &ids)
            .await
            .map_err(fail)?;
        Ok(found.len() == 1)
    })
    .await?;
    // a delete over both chunks of groups removes both rows
    for (i, group) in [(1, &groups[0]), (2, &groups[100])] {
        let row = submission(group, &sha256, i);
        queries
            .samples_list
            .delete_multiple_groups(session, &groups, row.year, row.bucket, row.uploaded, row.id)
            .await
            .map_err(fail)?;
    }
    let remaining = ks.rows(&<SamplesListTable as ScyllaObject>::DEF).await?;
    assert_eq!(remaining, Vec::<Row>::new());
    ks.drop().await
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

/// The statements the generated ones replaced, as the API prepared them
fn legacy_statements(ks: &str) -> BTreeMap<String, String> {
    parse_statements(include_str!(
        "../src/models/schema/goldens/legacy_statements.cql"
    ))
    .into_iter()
    .map(|(name, cql)| (name, cql.replace("{ns}", ks)))
    .collect()
}

/// Key a table's raw rows by one of their columns
///
/// # Arguments
///
/// * `rows` - The raw rows
/// * `column` - The index of the column to key by
fn keyed(rows: Vec<Row>, column: usize) -> HashMap<String, Vec<Option<CqlValue>>> {
    rows.into_iter()
        .map(|mut row| {
            let key = match row.columns[column].take() {
                Some(CqlValue::Text(text)) => text,
                other => panic!("unexpected key {other:?}"),
            };
            (key, row.columns)
        })
        .collect()
}

/// Rows written by the replaced statements, with the binds the API used, read back through the
/// generated statements and rewritten by them store identical values
#[tokio::test]
// the test walks every statement of its tables in one keyspace
#[allow(clippy::too_many_lines)]
async fn rows_match_the_replaced_statements() -> Result<(), Error> {
    let ks = Keyspace::new("parity").await?;
    let (session, queries) = (&ks.session, &ks.queries);
    let legacy = legacy_statements(&ks.name);
    let sha256 = "e".repeat(64);
    let row = submission("old", &sha256, 31);
    // write a submission the way the API did, binding a positional tuple
    let insert = session
        .prepare(legacy["samples_list.insert"].clone())
        .await
        .map_err(fail)?;
    session
        .execute_unpaged(
            &insert,
            (
                "old",
                &row.year,
                row.bucket,
                &row.sha256,
                &row.sha1,
                &row.md5,
                &row.id,
                &row.name,
                &row.description,
                &row.submitter,
                &row.origin,
                row.uploaded,
            ),
        )
        .await
        .map_err(fail)?;
    // read it through the generated statement and write it back to another group
    let groups = vec!["old".to_owned()];
    let read = eventually(|| async {
        let rows = queries
            .samples
            .get(session, &sha256, &groups)
            .await
            .map_err(fail)?;
        Ok(rows.len() == 1)
    });
    read.await?;
    let old = collect(
        queries
            .samples
            .get(session, &sha256, &groups)
            .await
            .map_err(fail)?,
    )?
    .remove(0);
    let rewritten = SamplesListTableRef {
        group: "new",
        year: row.year,
        bucket: row.bucket,
        uploaded: old.uploaded,
        id: old.id,
        sha256: &old.sha256,
        sha1: &old.sha1,
        md5: &old.md5,
        name: old.name.as_deref(),
        description: old.description.as_deref(),
        submitter: &old.submitter,
        origin: old.origin.as_deref(),
    };
    queries
        .samples_list
        .insert(session, &rewritten)
        .await
        .map_err(fail)?;
    let rows = keyed(ks.rows(&<SamplesListTable as ScyllaObject>::DEF).await?, 0);
    assert_eq!(rows["old"], rows["new"]);
    // a comment written the way the API did, with its attachments serialized first
    let original = comment("old", &sha256, 41);
    let insert = session
        .prepare(legacy["comments.insert"].clone())
        .await
        .map_err(fail)?;
    let files = serde_json::to_string(&original.files.0).map_err(fail)?;
    session
        .execute_unpaged(
            &insert,
            (
                "old",
                &sha256,
                original.uploaded,
                &original.id,
                &original.author,
                &original.comment,
                &files,
            ),
        )
        .await
        .map_err(fail)?;
    queries
        .comments
        .insert(
            session,
            &CommentsTableRef {
                group: "new",
                ..original.as_ref()
            },
        )
        .await
        .map_err(fail)?;
    let rows = keyed(ks.rows(&<CommentsTable as ScyllaObject>::DEF).await?, 0);
    assert_eq!(rows["old"], rows["new"]);
    // an S3 id written the way the API did, with its type bound as text
    let insert = session
        .prepare(legacy["s3_ids.insert"].clone())
        .await
        .map_err(fail)?;
    session
        .execute_unpaged(&insert, ("File", Uuid::from_u128(51), "old"))
        .await
        .map_err(fail)?;
    queries
        .s3_ids
        .insert(
            session,
            &S3IdsTableRef {
                object_type: &S3Objects::File,
                id: Uuid::from_u128(51),
                sha256: "new",
            },
        )
        .await
        .map_err(fail)?;
    let rows = keyed(ks.rows(&<S3IdsTable as ScyllaObject>::DEF).await?, 2);
    assert_eq!(rows["old"], rows["new"]);
    ks.drop().await
}

/// Reading a row whose nullable non-key columns are all null keeps each projection's outcome:
/// `Option` fields read `None`, and a non-`Option` field fails to deserialize
#[tokio::test]
async fn projections_over_null_columns() -> Result<(), Error> {
    let ks = Keyspace::new("nulls").await?;
    let (session, queries) = (&ks.session, &ks.queries);
    // a submission and a comment with only their key columns set
    exec(
        session,
        &format!(
            "INSERT INTO {}.samples_list (group, year, bucket, uploaded, id, sha256) \
             VALUES ('g', 2026, 1, '2026-01-01T00:00:00Z', {}, 'nulls')",
            ks.name,
            Uuid::from_u128(1)
        ),
    )
    .await?;
    exec(
        session,
        &format!(
            "INSERT INTO {}.comments (group, sha256, uploaded, id) \
             VALUES ('g', 'nulls', '2026-01-01T00:00:00Z', {})",
            ks.name,
            Uuid::from_u128(2)
        ),
    )
    .await?;
    let groups = vec!["g".to_owned()];
    // the submission's text columns are required, so its row fails to deserialize
    eventually(|| async {
        let rows = queries
            .samples
            .get(session, "nulls", &groups)
            .await
            .map_err(fail)?;
        Ok(rows.len() == 1)
    })
    .await?;
    let rows = queries
        .samples
        .get(session, "nulls", &groups)
        .await
        .map_err(fail)?;
    assert!(rows[0].is_err(), "{:?}", rows[0]);
    let basic = queries
        .samples
        .get_basic_submission_info(session, "nulls")
        .await
        .map_err(fail)?;
    assert!(basic[0].is_err(), "{:?}", basic[0]);
    // the sha256 is a key column of the view, so the auth projection still reads it
    let authed = collect(
        queries
            .samples
            .auth(session, &["nulls".to_owned()], &groups)
            .await
            .map_err(fail)?,
    )?;
    assert_eq!(authed.len(), 1);
    // the comment's author, text, and files are required too
    let comments = queries
        .comments
        .get(session, &groups, "nulls")
        .await
        .map_err(fail)?;
    assert!(comments[0].is_err(), "{:?}", comments[0]);
    // the replaced statements, read into the field types their row structs had in order, fail
    // on the same rows
    let legacy = legacy_statements(&ks.name);
    let old = legacy_rows::<
        (
            String,
            String,
            String,
            Uuid,
            Option<String>,
            Option<String>,
            String,
            String,
            Option<String>,
            chrono::DateTime<Utc>,
        ),
        _,
    >(session, &legacy["samples.get"], ("nulls", &groups))
    .await?;
    assert!(old[0].is_err(), "{:?}", old[0]);
    let old = legacy_rows::<(String, String, Uuid), _>(
        session,
        &legacy["samples.get_basic_submission_info"],
        ("nulls",),
    )
    .await?;
    assert!(old[0].is_err(), "{:?}", old[0]);
    let old = legacy_rows::<
        (
            String,
            String,
            chrono::DateTime<Utc>,
            Uuid,
            String,
            String,
            String,
        ),
        _,
    >(session, &legacy["comments.get"], (&groups, "nulls"))
    .await?;
    assert!(old[0].is_err(), "{:?}", old[0]);
    ks.drop().await
}

/// Run a replaced statement and read each row into a tuple of the types its row struct read
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `cql` - The replaced statement
/// * `values` - The values to bind
async fn legacy_rows<T, V>(
    session: &Session,
    cql: &str,
    values: V,
) -> Result<Vec<Result<T, scylla::deserialize::DeserializationError>>, Error>
where
    T: for<'f, 'm> scylla::deserialize::row::DeserializeRow<'f, 'm>,
    V: scylla::serialize::row::SerializeRow,
{
    let rows = session
        .query_unpaged(cql, values)
        .await
        .map_err(fail)?
        .into_rows_result()
        .map_err(fail)?;
    Ok(rows.rows::<T>().map_err(fail)?.collect())
}

/// A delete by key leaves its siblings, a partition delete removes only its partition, and
/// both reach the table's view
#[tokio::test]
// the test walks every statement of its tables in one keyspace
#[allow(clippy::too_many_lines)]
async fn deletes_leave_siblings_and_reach_views() -> Result<(), Error> {
    let ks = Keyspace::new("deletes").await?;
    let (session, queries) = (&ks.session, &ks.queries);
    let sha256 = "f".repeat(64);
    // two submissions in one partition, and two comments in one partition
    let (first, sibling) = (submission("g", &sha256, 1), submission("g", &sha256, 2));
    queries
        .samples_list
        .insert(session, &first.as_ref())
        .await
        .map_err(fail)?;
    queries
        .samples_list
        .insert(session, &sibling.as_ref())
        .await
        .map_err(fail)?;
    let (note, other_note) = (comment("g", &sha256, 3), comment("g", &sha256, 4));
    queries
        .comments
        .insert(session, &note.as_ref())
        .await
        .map_err(fail)?;
    queries
        .comments
        .insert(session, &other_note.as_ref())
        .await
        .map_err(fail)?;
    // delete one of each by key
    queries
        .samples_list
        .delete_multiple_groups(
            session,
            &["g".to_owned()],
            first.year,
            first.bucket,
            first.uploaded,
            first.id,
        )
        .await
        .map_err(fail)?;
    queries
        .comments
        .delete(session, "g", &sha256, note.uploaded, note.id)
        .await
        .map_err(fail)?;
    // the siblings stay in the tables, and the deletes reach the views
    let groups = vec!["g".to_owned()];
    eventually(|| async {
        let rows = collect(
            queries
                .samples
                .get(session, &sha256, &groups)
                .await
                .map_err(fail)?,
        )?;
        Ok(rows.len() == 1 && rows[0].id == sibling.id)
    })
    .await?;
    let comments = collect(
        queries
            .comments
            .get(session, &groups, &sha256)
            .await
            .map_err(fail)?,
    )?;
    assert_eq!(
        comments.iter().map(|row| row.id).collect::<Vec<_>>(),
        [other_note.id]
    );
    eventually(|| async {
        let found = collect(
            queries
                .comments_by_id
                .exists(session, &[note.id, other_note.id])
                .await
                .map_err(fail)?,
        )?;
        Ok(
            found.iter().map(|row| row.id).collect::<HashSet<_>>()
                == HashSet::from([other_note.id]),
        )
    })
    .await?;
    // a partition delete of one S3 id leaves the other id
    for (id, key) in [(5, "five"), (6, "six")] {
        queries
            .s3_ids
            .insert(
                session,
                &S3IdsTableRef {
                    object_type: &S3Objects::File,
                    id: Uuid::from_u128(id),
                    sha256: key,
                },
            )
            .await
            .map_err(fail)?;
    }
    queries
        .s3_ids
        .delete(session, &S3Objects::File, Uuid::from_u128(5))
        .await
        .map_err(fail)?;
    let rows = keyed(ks.rows(&<S3IdsTable as ScyllaObject>::DEF).await?, 2);
    assert_eq!(rows.keys().collect::<Vec<_>>(), ["six"]);
    eventually(|| async {
        let gone = !queries
            .s3_sha256s
            .object_exists(session, "five", &S3Objects::File)
            .await
            .map_err(fail)?;
        let kept = queries
            .s3_sha256s
            .object_exists(session, "six", &S3Objects::File)
            .await
            .map_err(fail)?;
        Ok(gone && kept)
    })
    .await?;
    ks.drop().await
}

/// Each existence and authorization statement finds nothing for an absent row or for a row
/// only in a group the request didn't name
#[tokio::test]
// the test walks every statement of its tables in one keyspace
#[allow(clippy::too_many_lines)]
async fn exists_and_auth_negatives() -> Result<(), Error> {
    let ks = Keyspace::new("negatives").await?;
    let (session, queries) = (&ks.session, &ks.queries);
    let sha256 = "9".repeat(64);
    // one submission, comment, and S3 id, all in group `seen`
    queries
        .samples_list
        .insert(session, &submission("seen", &sha256, 1).as_ref())
        .await
        .map_err(fail)?;
    queries
        .comments
        .insert(session, &comment("seen", &sha256, 2).as_ref())
        .await
        .map_err(fail)?;
    queries
        .s3_ids
        .insert(
            session,
            &S3IdsTableRef {
                object_type: &S3Objects::File,
                id: Uuid::from_u128(3),
                sha256: &sha256,
            },
        )
        .await
        .map_err(fail)?;
    let seen = vec!["seen".to_owned()];
    eventually(|| async {
        let rows = queries
            .samples
            .get(session, &sha256, &seen)
            .await
            .map_err(fail)?;
        Ok(rows.len() == 1)
    })
    .await?;
    // an absent sha256 and a group the row isn't in both authorize nothing
    let unseen = vec!["unseen".to_owned()];
    let absent = vec!["0".repeat(64)];
    let present = vec![sha256.clone()];
    assert!(
        queries
            .samples
            .auth(session, &absent, &seen)
            .await
            .map_err(fail)?
            .is_empty()
    );
    assert!(
        queries
            .samples
            .auth(session, &present, &unseen)
            .await
            .map_err(fail)?
            .is_empty()
    );
    assert!(
        !queries
            .samples
            .auth_any(session, &present, &unseen)
            .await
            .map_err(fail)?
    );
    assert!(
        queries
            .samples
            .get(session, &sha256, &unseen)
            .await
            .map_err(fail)?
            .is_empty()
    );
    assert!(
        queries
            .comments
            .get(session, &unseen, &sha256)
            .await
            .map_err(fail)?
            .is_empty()
    );
    // an absent id or key, or the other object type, doesn't exist
    assert!(
        !queries
            .s3_ids
            .id_exists(session, &S3Objects::File, Uuid::from_u128(4))
            .await
            .map_err(fail)?
    );
    assert!(
        !queries
            .s3_ids
            .id_exists(session, &S3Objects::Repo, Uuid::from_u128(3))
            .await
            .map_err(fail)?
    );
    eventually(|| async {
        let found = queries
            .s3_sha256s
            .object_exists(session, &sha256, &S3Objects::File)
            .await
            .map_err(fail)?;
        Ok(found)
    })
    .await?;
    assert!(
        !queries
            .s3_sha256s
            .object_exists(session, &sha256, &S3Objects::Repo)
            .await
            .map_err(fail)?
    );
    assert!(
        queries
            .s3_sha256s
            .get(session, "absent", &S3Objects::File)
            .await
            .map_err(fail)?
            .is_none()
    );
    assert!(
        queries
            .comments_by_id
            .exists(session, &[Uuid::from_u128(99)])
            .await
            .map_err(fail)?
            .is_empty()
    );
    ks.drop().await
}

/// The clone scheme every repo fixture uses, holding credentials
fn scheme() -> RepoScheme {
    RepoScheme::HttpsAuthed {
        username: "farmer".to_owned(),
        password: "hunter2".to_owned(),
    }
}

/// A repo submission with a distinct value in every column it inserts
///
/// # Arguments
///
/// * `group` - The submission's group
/// * `url` - The repo's url
/// * `id` - The submission's id
/// * `scheme` - The clone scheme
/// * `checkout` - The default checkout, if any
fn repo<'a>(
    group: &'a str,
    url: &'a str,
    id: u128,
    scheme: &'a RepoScheme,
    checkout: Option<&'a RepoCheckout>,
) -> ReposListInsert<'a> {
    ReposListInsert {
        group,
        year: 2026,
        bucket: 17,
        uploaded: Utc.with_ymd_and_hms(2026, 5, 6, 7, 8, 9).unwrap()
            + chrono::Duration::milliseconds(i64::try_from(id % 1000).unwrap()),
        id: Uuid::from_u128(id),
        url,
        provider: "github.com",
        user: "curl",
        name: "curl",
        creator: "europa",
        scheme,
        default_checkout: checkout,
    }
}

/// A commitish's details with a distinct value in every column
///
/// # Arguments
///
/// * `group` - The commitish's group
/// * `kind` - The commitish's kind
/// * `key` - The commitish's key
fn commitish<'a>(
    group: &'a str,
    kind: &'a CommitishKinds,
    key: &'a str,
) -> CommitishesTableRef<'a> {
    CommitishesTableRef {
        kind,
        group,
        repo: "github.com/curl/curl",
        key,
        timestamp: Utc.with_ymd_and_hms(2026, 7, 8, 9, 10, 11).unwrap(),
        data: "{\"author\":\"daniel\"}",
        repo_data: "blob-sha256",
    }
}

/// Every repos, repo data, and commitish statement runs through its generated method and binds
/// each value to its own column
#[tokio::test]
// the test walks every statement of its tables in one keyspace
#[allow(clippy::too_many_lines)]
async fn repos_statements_round_trip() -> Result<(), Error> {
    let ks = Keyspace::new("repos_rt").await?;
    let (session, queries) = (&ks.session, &ks.queries);
    let (scheme, checkout) = (scheme(), RepoCheckout::Branch("main".to_owned()));
    let url = "github.com/curl/curl";
    // one submission in two groups
    let first = repo("ceres", url, 1, &scheme, Some(&checkout));
    queries
        .repos_list
        .insert(session, &first)
        .await
        .map_err(fail)?;
    queries
        .repos_list
        .insert(
            session,
            &ReposListInsert {
                group: "vesta",
                ..first
            },
        )
        .await
        .map_err(fail)?;
    let groups = vec!["ceres".to_owned(), "vesta".to_owned()];
    eventually(|| async {
        let rows = queries
            .repos
            .get(session, url, &groups)
            .await
            .map_err(fail)?;
        Ok(rows.len() == 2)
    })
    .await?;
    let rows = collect(
        queries
            .repos
            .get(session, url, &groups)
            .await
            .map_err(fail)?,
    )?;
    let row = rows
        .iter()
        .find(|row| row.group == "ceres")
        .expect("ceres' row");
    assert_eq!(row.provider, "github.com");
    assert_eq!(row.user, "curl");
    assert_eq!(row.name, "curl");
    assert_eq!(row.url, url);
    assert_eq!(row.id, first.id);
    assert_eq!(row.creator, "europa");
    assert_eq!(row.uploaded, first.uploaded);
    assert_eq!(row.scheme, serde_json::to_string(&scheme).map_err(fail)?);
    assert_eq!(
        row.default_checkout,
        Some(serde_json::to_string(&checkout).map_err(fail)?)
    );
    assert_eq!(row.earliest, None);
    // each update sets its one column in one group
    let tag = RepoCheckout::Tag("v1".to_owned());
    let earliest = Utc.with_ymd_and_hms(2001, 2, 3, 4, 5, 6).unwrap();
    queries
        .repos_list
        .update_default_checkout(
            session,
            Some(&tag),
            "ceres",
            first.year,
            first.bucket,
            first.uploaded,
            first.id,
        )
        .await
        .map_err(fail)?;
    queries
        .repos_list
        .update_earliest(
            session,
            Some(earliest),
            "ceres",
            first.year,
            first.bucket,
            first.uploaded,
            first.id,
        )
        .await
        .map_err(fail)?;
    let tagged = Some(serde_json::to_string(&tag).map_err(fail)?);
    eventually(|| async {
        let rows = collect(
            queries
                .repos
                .get(session, url, &groups)
                .await
                .map_err(fail)?,
        )?;
        let ceres = rows.iter().find(|row| row.group == "ceres");
        Ok(ceres
            .is_some_and(|row| row.default_checkout == tagged && row.earliest == Some(earliest)))
    })
    .await?;
    let rows = collect(
        queries
            .repos
            .get(session, url, &groups)
            .await
            .map_err(fail)?,
    )?;
    let vesta = rows
        .iter()
        .find(|row| row.group == "vesta")
        .expect("vesta's row");
    assert_eq!(vesta.earliest, None);
    assert_ne!(vesta.default_checkout, tagged);
    // get_many and auth find the repo by url
    let urls = vec![url.to_owned()];
    assert_eq!(
        queries
            .repos
            .get_many(session, &urls, &groups)
            .await
            .map_err(fail)?
            .len(),
        2
    );
    let authed = collect(
        queries
            .repos
            .auth(session, &urls, &groups)
            .await
            .map_err(fail)?,
    )?;
    assert_eq!(
        authed
            .iter()
            .map(|row| row.url.as_str())
            .collect::<Vec<_>>(),
        [url]
    );
    assert!(
        queries
            .repos
            .auth_any(session, &urls, &groups)
            .await
            .map_err(fail)?
    );
    // the repo's data blobs
    for hash in ["blob1", "blob2"] {
        queries
            .repo_data
            .insert_data(session, &RepoDataTableRef { repo: url, hash })
            .await
            .map_err(fail)?;
    }
    let hashes = collect(
        queries
            .repo_data
            .get_data(session, url)
            .await
            .map_err(fail)?,
    )?;
    assert_eq!(
        hashes
            .iter()
            .map(|row| row.hash.as_str())
            .collect::<Vec<_>>(),
        ["blob1", "blob2"]
    );
    assert!(
        queries
            .repo_data
            .data_exists(session, url, "blob1")
            .await
            .map_err(fail)?
    );
    queries
        .repo_data
        .delete_data(session, url, "blob1")
        .await
        .map_err(fail)?;
    assert!(
        !queries
            .repo_data
            .data_exists(session, url, "blob1")
            .await
            .map_err(fail)?
    );
    assert!(
        queries
            .repo_data
            .data_exists(session, url, "blob2")
            .await
            .map_err(fail)?
    );
    // a commit and a branch sharing a key, each in one group
    let (commit, branch) = (CommitishKinds::Commit, CommitishKinds::Branch);
    queries
        .commitishes
        .insert(session, &commitish("ceres", &commit, "main"))
        .await
        .map_err(fail)?;
    queries
        .commitishes
        .insert(
            session,
            &CommitishesTableRef {
                repo_data: "other-blob",
                ..commitish("vesta", &branch, "main")
            },
        )
        .await
        .map_err(fail)?;
    let data = queries
        .commitishes
        .get_data(session, &commit, &groups, url, "main")
        .await
        .map_err(fail)?;
    assert_eq!(
        data.map(|row| row.data),
        Some("{\"author\":\"daniel\"}".to_owned())
    );
    assert!(
        queries
            .commitishes
            .exists(session, &branch, &groups, url, "main")
            .await
            .map_err(fail)?
    );
    let found = collect(
        queries
            .commitishes
            .get_repo_data(session, &[commit, branch], &groups, url, "main")
            .await
            .map_err(fail)?,
    )?;
    let found = found
        .into_iter()
        .map(|row| (row.kind, row.repo_data))
        .collect::<Vec<_>>();
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(found.contains(&(CommitishKinds::Commit, "blob-sha256".to_owned())));
    assert!(found.contains(&(CommitishKinds::Branch, "other-blob".to_owned())));
    // a commitish list row reads back with every column as written
    let listed = CommitishListTable {
        kind: CommitishKinds::Tag,
        group: "ceres".to_owned(),
        year: 2026,
        bucket: 9,
        repo: url.to_owned(),
        timestamp: Utc.with_ymd_and_hms(2026, 9, 9, 9, 9, 9).unwrap(),
        key: "v1".to_owned(),
        repo_data: "blob2".to_owned(),
    };
    queries
        .commitish_list
        .insert_list(session, &listed.as_ref())
        .await
        .map_err(fail)?;
    let rows = ks.rows(&<CommitishListTable as ScyllaObject>::DEF).await?;
    assert_eq!(
        rows[0].columns,
        vec![
            Some(CqlValue::Text("Tag".to_owned())),
            Some(CqlValue::Text("ceres".to_owned())),
            Some(CqlValue::Int(2026)),
            Some(CqlValue::Int(9)),
            Some(CqlValue::Text(url.to_owned())),
            Some(CqlValue::Timestamp(scylla::value::CqlTimestamp(
                listed.timestamp.timestamp_millis()
            ))),
            Some(CqlValue::Text("v1".to_owned())),
            Some(CqlValue::Text("blob2".to_owned())),
        ]
    );
    ks.drop().await
}

/// The repos statements' `IN` lists longer than one chunk return the same rows as a single
/// statement would
#[tokio::test]
async fn repos_long_in_lists_return_every_row() -> Result<(), Error> {
    let ks = Keyspace::new("repos_chunks").await?;
    let (session, queries) = (&ks.session, &ks.queries);
    let scheme = scheme();
    let url = "github.com/rust-lang/rust";
    // 101 groups, with the repo and a commit in the first and the last
    let groups = (0..101).map(|i| format!("group{i:03}")).collect::<Vec<_>>();
    let commit = CommitishKinds::Commit;
    for (i, group) in [(1, &groups[0]), (2, &groups[100])] {
        queries
            .repos_list
            .insert(session, &repo(group, url, i, &scheme, None))
            .await
            .map_err(fail)?;
        queries
            .commitishes
            .insert(session, &commitish(group, &commit, "abc"))
            .await
            .map_err(fail)?;
    }
    eventually(|| async {
        let rows = queries
            .repos
            .get(session, url, &groups)
            .await
            .map_err(fail)?;
        Ok(rows.len() == 2)
    })
    .await?;
    // 101 urls with this one last
    let mut urls = (0..100)
        .map(|i| format!("github.com/nobody/{i}"))
        .collect::<Vec<_>>();
    urls.push(url.to_owned());
    assert_eq!(
        queries
            .repos
            .get_many(session, &urls, &groups)
            .await
            .map_err(fail)?
            .len(),
        2
    );
    let authed = collect(
        queries
            .repos
            .auth(session, &urls, &groups)
            .await
            .map_err(fail)?,
    )?;
    let authed = authed
        .into_iter()
        .map(|row| row.url)
        .collect::<HashSet<_>>();
    assert_eq!(authed, HashSet::from([url.to_owned()]));
    // a commit only in the last of 101 groups is found in the second chunk
    let mut chunked = vec!["nobody".to_owned()];
    chunked.extend(groups[1..].iter().cloned());
    assert!(
        queries
            .commitishes
            .exists(session, &commit, &chunked, "github.com/curl/curl", "abc")
            .await
            .map_err(fail)?
    );
    // three kinds times 101 groups keeps each execution within Scylla's limit
    let kinds = [
        CommitishKinds::Commit,
        CommitishKinds::Branch,
        CommitishKinds::Tag,
    ];
    let found = queries
        .commitishes
        .get_repo_data(session, &kinds, &groups, "github.com/curl/curl", "abc")
        .await
        .map_err(fail)?;
    assert_eq!(found.len(), 2);
    ks.drop().await
}

/// Repo and commitish rows written by the replaced statements, with the binds the API used,
/// match the rows the generated statements write
#[tokio::test]
// the test writes each table both ways in one keyspace
#[allow(clippy::too_many_lines)]
async fn repos_rows_match_the_replaced_statements() -> Result<(), Error> {
    let ks = Keyspace::new("repos_parity").await?;
    let (session, queries) = (&ks.session, &ks.queries);
    let legacy = legacy_statements(&ks.name);
    let (scheme, checkout) = (scheme(), RepoCheckout::Commit("abc".to_owned()));
    let row = repo("old", "github.com/curl/curl", 1, &scheme, Some(&checkout));
    // a repo submission the way the API wrote it, with its JSON serialized first
    let scheme_raw = serde_json::to_string(&scheme).map_err(fail)?;
    let checkout_raw = Some(serde_json::to_string(&checkout).map_err(fail)?);
    let insert = session
        .prepare(legacy["repos_list.insert"].clone())
        .await
        .map_err(fail)?;
    for (group, checkout) in [("old", &checkout_raw), ("old-none", &None)] {
        session
            .execute_unpaged(
                &insert,
                (
                    group,
                    row.year,
                    row.bucket,
                    row.uploaded,
                    row.id,
                    row.url,
                    row.provider,
                    row.user,
                    row.name,
                    row.creator,
                    &scheme_raw,
                    checkout,
                ),
            )
            .await
            .map_err(fail)?;
    }
    queries
        .repos_list
        .insert(
            session,
            &ReposListInsert {
                group: "new",
                ..row
            },
        )
        .await
        .map_err(fail)?;
    queries
        .repos_list
        .insert(
            session,
            &ReposListInsert {
                group: "new-none",
                default_checkout: None,
                ..row
            },
        )
        .await
        .map_err(fail)?;
    // both updates written both ways
    let update = session
        .prepare(legacy["repos_list.update_default_checkout"].clone())
        .await
        .map_err(fail)?;
    session
        .execute_unpaged(
            &update,
            (
                &checkout_raw,
                "old",
                row.year,
                row.bucket,
                row.uploaded,
                row.id,
            ),
        )
        .await
        .map_err(fail)?;
    queries
        .repos_list
        .update_default_checkout(
            session,
            Some(&checkout),
            "new",
            row.year,
            row.bucket,
            row.uploaded,
            row.id,
        )
        .await
        .map_err(fail)?;
    let earliest = Utc.with_ymd_and_hms(2000, 1, 2, 3, 4, 5).unwrap();
    let update = session
        .prepare(legacy["repos_list.update_earliest"].clone())
        .await
        .map_err(fail)?;
    session
        .execute_unpaged(
            &update,
            (earliest, "old", row.year, row.bucket, row.uploaded, row.id),
        )
        .await
        .map_err(fail)?;
    queries
        .repos_list
        .update_earliest(
            session,
            Some(earliest),
            "new",
            row.year,
            row.bucket,
            row.uploaded,
            row.id,
        )
        .await
        .map_err(fail)?;
    let rows = keyed(ks.rows(&<ReposListTable as ScyllaObject>::DEF).await?, 0);
    assert_eq!(rows["old"], rows["new"]);
    assert_eq!(rows["old-none"], rows["new-none"]);
    // a commitish and its list row
    let commit = CommitishKinds::Commit;
    let details = commitish("old", &commit, "abc");
    let insert = session
        .prepare(legacy["commitishes.insert"].clone())
        .await
        .map_err(fail)?;
    session
        .execute_unpaged(
            &insert,
            (
                commit,
                "old",
                details.repo,
                details.key,
                details.timestamp,
                details.data,
                details.repo_data,
            ),
        )
        .await
        .map_err(fail)?;
    queries
        .commitishes
        .insert(
            session,
            &CommitishesTableRef {
                group: "new",
                ..details
            },
        )
        .await
        .map_err(fail)?;
    let rows = keyed(ks.rows(&<CommitishesTable as ScyllaObject>::DEF).await?, 1);
    assert_eq!(rows["old"], rows["new"]);
    let insert = session
        .prepare(legacy["commitish_list.insert_list"].clone())
        .await
        .map_err(fail)?;
    session
        .execute_unpaged(
            &insert,
            (
                commit,
                "old",
                2026,
                4,
                details.repo,
                details.timestamp,
                details.key,
                details.repo_data,
            ),
        )
        .await
        .map_err(fail)?;
    let listed = CommitishListTableRef {
        kind: &commit,
        group: "new",
        year: 2026,
        bucket: 4,
        repo: details.repo,
        timestamp: details.timestamp,
        key: details.key,
        repo_data: details.repo_data,
    };
    queries
        .commitish_list
        .insert_list(session, &listed)
        .await
        .map_err(fail)?;
    let rows = keyed(
        ks.rows(&<CommitishListTable as ScyllaObject>::DEF).await?,
        1,
    );
    assert_eq!(rows["old"], rows["new"]);
    // a repo data blob
    let insert = session
        .prepare(legacy["repo_data.insert_data"].clone())
        .await
        .map_err(fail)?;
    session
        .execute_unpaged(&insert, ("old", "blob"))
        .await
        .map_err(fail)?;
    queries
        .repo_data
        .insert_data(
            session,
            &RepoDataTableRef {
                repo: "new",
                hash: "blob",
            },
        )
        .await
        .map_err(fail)?;
    let rows = keyed(ks.rows(&<RepoDataTable as ScyllaObject>::DEF).await?, 0);
    assert_eq!(rows["old"], rows["new"]);
    ks.drop().await
}

/// A subset insert over an existing row leaves its omitted column in place, and an update
/// changes only its column without adding a row
#[tokio::test]
async fn subset_inserts_and_updates_change_only_their_columns() -> Result<(), Error> {
    let ks = Keyspace::new("repos_subset").await?;
    let (session, queries) = (&ks.session, &ks.queries);
    let scheme = scheme();
    let row = repo("ceres", "github.com/curl/curl", 1, &scheme, None);
    // a full row with its earliest set
    queries
        .repos_list
        .insert(session, &row)
        .await
        .map_err(fail)?;
    let earliest = Utc.with_ymd_and_hms(2003, 4, 5, 6, 7, 8).unwrap();
    queries
        .repos_list
        .update_earliest(
            session,
            Some(earliest),
            "ceres",
            row.year,
            row.bucket,
            row.uploaded,
            row.id,
        )
        .await
        .map_err(fail)?;
    // the subset insert rewrites the provider but not the earliest
    queries
        .repos_list
        .insert(
            session,
            &ReposListInsert {
                provider: "gitlab.com",
                ..row
            },
        )
        .await
        .map_err(fail)?;
    let rows = ks.rows(&<ReposListTable as ScyllaObject>::DEF).await?;
    assert_eq!(rows.len(), 1);
    let columns = &rows[0].columns;
    assert_eq!(columns[6], Some(CqlValue::Text("gitlab.com".to_owned())));
    assert_eq!(
        columns[12],
        Some(CqlValue::Timestamp(scylla::value::CqlTimestamp(
            earliest.timestamp_millis()
        )))
    );
    // the default checkout update changes only that column
    let checkout = RepoCheckout::Tag("v2".to_owned());
    queries
        .repos_list
        .update_default_checkout(
            session,
            Some(&checkout),
            "ceres",
            row.year,
            row.bucket,
            row.uploaded,
            row.id,
        )
        .await
        .map_err(fail)?;
    let after = ks.rows(&<ReposListTable as ScyllaObject>::DEF).await?;
    assert_eq!(after.len(), 1);
    for (index, (before, now)) in columns.iter().zip(&after[0].columns).enumerate() {
        if index == 11 {
            assert_eq!(
                now,
                &Some(CqlValue::Text(
                    serde_json::to_string(&checkout).map_err(fail)?
                ))
            );
        } else {
            assert_eq!(before, now, "column {index}");
        }
    }
    ks.drop().await
}

/// Reading repo and commitish rows whose nullable non-key columns are all null keeps each
/// projection's outcome
#[tokio::test]
async fn repos_projections_over_null_columns() -> Result<(), Error> {
    let ks = Keyspace::new("repos_nulls").await?;
    let (session, queries) = (&ks.session, &ks.queries);
    let legacy = legacy_statements(&ks.name);
    // a repo submission with only its key and url, and a commitish with only its key
    exec(
        session,
        &format!(
            "INSERT INTO {}.repos_list (group, year, bucket, uploaded, id, url) \
             VALUES ('g', 2026, 1, '2026-01-01T00:00:00Z', {}, 'nulls')",
            ks.name,
            Uuid::from_u128(1)
        ),
    )
    .await?;
    exec(
        session,
        &format!(
            "INSERT INTO {}.commitishes (kind, group, repo, key) VALUES ('Commit', 'g', 'nulls', 'k')",
            ks.name
        ),
    )
    .await?;
    let groups = vec!["g".to_owned()];
    // the submission's text columns are required, so its row fails to deserialize
    eventually(|| async {
        let rows = queries
            .repos
            .get(session, "nulls", &groups)
            .await
            .map_err(fail)?;
        Ok(rows.len() == 1)
    })
    .await?;
    let rows = queries
        .repos
        .get(session, "nulls", &groups)
        .await
        .map_err(fail)?;
    assert!(rows[0].is_err(), "{:?}", rows[0].as_ref().map(|_| ()));
    let old = legacy_rows::<
        (
            String,
            String,
            String,
            String,
            String,
            Uuid,
            String,
            chrono::DateTime<Utc>,
            String,
            Option<String>,
            Option<chrono::DateTime<Utc>>,
        ),
        _,
    >(session, &legacy["repos.get"], ("nulls", &groups))
    .await?;
    assert!(old[0].is_err());
    // the commitish's data and repo data are required too
    let commit = CommitishKinds::Commit;
    let data = queries
        .commitishes
        .get_data(session, &commit, &groups, "nulls", "k")
        .await;
    assert!(data.is_err());
    let old = legacy_rows::<(String,), _>(
        session,
        &legacy["commitishes.get_data"],
        (commit, &groups, "nulls", "k"),
    )
    .await?;
    assert!(old[0].is_err());
    let found = queries
        .commitishes
        .get_repo_data(session, &[commit], &groups, "nulls", "k")
        .await
        .map_err(fail)?;
    assert!(found[0].is_err());
    let old = legacy_rows::<(CommitishKinds, String), _>(
        session,
        &legacy["commitishes.get_repo_data"],
        (vec![commit], &groups, "nulls", "k"),
    )
    .await?;
    assert!(old[0].is_err());
    // the repo's url is the view's key, so its auth row still reads
    let authed = collect(
        queries
            .repos
            .auth(session, &["nulls".to_owned()], &groups)
            .await
            .map_err(fail)?,
    )?;
    assert_eq!(authed.len(), 1);
    ks.drop().await
}

/// A repo data delete by key leaves the repo's other blobs, and each existence and
/// authorization statement finds nothing for an absent row or an unrequested group
#[tokio::test]
// the test walks every existence and authorization statement of its tables in one keyspace
#[allow(clippy::too_many_lines)]
async fn repos_deletes_and_negatives() -> Result<(), Error> {
    let ks = Keyspace::new("repos_negatives").await?;
    let (session, queries) = (&ks.session, &ks.queries);
    let scheme = scheme();
    let url = "github.com/curl/curl";
    // two blobs of one repo, and a submission and a commit in group `seen`
    for hash in ["a", "b"] {
        queries
            .repo_data
            .insert_data(session, &RepoDataTableRef { repo: url, hash })
            .await
            .map_err(fail)?;
    }
    queries
        .repo_data
        .delete_data(session, url, "a")
        .await
        .map_err(fail)?;
    let hashes = collect(
        queries
            .repo_data
            .get_data(session, url)
            .await
            .map_err(fail)?,
    )?;
    assert_eq!(
        hashes
            .iter()
            .map(|row| row.hash.as_str())
            .collect::<Vec<_>>(),
        ["b"]
    );
    queries
        .repos_list
        .insert(session, &repo("seen", url, 1, &scheme, None))
        .await
        .map_err(fail)?;
    let commit = CommitishKinds::Commit;
    queries
        .commitishes
        .insert(session, &commitish("seen", &commit, "abc"))
        .await
        .map_err(fail)?;
    let (seen, unseen) = (vec!["seen".to_owned()], vec!["unseen".to_owned()]);
    eventually(|| async {
        let rows = queries.repos.get(session, url, &seen).await.map_err(fail)?;
        Ok(rows.len() == 1)
    })
    .await?;
    // nothing authorizes an absent url or a group the repo isn't in
    let (present, absent) = (
        vec![url.to_owned()],
        vec!["github.com/none/none".to_owned()],
    );
    assert!(
        queries
            .repos
            .auth(session, &absent, &seen)
            .await
            .map_err(fail)?
            .is_empty()
    );
    assert!(
        queries
            .repos
            .auth(session, &present, &unseen)
            .await
            .map_err(fail)?
            .is_empty()
    );
    assert!(
        queries
            .repos
            .get(session, url, &unseen)
            .await
            .map_err(fail)?
            .is_empty()
    );
    assert!(
        !queries
            .repo_data
            .data_exists(session, url, "a")
            .await
            .map_err(fail)?
    );
    assert!(
        !queries
            .repo_data
            .data_exists(session, "github.com/none/none", "b")
            .await
            .map_err(fail)?
    );
    // a commit doesn't exist in another group, under another kind, or by another key
    assert!(
        !queries
            .commitishes
            .exists(session, &commit, &unseen, url, "abc")
            .await
            .map_err(fail)?
    );
    assert!(
        !queries
            .commitishes
            .exists(session, &CommitishKinds::Tag, &seen, url, "abc")
            .await
            .map_err(fail)?
    );
    assert!(
        queries
            .commitishes
            .get_data(session, &commit, &seen, url, "zzz")
            .await
            .map_err(fail)?
            .is_none()
    );
    assert!(
        queries
            .commitishes
            .get_data(session, &commit, &unseen, url, "abc")
            .await
            .map_err(fail)?
            .is_none()
    );
    ks.drop().await
}

/// The files and entity counts every result fixture uses
fn result_parts() -> (HashSet<String>, HashMap<EntityKinds, usize>) {
    (
        HashSet::from(["report.txt".to_owned(), "graph.png".to_owned()]),
        HashMap::from([
            (EntityKinds::Other, 3),
            (EntityKinds::WindowsProcessTree, 7),
        ]),
    )
}

/// A result with a distinct value in every column it inserts
///
/// # Arguments
///
/// * `id` - The result's id
/// * `version` - The tool version
/// * `files` - The result's files
/// * `entities` - The result's entity counts
fn result<'a>(
    id: u128,
    version: &'a ImageVersion,
    files: &'a HashSet<String>,
    entities: &'a HashMap<EntityKinds, usize>,
) -> ResultsInsert<'a> {
    ResultsInsert {
        id: Uuid::from_u128(id),
        uploaded: Utc.with_ymd_and_hms(2026, 8, 9, 10, 11, 12).unwrap(),
        tool: Some("strings"),
        tool_version: Some(version),
        cmd: Some("strings -a"),
        result: Some("{\"found\":1}"),
        files: Some(files),
        entities: Some(entities),
        display_type: Some(&OutputDisplayType::Json),
    }
}

/// A result's stream row with a distinct value in every column
///
/// # Arguments
///
/// * `group` - The row's group
/// * `id` - The result's id
/// * `tool` - The tool that made the result
/// * `version` - The tool version
fn stream_row<'a>(
    group: &'a str,
    id: u128,
    tool: &'a str,
    version: &'a ImageVersion,
) -> ResultsStreamTableRef<'a> {
    ResultsStreamTableRef {
        kind: &OutputKind::Files,
        group,
        year: 2026,
        bucket: 5,
        uploaded: Utc.with_ymd_and_hms(2026, 8, 9, 10, 11, 12).unwrap()
            + chrono::Duration::milliseconds(i64::try_from(id % 1000).unwrap()),
        id: Uuid::from_u128(id),
        key: "f00d",
        tool: Some(tool),
        tool_version: Some(version),
        display_type: Some(&OutputDisplayType::Table),
        cmd: Some("run --fast"),
    }
}

/// A tag with a distinct value in every column
///
/// # Arguments
///
/// * `group` - The tag's group
/// * `key` - The tag's key
/// * `value` - The tag's value
fn tag<'a>(group: &'a str, key: &'a str, value: &'a str) -> TagsTableRef<'a> {
    TagsTableRef {
        tag_type: &TagType::Files,
        group,
        year: 2026,
        bucket: 12,
        key,
        value,
        uploaded: Utc.with_ymd_and_hms(2026, 6, 7, 8, 9, 10).unwrap(),
        item: "f00d",
        key_lower: "lowered-key",
        value_lower: "lowered-value",
    }
}

/// Every results statement runs through its generated method and binds each value to its own
/// column
#[tokio::test]
// the test walks every statement of its tables in one keyspace
#[allow(clippy::too_many_lines)]
async fn results_statements_round_trip() -> Result<(), Error> {
    let ks = Keyspace::new("results_rt").await?;
    let (session, queries) = (&ks.session, &ks.queries);
    let version = ImageVersion::Custom("1.2-custom".to_owned());
    let (files, entities) = result_parts();
    // one result, read back with every column as written
    let written = result(1, &version, &files, &entities);
    queries
        .results
        .insert(session, &written)
        .await
        .map_err(fail)?;
    let rows = collect(
        queries
            .results
            .get(session, &[written.id])
            .await
            .map_err(fail)?,
    )?;
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.id, written.id);
    assert_eq!(row.tool, "strings");
    assert_eq!(row.tool_version, Some(version.clone()));
    assert_eq!(row.cmd.as_deref(), Some("strings -a"));
    assert_eq!(row.uploaded, written.uploaded);
    assert_eq!(row.result, "{\"found\":1}");
    let read_files = row
        .files
        .clone()
        .unwrap_or_default()
        .into_iter()
        .collect::<HashSet<_>>();
    assert_eq!(read_files, files);
    let read_entities = row.entities.clone().unwrap_or_default();
    assert_eq!(read_entities.get(&EntityKinds::Other), Some(&3));
    assert_eq!(
        read_entities.get(&EntityKinds::WindowsProcessTree),
        Some(&7)
    );
    assert_eq!(row.display_type, OutputDisplayType::Json);
    assert_eq!(row.children, None);
    let uploaded = collect(
        queries
            .results
            .get_uploaded_by_id(session, &[written.id])
            .await
            .map_err(fail)?,
    )?;
    assert_eq!(
        uploaded
            .iter()
            .map(|row| (row.id, row.uploaded))
            .collect::<Vec<_>>(),
        [(written.id, written.uploaded)]
    );
    // the map entry update adds a child without touching the other columns
    let child = Uuid::from_u128(77);
    queries
        .results
        .update_children(session, "child-sha256", child, written.id, written.uploaded)
        .await
        .map_err(fail)?;
    let rows = collect(
        queries
            .results
            .get(session, &[written.id])
            .await
            .map_err(fail)?,
    )?;
    assert_eq!(
        rows[0].children,
        Some(HashMap::from([("child-sha256".to_owned(), child)]))
    );
    assert_eq!(rows[0].cmd.as_deref(), Some("strings -a"));
    // stream rows in two groups, for two tools
    queries
        .results_stream
        .insert_stream(session, &stream_row("ceres", 1, "strings", &version))
        .await
        .map_err(fail)?;
    queries
        .results_stream
        .insert_stream(session, &stream_row("vesta", 1, "strings", &version))
        .await
        .map_err(fail)?;
    queries
        .results_stream
        .insert_stream(session, &stream_row("ceres", 2, "yara", &version))
        .await
        .map_err(fail)?;
    let groups = vec!["ceres".to_owned(), "vesta".to_owned()];
    let kind = OutputKind::Files;
    eventually(|| async {
        let rows = queries
            .results_auth
            .get_with_key(session, "f00d", &kind, &groups)
            .await
            .map_err(fail)?;
        Ok(rows.len() == 3)
    })
    .await?;
    let ids = collect(
        queries
            .results_auth
            .get_id(session, "f00d", &kind, &groups, "yara")
            .await
            .map_err(fail)?,
    )?;
    assert_eq!(
        ids.iter().map(|row| row.id).collect::<Vec<_>>(),
        [Uuid::from_u128(2)]
    );
    let rows = collect(
        queries
            .results_auth
            .get_with_key(session, "f00d", &kind, &groups)
            .await
            .map_err(fail)?,
    )?;
    let yara = rows
        .iter()
        .find(|row| row.tool == "yara")
        .expect("yara's row");
    assert_eq!(yara.id, Uuid::from_u128(2));
    assert_eq!(yara.display_type, OutputDisplayType::Table);
    assert_eq!(yara.cmd.as_deref(), Some("run --fast"));
    assert_eq!(yara.group, "ceres");
    assert_eq!(
        yara.uploaded,
        stream_row("ceres", 2, "yara", &version).uploaded
    );
    let tools = vec!["strings".to_owned()];
    let rows = queries
        .results_auth
        .get_with_key_and_tool(session, "f00d", &kind, &groups, &tools)
        .await
        .map_err(fail)?;
    assert_eq!(rows.len(), 2);
    eventually(|| async {
        let rows = queries
            .results_ids
            .count(session, "f00d", Uuid::from_u128(1), &kind)
            .await
            .map_err(fail)?;
        Ok(rows.len() == 2)
    })
    .await?;
    // deleting the stream rows and then the result removes them from the views and the table
    for group in ["ceres", "vesta"] {
        let row = stream_row(group, 1, "strings", &version);
        queries
            .results_stream
            .delete_stream(
                session,
                &kind,
                group,
                row.year,
                row.bucket,
                row.uploaded,
                row.id,
            )
            .await
            .map_err(fail)?;
    }
    eventually(|| async {
        let rows = queries
            .results_ids
            .count(session, "f00d", Uuid::from_u128(1), &kind)
            .await
            .map_err(fail)?;
        Ok(rows.is_empty())
    })
    .await?;
    queries
        .results
        .delete(session, written.id)
        .await
        .map_err(fail)?;
    assert!(
        queries
            .results
            .get(session, &[written.id])
            .await
            .map_err(fail)?
            .is_empty()
    );
    ks.drop().await
}

/// Every tags statement runs through its generated method, and a tag delete reaches the view
#[tokio::test]
async fn tags_statements_round_trip() -> Result<(), Error> {
    let ks = Keyspace::new("tags_rt").await?;
    let (session, queries) = (&ks.session, &ks.queries);
    // two tags on one item in one group, and one in another group
    let (first, second) = (tag("ceres", "Format", "PE32"), tag("ceres", "Arch", "x86"));
    queries.tags.insert(session, &first).await.map_err(fail)?;
    queries.tags.insert(session, &second).await.map_err(fail)?;
    queries
        .tags
        .insert(session, &tag("vesta", "Format", "PE32"))
        .await
        .map_err(fail)?;
    let groups = vec!["ceres".to_owned(), "vesta".to_owned()];
    eventually(|| async {
        let rows = queries
            .tags_by_item
            .get(session, &TagType::Files, &groups, "f00d")
            .await
            .map_err(fail)?;
        Ok(rows.len() == 3)
    })
    .await?;
    let rows = collect(
        queries
            .tags_by_item
            .get(session, &TagType::Files, &groups, "f00d")
            .await
            .map_err(fail)?,
    )?;
    let arch = rows
        .iter()
        .find(|row| row.key == "Arch")
        .expect("the arch tag");
    assert_eq!(
        (arch.group.as_str(), arch.item.as_str(), arch.value.as_str()),
        ("ceres", "f00d", "x86")
    );
    let full = collect(
        queries
            .tags_by_item
            .get_rows(session, &TagType::Files, &groups, "f00d")
            .await
            .map_err(fail)?,
    )?;
    let arch = full
        .iter()
        .find(|row| row.key == "Arch")
        .expect("the arch tag's row");
    assert_eq!(arch.year, 2026);
    assert_eq!(arch.bucket, 12);
    assert_eq!(arch.value, "x86");
    assert_eq!(arch.uploaded, second.uploaded);
    assert_eq!(arch.item, "f00d");
    // deleting one tag leaves its sibling in the partition and reaches the view
    queries
        .tags
        .delete(
            session,
            &TagType::Files,
            "ceres",
            2026,
            12,
            "Arch",
            "x86",
            second.uploaded,
            "f00d",
        )
        .await
        .map_err(fail)?;
    eventually(|| async {
        let rows = collect(
            queries
                .tags_by_item
                .get(session, &TagType::Files, &groups, "f00d")
                .await
                .map_err(fail)?,
        )?;
        Ok(rows.len() == 2 && rows.iter().all(|row| row.key == "Format"))
    })
    .await?;
    ks.drop().await
}

/// The results and tags statements' `IN` lists longer than one chunk return every row
#[tokio::test]
async fn results_long_in_lists_return_every_row() -> Result<(), Error> {
    let ks = Keyspace::new("results_chunks").await?;
    let (session, queries) = (&ks.session, &ks.queries);
    let version = ImageVersion::Custom("1".to_owned());
    let (files, entities) = result_parts();
    // a result whose id is last of 101
    queries
        .results
        .insert(session, &result(5, &version, &files, &entities))
        .await
        .map_err(fail)?;
    let mut ids = (100..200).map(Uuid::from_u128).collect::<Vec<_>>();
    ids.push(Uuid::from_u128(5));
    assert_eq!(
        queries
            .results
            .get(session, &ids)
            .await
            .map_err(fail)?
            .len(),
        1
    );
    // stream rows and tags in the first and last of 101 groups
    let groups = (0..101).map(|i| format!("group{i:03}")).collect::<Vec<_>>();
    for (i, group) in [(1, &groups[0]), (2, &groups[100])] {
        queries
            .results_stream
            .insert_stream(session, &stream_row(group, i, "strings", &version))
            .await
            .map_err(fail)?;
        queries
            .tags
            .insert(session, &tag(group, "Format", "ELF"))
            .await
            .map_err(fail)?;
    }
    let kind = OutputKind::Files;
    eventually(|| async {
        let rows = queries
            .results_auth
            .get_with_key(session, "f00d", &kind, &groups)
            .await
            .map_err(fail)?;
        Ok(rows.len() == 2)
    })
    .await?;
    assert_eq!(
        queries
            .results_auth
            .get_id(session, "f00d", &kind, &groups, "strings")
            .await
            .map_err(fail)?
            .len(),
        2
    );
    // 101 groups times 25 tools stays within each execution's limit
    let tools = (0..24)
        .map(|i| format!("tool{i}"))
        .chain(["strings".to_owned()])
        .collect::<Vec<_>>();
    let rows = queries
        .results_auth
        .get_with_key_and_tool(session, "f00d", &kind, &groups, &tools)
        .await
        .map_err(fail)?;
    assert_eq!(rows.len(), 2);
    eventually(|| async {
        let rows = queries
            .tags_by_item
            .get(session, &TagType::Files, &groups, "f00d")
            .await
            .map_err(fail)?;
        Ok(rows.len() == 2)
    })
    .await?;
    assert_eq!(
        queries
            .tags_by_item
            .get_rows(session, &TagType::Files, &groups, "f00d")
            .await
            .map_err(fail)?
            .len(),
        2
    );
    ks.drop().await
}

/// Entity counts round trip up to the largest value a bigint holds, and a larger count fails
/// to write with an error naming the column
#[tokio::test]
async fn result_entity_counts_are_checked() -> Result<(), Error> {
    let ks = Keyspace::new("results_unsigned").await?;
    let (session, queries) = (&ks.session, &ks.queries);
    let version = ImageVersion::Custom("1".to_owned());
    let files = HashSet::new();
    // the largest count a bigint holds reads back as written
    let largest = usize::try_from(i64::MAX).unwrap();
    let fits = HashMap::from([
        (EntityKinds::Other, largest),
        (EntityKinds::WindowsProcessTree, 0),
    ]);
    queries
        .results
        .insert(session, &result(1, &version, &files, &fits))
        .await
        .map_err(fail)?;
    let rows = collect(
        queries
            .results
            .get(session, &[Uuid::from_u128(1)])
            .await
            .map_err(fail)?,
    )?;
    let counts = rows[0].entities.clone().unwrap_or_default();
    assert_eq!(counts.get(&EntityKinds::Other), Some(&i64::MAX));
    assert_eq!(counts.get(&EntityKinds::WindowsProcessTree), Some(&0));
    // one more is refused before it reaches Scylla
    let too_big = HashMap::from([(EntityKinds::Other, largest + 1)]);
    let err = queries
        .results
        .insert(session, &result(2, &version, &files, &too_big))
        .await
        .expect_err("a count above i64::MAX can't be stored");
    assert!(err.to_string().contains("results.entities"), "{err}");
    assert!(
        queries
            .results
            .get(session, &[Uuid::from_u128(2)])
            .await
            .map_err(fail)?
            .is_empty()
    );
    ks.drop().await
}

/// Result, stream, and tag rows written by the replaced statements, with the binds the API
/// used, match the rows the generated statements write
#[tokio::test]
// the test writes each table both ways in one keyspace
#[allow(clippy::too_many_lines)]
async fn results_rows_match_the_replaced_statements() -> Result<(), Error> {
    let ks = Keyspace::new("results_parity").await?;
    let (session, queries) = (&ks.session, &ks.queries);
    let legacy = legacy_statements(&ks.name);
    let version = ImageVersion::Custom("2.0".to_owned());
    let (files, entities) = result_parts();
    // a result the way the API wrote it: files as a list and counts as signed integers
    let written = result(1, &version, &files, &entities);
    let file_list = files.iter().cloned().collect::<Vec<String>>();
    let signed = entities
        .iter()
        .map(|(kind, count)| (*kind, i64::try_from(*count).unwrap()))
        .collect::<HashMap<EntityKinds, i64>>();
    let insert = session
        .prepare(legacy["results.insert"].clone())
        .await
        .map_err(fail)?;
    session
        .execute_unpaged(
            &insert,
            (
                Uuid::from_u128(10),
                written.uploaded,
                "strings",
                &version,
                "strings -a",
                "{\"found\":1}",
                &file_list,
                &signed,
                OutputDisplayType::Json,
            ),
        )
        .await
        .map_err(fail)?;
    queries
        .results
        .insert(
            session,
            &ResultsInsert {
                id: Uuid::from_u128(20),
                ..written
            },
        )
        .await
        .map_err(fail)?;
    // a child added both ways
    let update = session
        .prepare(legacy["results.update_children"].clone())
        .await
        .map_err(fail)?;
    session
        .execute_unpaged(
            &update,
            (
                "child",
                Uuid::from_u128(3),
                Uuid::from_u128(10),
                written.uploaded,
            ),
        )
        .await
        .map_err(fail)?;
    queries
        .results
        .update_children(
            session,
            "child",
            Uuid::from_u128(3),
            Uuid::from_u128(20),
            written.uploaded,
        )
        .await
        .map_err(fail)?;
    let mut rows = ks
        .rows(&<ResultsTable as ScyllaObject>::DEF)
        .await?
        .into_iter()
        .map(|mut row| match row.columns.remove(0) {
            Some(CqlValue::Uuid(id)) => (id, row.columns),
            other => panic!("unexpected id {other:?}"),
        })
        .collect::<HashMap<_, _>>();
    let old = rows.remove(&Uuid::from_u128(10)).expect("the old row");
    let new = rows.remove(&Uuid::from_u128(20)).expect("the new row");
    assert_eq!(old, new);
    // a stream row the way the API wrote it
    let row = stream_row("old", 1, "strings", &version);
    let insert = session
        .prepare(legacy["results_stream.insert_stream"].clone())
        .await
        .map_err(fail)?;
    session
        .execute_unpaged(
            &insert,
            (
                OutputKind::Files,
                "old",
                row.year,
                row.bucket,
                row.key,
                "strings",
                &version,
                OutputDisplayType::Table,
                row.uploaded,
                "run --fast",
                row.id,
            ),
        )
        .await
        .map_err(fail)?;
    queries
        .results_stream
        .insert_stream(
            session,
            &ResultsStreamTableRef {
                group: "new",
                ..row
            },
        )
        .await
        .map_err(fail)?;
    let rows = keyed(
        ks.rows(&<ResultsStreamTable as ScyllaObject>::DEF).await?,
        1,
    );
    assert_eq!(rows["old"], rows["new"]);
    // a tag the way the API wrote it
    let written = tag("old", "Format", "PE32");
    let insert = session
        .prepare(legacy["tags.insert"].clone())
        .await
        .map_err(fail)?;
    session
        .execute_unpaged(
            &insert,
            (
                TagType::Files,
                "old",
                written.item,
                written.year,
                written.bucket,
                written.key,
                written.value,
                written.uploaded,
                written.key_lower,
                written.value_lower,
            ),
        )
        .await
        .map_err(fail)?;
    queries
        .tags
        .insert(
            session,
            &TagsTableRef {
                group: "new",
                ..written
            },
        )
        .await
        .map_err(fail)?;
    let rows = keyed(ks.rows(&<TagsTable as ScyllaObject>::DEF).await?, 1);
    assert_eq!(rows["old"], rows["new"]);
    ks.drop().await
}

/// A result's subset insert keeps the children an earlier update added, and reading rows whose
/// nullable non-key columns are null keeps each projection's outcome
#[tokio::test]
// the test walks each projection of its tables in one keyspace
#[allow(clippy::too_many_lines)]
async fn results_subset_insert_and_null_columns() -> Result<(), Error> {
    let ks = Keyspace::new("results_nulls").await?;
    let (session, queries) = (&ks.session, &ks.queries);
    let legacy = legacy_statements(&ks.name);
    let version = ImageVersion::Custom("1".to_owned());
    let (files, entities) = result_parts();
    // a subset insert over a result with a child leaves the child in place
    let written = result(1, &version, &files, &entities);
    queries
        .results
        .insert(session, &written)
        .await
        .map_err(fail)?;
    queries
        .results
        .update_children(
            session,
            "kid",
            Uuid::from_u128(9),
            written.id,
            written.uploaded,
        )
        .await
        .map_err(fail)?;
    queries
        .results
        .insert(
            session,
            &ResultsInsert {
                cmd: Some("again"),
                ..written
            },
        )
        .await
        .map_err(fail)?;
    let rows = collect(
        queries
            .results
            .get(session, &[written.id])
            .await
            .map_err(fail)?,
    )?;
    assert_eq!(rows[0].cmd.as_deref(), Some("again"));
    assert_eq!(
        rows[0].children,
        Some(HashMap::from([("kid".to_owned(), Uuid::from_u128(9))]))
    );
    // a result with only its key fails to deserialize its required columns, as before
    exec(
        session,
        &format!(
            "INSERT INTO {}.results (id, uploaded) VALUES ({}, '2026-01-01T00:00:00Z')",
            ks.name,
            Uuid::from_u128(2)
        ),
    )
    .await?;
    let rows = queries
        .results
        .get(session, &[Uuid::from_u128(2)])
        .await
        .map_err(fail)?;
    assert!(rows[0].is_err());
    let old = legacy_rows::<
        (
            Uuid,
            String,
            Option<ImageVersion>,
            Option<String>,
            chrono::DateTime<Utc>,
            String,
            Option<Vec<String>>,
            Option<HashMap<EntityKinds, i64>>,
            OutputDisplayType,
            Option<HashMap<String, Uuid>>,
        ),
        _,
    >(session, &legacy["results.get"], (vec![Uuid::from_u128(2)],))
    .await?;
    assert!(old[0].is_err());
    // a stream row with a tool but no display type fails the same way through the auth view
    exec(
        session,
        &format!(
            "INSERT INTO {}.results_stream (kind, group, year, bucket, uploaded, id, key, tool) \
             VALUES ('Files', 'g', 2026, 1, '2026-01-01T00:00:00Z', {}, 'nulls', 'strings')",
            ks.name,
            Uuid::from_u128(3)
        ),
    )
    .await?;
    let groups = vec!["g".to_owned()];
    let kind = OutputKind::Files;
    eventually(|| async {
        let rows = queries
            .results_auth
            .get_with_key(session, "nulls", &kind, &groups)
            .await
            .map_err(fail)?;
        Ok(rows.len() == 1)
    })
    .await?;
    let rows = queries
        .results_auth
        .get_with_key(session, "nulls", &kind, &groups)
        .await
        .map_err(fail)?;
    assert!(rows[0].is_err());
    let old = legacy_rows::<
        (
            Uuid,
            String,
            OutputDisplayType,
            Option<String>,
            String,
            chrono::DateTime<Utc>,
        ),
        _,
    >(
        session,
        &legacy["results_auth.get_with_key"],
        (kind, &groups, "nulls"),
    )
    .await?;
    assert!(old[0].is_err());
    // the id projections read only key columns, so they still read
    let ids = collect(
        queries
            .results_auth
            .get_id(session, "nulls", &kind, &groups, "strings")
            .await
            .map_err(fail)?,
    )?;
    assert_eq!(ids.len(), 1);
    ks.drop().await
}

/// A stream delete leaves the key's other results, and the existence statements find nothing
/// for an absent row or an unrequested group
#[tokio::test]
async fn results_deletes_and_negatives() -> Result<(), Error> {
    let ks = Keyspace::new("results_negatives").await?;
    let (session, queries) = (&ks.session, &ks.queries);
    let version = ImageVersion::Custom("1".to_owned());
    let kind = OutputKind::Files;
    // two results for one key in group `seen`
    for (id, tool) in [(1, "strings"), (2, "yara")] {
        queries
            .results_stream
            .insert_stream(session, &stream_row("seen", id, tool, &version))
            .await
            .map_err(fail)?;
    }
    let row = stream_row("seen", 1, "strings", &version);
    queries
        .results_stream
        .delete_stream(
            session,
            &kind,
            "seen",
            row.year,
            row.bucket,
            row.uploaded,
            row.id,
        )
        .await
        .map_err(fail)?;
    let (seen, unseen) = (vec!["seen".to_owned()], vec!["unseen".to_owned()]);
    eventually(|| async {
        let rows = collect(
            queries
                .results_auth
                .get_with_key(session, "f00d", &kind, &seen)
                .await
                .map_err(fail)?,
        )?;
        Ok(rows.len() == 1 && rows[0].tool == "yara")
    })
    .await?;
    // nothing is found for a group, tool, key, or id the rows don't have
    assert!(
        queries
            .results_auth
            .get_id(session, "f00d", &kind, &unseen, "yara")
            .await
            .map_err(fail)?
            .is_empty()
    );
    assert!(
        queries
            .results_auth
            .get_id(session, "f00d", &kind, &seen, "strings")
            .await
            .map_err(fail)?
            .is_empty()
    );
    assert!(
        queries
            .results_auth
            .get_with_key(session, "beef", &kind, &seen)
            .await
            .map_err(fail)?
            .is_empty()
    );
    assert!(
        queries
            .results_ids
            .count(session, "f00d", Uuid::from_u128(1), &kind)
            .await
            .map_err(fail)?
            .is_empty()
    );
    assert!(
        queries
            .results_ids
            .count(session, "f00d", Uuid::from_u128(2), &OutputKind::Repos)
            .await
            .map_err(fail)?
            .is_empty()
    );
    assert!(
        queries
            .tags_by_item
            .get(session, &TagType::Files, &seen, "f00d")
            .await
            .map_err(fail)?
            .is_empty()
    );
    ks.drop().await
}

/// The vendor metadata every entity fixture stores
fn vendor() -> EntityMetadata {
    EntityMetadata::Vendor(VendorEntity {
        countries: BTreeSet::new(),
        critical_sectors: BTreeSet::new(),
    })
}

/// An entity's row with a distinct value in every column
///
/// # Arguments
///
/// * `group` - The entity's group
/// * `id` - The entity's id
/// * `metadata` - The entity's kind-specific data
fn entity<'a>(group: &'a str, id: u128, metadata: &'a EntityMetadata) -> EntitiesTableRef<'a> {
    EntitiesTableRef {
        kind: &EntityKinds::Vendor,
        group,
        year: 2026,
        bucket: 3,
        created: Utc.with_ymd_and_hms(2026, 4, 5, 6, 7, 8).unwrap(),
        id: Uuid::from_u128(id),
        name: "acme",
        submitter: "europa",
        kind_data: metadata,
        description: Some("makes anvils"),
        image: Some("acme.png"),
    }
}

/// A network policy's row with a distinct value in every column
///
/// # Arguments
///
/// * `group` - The policy's group
/// * `name` - The policy's name
/// * `default` - Whether the policy is a default in its group
/// * `rules` - The policy's ingress rules
// the rules are borrowed whole, as the table's JSON column binds them
#[allow(clippy::ref_option)]
fn policy<'a>(
    group: &'a str,
    name: &'a str,
    default: bool,
    rules: &'a Option<Vec<NetworkPolicyRule>>,
) -> NetworkPoliciesTableRef<'a> {
    NetworkPoliciesTableRef {
        group,
        name,
        id: Uuid::from_u128(9),
        k8s_name: "k8s-name",
        created: Utc.with_ymd_and_hms(2026, 2, 3, 4, 5, 6).unwrap(),
        ingress: JsonText(rules),
        egress: JsonText(&None),
        forced_policy: true,
        default_policy: default,
    }
}

/// A notification's row with a distinct value in every column
///
/// # Arguments
///
/// * `key` - The stored key of the notification's entity
/// * `id` - The notification's id
fn notification(key: &str, id: u128) -> NotificationsTableRef<'_> {
    NotificationsTableRef {
        kind: &NotificationType::Images,
        key,
        created: Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap()
            + chrono::Duration::milliseconds(i64::try_from(id).unwrap()),
        id: Uuid::from_u128(id),
        msg: "the image failed",
        level: &NotificationLevel::Error,
        ban_id: Some(Uuid::from_u128(id + 100)),
    }
}

/// Every entity and association statement runs through its generated method and binds each
/// value to its own column
#[tokio::test]
// the test walks every statement of its tables in one keyspace
#[allow(clippy::too_many_lines)]
async fn entities_statements_round_trip() -> Result<(), Error> {
    let ks = Keyspace::new("entities_rt").await?;
    let (session, queries) = (&ks.session, &ks.queries);
    let metadata = vendor();
    // one entity in two groups and another in one
    let first = entity("ceres", 1, &metadata);
    queries
        .entities
        .insert(session, &first)
        .await
        .map_err(fail)?;
    queries
        .entities
        .insert(
            session,
            &EntitiesTableRef {
                group: "vesta",
                ..first
            },
        )
        .await
        .map_err(fail)?;
    queries
        .entities
        .insert(session, &entity("ceres", 2, &metadata))
        .await
        .map_err(fail)?;
    let groups = vec!["ceres".to_owned(), "vesta".to_owned()];
    eventually(|| async {
        let rows = queries
            .entities_by_id
            .get(session, first.id, &groups)
            .await
            .map_err(fail)?;
        Ok(rows.len() == 2)
    })
    .await?;
    let rows = collect(
        queries
            .entities_by_id
            .get(session, first.id, &groups)
            .await
            .map_err(fail)?,
    )?;
    let row = &rows[0];
    assert_eq!(row.id, first.id);
    assert_eq!(row.kind, EntityKinds::Vendor);
    assert_eq!(row.created, first.created);
    assert_eq!(row.name, "acme");
    assert_eq!(row.submitter, "europa");
    assert_eq!(
        serde_json::to_string(&row.metadata).map_err(fail)?,
        serde_json::to_string(&metadata).map_err(fail)?
    );
    assert_eq!(row.description.as_deref(), Some("makes anvils"));
    assert_eq!(row.image.as_deref(), Some("acme.png"));
    let ids = [first.id, Uuid::from_u128(2)];
    assert_eq!(
        queries
            .entities_by_id
            .get_many(session, &ids, &groups)
            .await
            .map_err(fail)?
            .len(),
        3
    );
    let names = collect(
        queries
            .entities_by_id
            .get_names_kinds_by_ids(session, &ids)
            .await
            .map_err(fail)?,
    )?;
    assert_eq!(names.len(), 3);
    assert!(
        names
            .iter()
            .all(|row| row.name == "acme" && row.kind == EntityKinds::Vendor)
    );
    // deleting the first entity from one group leaves its other group and the other entity
    queries
        .entities
        .delete(
            session,
            &EntityKinds::Vendor,
            &["vesta".to_owned()],
            first.year,
            first.bucket,
            first.created,
            first.id,
        )
        .await
        .map_err(fail)?;
    eventually(|| async {
        let rows = collect(
            queries
                .entities_by_id
                .get(session, first.id, &groups)
                .await
                .map_err(fail)?,
        )?;
        Ok(rows.len() == 1 && rows[0].group == "ceres")
    })
    .await?;
    assert_eq!(
        ks.rows(&<EntitiesTable as ScyllaObject>::DEF).await?.len(),
        2
    );
    // both directions of an association in one group
    let (forward, reverse) = association("ceres");
    queries
        .associations
        .insert(session, &forward)
        .await
        .map_err(fail)?;
    queries
        .associations
        .insert(session, &reverse)
        .await
        .map_err(fail)?;
    let rows = ks.rows(&<AssociationsTable as ScyllaObject>::DEF).await?;
    assert_eq!(rows.len(), 2);
    let row = rows
        .iter()
        .find(|row| row.columns[3] == Some(CqlValue::Text(forward.source.to_owned())))
        .expect("the forward row");
    assert_eq!(
        row.columns[5],
        Some(CqlValue::Text(forward.target.to_owned()))
    );
    assert_eq!(row.columns[6], Some(CqlValue::Text("To".to_owned())));
    assert_eq!(row.columns[7], Some(CqlValue::Text("FileFor".to_owned())));
    assert_eq!(row.columns[8], Some(CqlValue::Text("europa".to_owned())));
    assert_eq!(
        row.columns[9],
        Some(CqlValue::Text("extra-source".to_owned()))
    );
    assert_eq!(
        row.columns[10],
        Some(CqlValue::Text("extra-target".to_owned()))
    );
    assert_eq!(row.columns[11], Some(CqlValue::Uuid(Uuid::from_u128(31))));
    assert_eq!(row.columns[12], Some(CqlValue::Uuid(Uuid::from_u128(32))));
    // deleting each direction removes both rows
    for row in [&forward, &reverse] {
        queries
            .associations
            .delete(
                session,
                &["ceres".to_owned()],
                row.year,
                row.bucket,
                row.source,
                row.created,
                row.target,
                row.direction,
            )
            .await
            .map_err(fail)?;
    }
    assert_eq!(
        ks.rows(&<AssociationsTable as ScyllaObject>::DEF).await?,
        Vec::<Row>::new()
    );
    ks.drop().await
}

/// Both directions of an association between a file and a repo in one group
///
/// # Arguments
///
/// * `group` - The association's group
fn association(group: &str) -> (AssociationsTableRef<'_>, AssociationsTableRef<'_>) {
    let forward = AssociationsTableRef {
        group,
        year: 2026,
        bucket: 4,
        source: "{\"File\":\"f00d\"}",
        created: Utc.with_ymd_and_hms(2026, 3, 3, 3, 3, 3).unwrap(),
        target: "{\"Repo\":\"github.com/curl/curl\"}",
        direction: &Directionality::To,
        kind: &AssociationKind::FileFor,
        submitter: "europa",
        extra_source: Some("extra-source"),
        extra_target: Some("extra-target"),
        submissions_source: Some(Uuid::from_u128(31)),
        submissions_other: Some(Uuid::from_u128(32)),
    };
    let reverse = AssociationsTableRef {
        source: forward.target,
        target: forward.source,
        direction: &Directionality::From,
        extra_source: forward.extra_target,
        extra_target: forward.extra_source,
        submissions_source: forward.submissions_other,
        submissions_other: forward.submissions_source,
        ..forward
    };
    (forward, reverse)
}

/// Every network policy statement runs through its generated method, and its deletes reach
/// both views
#[tokio::test]
// the test walks every statement of its tables in one keyspace
#[allow(clippy::too_many_lines)]
async fn network_policies_statements_round_trip() -> Result<(), Error> {
    let ks = Keyspace::new("netpols_rt").await?;
    let (session, queries) = (&ks.session, &ks.queries);
    let rules = Some(vec![NetworkPolicyRule {
        id: Uuid::from_u128(5),
        allowed_ips: Vec::new(),
        allowed_groups: vec!["friends".to_owned()],
        allowed_tools: Vec::new(),
        allowed_local: false,
        allowed_internet: false,
        allowed_all: false,
        ports: Vec::new(),
        allowed_custom: Vec::new(),
    }]);
    // a default policy in two groups and another policy in one
    queries
        .network_policies
        .insert(session, &policy("ceres", "web", true, &rules))
        .await
        .map_err(fail)?;
    queries
        .network_policies
        .insert(session, &policy("vesta", "web", true, &rules))
        .await
        .map_err(fail)?;
    queries
        .network_policies
        .insert(session, &policy("ceres", "dns", false, &None))
        .await
        .map_err(fail)?;
    let groups = vec!["ceres".to_owned(), "vesta".to_owned()];
    eventually(|| async {
        let rows = queries
            .network_policies_by_name
            .get(session, "web", &groups)
            .await
            .map_err(fail)?;
        Ok(rows.len() == 2)
    })
    .await?;
    let rows = collect(
        queries
            .network_policies_by_name
            .get(session, "web", &groups)
            .await
            .map_err(fail)?,
    )?;
    let row = &rows[0];
    assert_eq!(row.name, "web");
    assert_eq!(row.id, Uuid::from_u128(9));
    assert_eq!(row.k8s_name, "k8s-name");
    assert_eq!(
        row.created,
        Utc.with_ymd_and_hms(2026, 2, 3, 4, 5, 6).unwrap()
    );
    assert_eq!(
        row.ingress_raw,
        serde_json::to_string(&rules).map_err(fail)?
    );
    assert_eq!(row.egress_raw, "null");
    assert!(row.forced_policy);
    assert!(row.default_policy);
    let names = vec!["web".to_owned(), "dns".to_owned()];
    assert_eq!(
        queries
            .network_policies_by_name
            .get_many(session, &names, &groups)
            .await
            .map_err(fail)?
            .len(),
        3
    );
    let found = queries
        .network_policies_by_name
        .exists(session, "dns", "ceres")
        .await
        .map_err(fail)?;
    assert_eq!(
        found.map(|row| (row.name, row.group)),
        Some(("dns".to_owned(), "ceres".to_owned()))
    );
    eventually(|| async {
        let rows = collect(
            queries
                .network_policies_default
                .get_default(session, "ceres")
                .await
                .map_err(fail)?,
        )?;
        Ok(rows.iter().map(|row| row.name.as_str()).collect::<Vec<_>>() == ["web"])
    })
    .await?;
    // deleting the default policy from both groups reaches both views
    queries
        .network_policies
        .delete(session, &groups, "web")
        .await
        .map_err(fail)?;
    eventually(|| async {
        let by_name = queries
            .network_policies_by_name
            .get(session, "web", &groups)
            .await
            .map_err(fail)?;
        let defaults = queries
            .network_policies_default
            .get_default(session, "ceres")
            .await
            .map_err(fail)?;
        Ok(by_name.is_empty() && defaults.is_empty())
    })
    .await?;
    // deleting a group's policies leaves the other group's
    queries
        .network_policies
        .insert(session, &policy("vesta", "ntp", false, &None))
        .await
        .map_err(fail)?;
    queries
        .network_policies
        .delete_all_in_group(session, "ceres")
        .await
        .map_err(fail)?;
    let rows = keyed(
        ks.rows(&<NetworkPoliciesTable as ScyllaObject>::DEF)
            .await?,
        0,
    );
    assert_eq!(rows.keys().collect::<Vec<_>>(), ["vesta"]);
    ks.drop().await
}

/// Every node statement runs through its generated method; registering again keeps the last
/// heart beat, and each update changes only its columns
#[tokio::test]
// the test walks every statement of its table in one keyspace
#[allow(clippy::too_many_lines)]
async fn nodes_statements_round_trip() -> Result<(), Error> {
    let ks = Keyspace::new("nodes_rt").await?;
    let (session, queries) = (&ks.session, &ks.queries);
    let resources = Resources {
        cpu: 16_000,
        memory: 64_000,
        ..Resources::default()
    };
    // register a node, then record a heart beat
    let registered = NodesInsert {
        cluster: "lab",
        node: "node-1",
        health: &NodeHealth::Registered,
        resources: &resources,
    };
    queries
        .nodes
        .insert(session, &registered)
        .await
        .map_err(fail)?;
    let row = first(
        queries
            .nodes
            .get(session, "lab", "node-1")
            .await
            .map_err(fail)?,
    )?;
    let row = row.expect("the node's row");
    assert_eq!((row.cluster.as_str(), row.node.as_str()), ("lab", "node-1"));
    assert_eq!(
        row.health,
        serde_json::to_string(&NodeHealth::Registered).map_err(fail)?
    );
    assert_eq!(
        row.resources,
        serde_json::to_string(&resources).map_err(fail)?
    );
    assert_eq!(row.heart_beat, None);
    let beat = Utc.with_ymd_and_hms(2026, 9, 9, 9, 9, 9).unwrap();
    queries
        .nodes
        .update_heart_beat(
            session,
            &NodeHealth::Healthy,
            &resources,
            Some(beat),
            "lab",
            "node-1",
        )
        .await
        .map_err(fail)?;
    // registering again rewrites health and resources but keeps the heart beat
    queries
        .nodes
        .insert(session, &registered)
        .await
        .map_err(fail)?;
    let row = first(
        queries
            .nodes
            .get(session, "lab", "node-1")
            .await
            .map_err(fail)?,
    )?
    .expect("the node");
    assert_eq!(row.heart_beat, Some(beat));
    assert_eq!(
        row.health,
        serde_json::to_string(&NodeHealth::Registered).map_err(fail)?
    );
    // an update without a heart beat changes health and resources only
    let smaller = Resources {
        cpu: 8_000,
        ..resources
    };
    queries
        .nodes
        .update(session, &NodeHealth::Unhealthy, &smaller, "lab", "node-1")
        .await
        .map_err(fail)?;
    let row = first(
        queries
            .nodes
            .get(session, "lab", "node-1")
            .await
            .map_err(fail)?,
    )?
    .expect("the node");
    assert_eq!(row.heart_beat, Some(beat));
    assert_eq!(
        row.health,
        serde_json::to_string(&NodeHealth::Unhealthy).map_err(fail)?
    );
    assert_eq!(
        row.resources,
        serde_json::to_string(&smaller).map_err(fail)?
    );
    assert_eq!(ks.rows(&<NodesTable as ScyllaObject>::DEF).await?.len(), 1);
    // a second node in another cluster is found by both lists
    queries
        .nodes
        .insert(
            session,
            &NodesInsert {
                cluster: "prod",
                node: "node-2",
                ..registered
            },
        )
        .await
        .map_err(fail)?;
    let clusters = vec!["lab".to_owned(), "prod".to_owned()];
    let nodes = vec!["node-1".to_owned(), "node-2".to_owned()];
    assert_eq!(
        queries
            .nodes
            .get_many(session, &clusters, &nodes)
            .await
            .map_err(fail)?
            .len(),
        2
    );
    assert!(
        first(
            queries
                .nodes
                .get(session, "prod", "node-1")
                .await
                .map_err(fail)?
        )?
        .is_none()
    );
    ks.drop().await
}

/// Every notification and log statement runs through its generated method, and each insert
/// leaves the TTL the replaced insert did
#[tokio::test]
// the test walks every statement of its tables in one keyspace
#[allow(clippy::too_many_lines)]
async fn notifications_and_logs_round_trip() -> Result<(), Error> {
    let ks = Keyspace::new("notes_rt").await?;
    let (session, queries) = (&ks.session, &ks.queries);
    let legacy = legacy_statements(&ks.name);
    let key = "{\"group\":\"g\",\"image\":\"img\"}";
    // one expiring and one lasting notification, plus one the replaced statement wrote
    queries
        .notifications
        .insert(session, &notification(key, 1))
        .await
        .map_err(fail)?;
    queries
        .notifications
        .insert_no_expire(session, &notification(key, 2))
        .await
        .map_err(fail)?;
    let old = notification(key, 3);
    let insert = session
        .prepare(legacy["notifications.insert"].clone())
        .await
        .map_err(fail)?;
    session
        .execute_unpaged(
            &insert,
            (
                NotificationType::Images,
                key,
                old.created,
                old.id,
                old.msg,
                NotificationLevel::Error,
                old.ban_id,
            ),
        )
        .await
        .map_err(fail)?;
    let rows = collect(
        queries
            .notifications
            .get(session, &NotificationType::Images, key)
            .await
            .map_err(fail)?,
    )?;
    assert_eq!(rows.len(), 3);
    let row = rows
        .iter()
        .find(|row| row.id == Uuid::from_u128(1))
        .expect("the first notification");
    assert_eq!(row.key, key);
    assert_eq!(row.created, notification(key, 1).created);
    assert_eq!(row.msg, "the image failed");
    assert_eq!(row.level, NotificationLevel::Error);
    assert_eq!(row.ban_id, Some(Uuid::from_u128(101)));
    // the generated insert keeps the table's default TTL, like the replaced one, and the
    // lasting insert sets none
    let ttls = ttls(&ks, "notifications", "msg", "id").await?;
    assert!(
        ttls[&Uuid::from_u128(1)].is_some_and(|ttl| ttl > 3000 && ttl <= 3600),
        "{ttls:?}"
    );
    assert!(
        ttls[&Uuid::from_u128(3)].is_some_and(|ttl| ttl > 3000 && ttl <= 3600),
        "{ttls:?}"
    );
    assert_eq!(ttls[&Uuid::from_u128(2)], None);
    // deleting one notification leaves its siblings, and deleting all empties the partition
    let first = notification(key, 1);
    queries
        .notifications
        .delete(
            session,
            &NotificationType::Images,
            key,
            first.created,
            first.id,
        )
        .await
        .map_err(fail)?;
    assert_eq!(
        queries
            .notifications
            .get(session, &NotificationType::Images, key)
            .await
            .map_err(fail)?
            .len(),
        2
    );
    queries
        .notifications
        .delete_all(session, &NotificationType::Images, key)
        .await
        .map_err(fail)?;
    assert!(
        queries
            .notifications
            .get(session, &NotificationType::Images, key)
            .await
            .map_err(fail)?
            .is_empty()
    );
    // three log lines in two buckets
    let reaction = Uuid::from_u128(4);
    for (position, bucket) in [(1_u64, 0), (2, 0), (2501, 1)] {
        let line = format!("line {position}");
        let row = LogsTableRef {
            reaction,
            stage: "build",
            bucket,
            position,
            line: &line,
        };
        queries.logs.insert(session, &row).await.map_err(fail)?;
    }
    // reading from position 2 with a limit of one line per bucket
    let lines = collect(
        queries
            .logs
            .get(session, reaction, "build", &[0, 1], 2, 1)
            .await
            .map_err(fail)?,
    )?;
    let lines = lines
        .into_iter()
        .map(|row| (row.position, row.line))
        .collect::<Vec<_>>();
    assert_eq!(
        lines,
        [(2, "line 2".to_owned()), (2501, "line 2501".to_owned())]
    );
    // log lines keep the table's default TTL too
    let ttls = log_ttls(&ks).await?;
    assert!(
        ttls.iter()
            .all(|ttl| ttl.is_some_and(|ttl| ttl > 3000 && ttl <= 3600)),
        "{ttls:?}"
    );
    ks.drop().await
}

/// Read the TTL of one column of every row of a table, keyed by a uuid column
///
/// # Arguments
///
/// * `ks` - The keyspace
/// * `table` - The table to read
/// * `column` - The column whose TTL to read
/// * `id` - The uuid column to key by
async fn ttls(
    ks: &Keyspace,
    table: &str,
    column: &str,
    id: &str,
) -> Result<HashMap<Uuid, Option<i32>>, Error> {
    let cql = format!("SELECT {id}, TTL({column}) FROM {}.{table}", ks.name);
    let rows = ks
        .session
        .query_unpaged(cql, &())
        .await
        .map_err(fail)?
        .into_rows_result()
        .map_err(fail)?
        .rows::<(Uuid, Option<i32>)>()
        .map_err(fail)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(fail)?;
    Ok(rows.into_iter().collect())
}

/// Read the TTL of every log line
///
/// # Arguments
///
/// * `ks` - The keyspace
async fn log_ttls(ks: &Keyspace) -> Result<Vec<Option<i32>>, Error> {
    let cql = format!("SELECT TTL(line) FROM {}.logs", ks.name);
    ks.session
        .query_unpaged(cql, &())
        .await
        .map_err(fail)?
        .into_rows_result()
        .map_err(fail)?
        .rows::<(Option<i32>,)>()
        .map_err(fail)?
        .map(|row| row.map(|(ttl,)| ttl))
        .collect::<Result<Vec<_>, _>>()
        .map_err(fail)
}

/// A table whose insert binds its TTL
#[derive(ScyllaTable)]
#[thorium_table(name = "expiring")]
pub struct ExpiringTable {
    /// The partition column
    #[thorium(partition)]
    pub id: Uuid,
    /// A value
    pub value: String,
}

/// Statements on the expiring table
#[thorium::table_queries(table = ExpiringTable)]
pub struct ExpiringQueries {
    /// Insert a row that expires after a given number of seconds
    #[insert(columns = all, ttl = bind)]
    insert: Insert,
}

/// A full-row insert that binds its TTL writes rows that expire after that TTL
#[tokio::test]
async fn bound_ttls_reach_the_rows() -> Result<(), Error> {
    let ks = Keyspace::new("bound_ttl").await?;
    exec(
        &ks.session,
        &<ExpiringTable as ScyllaObject>::DEF.create_cql(&ks.name, None),
    )
    .await?;
    let queries = ExpiringQueries::prepare(&ks.session, &ks.name)
        .await
        .map_err(fail)?;
    // write one row that expires in a minute and one that never does
    let row = ExpiringTable {
        id: Uuid::from_u128(1),
        value: "soon".to_owned(),
    };
    queries
        .insert(&ks.session, &row.as_ref(), 60)
        .await
        .map_err(fail)?;
    let row = ExpiringTable {
        id: Uuid::from_u128(2),
        value: "never".to_owned(),
    };
    queries
        .insert(&ks.session, &row.as_ref(), 0)
        .await
        .map_err(fail)?;
    let ttls = ttls(&ks, "expiring", "value", "id").await?;
    assert!(
        ttls[&Uuid::from_u128(1)].is_some_and(|ttl| ttl > 0 && ttl <= 60),
        "{ttls:?}"
    );
    assert_eq!(ttls[&Uuid::from_u128(2)], None);
    ks.drop().await
}

/// Log positions round trip up to the largest a bigint holds, and a larger position fails to
/// write with an error naming the column
#[tokio::test]
async fn log_positions_are_checked() -> Result<(), Error> {
    let ks = Keyspace::new("logs_unsigned").await?;
    let (session, queries) = (&ks.session, &ks.queries);
    let reaction = Uuid::from_u128(1);
    let largest = u64::try_from(i64::MAX).unwrap();
    let row = LogsTableRef {
        reaction,
        stage: "s",
        bucket: 0,
        position: largest,
        line: "last",
    };
    queries.logs.insert(session, &row).await.map_err(fail)?;
    let lines = collect(
        queries
            .logs
            .get(session, reaction, "s", &[0], largest, 10)
            .await
            .map_err(fail)?,
    )?;
    assert_eq!(
        lines.iter().map(|row| row.position).collect::<Vec<_>>(),
        [i64::MAX]
    );
    let err = queries
        .logs
        .insert(
            session,
            &LogsTableRef {
                position: largest + 1,
                ..row
            },
        )
        .await
        .expect_err("a position above i64::MAX can't be stored");
    assert!(err.to_string().contains("logs.position"), "{err}");
    let err = queries
        .logs
        .get(session, reaction, "s", &[0], largest + 1, 10)
        .await
        .expect_err("a position above i64::MAX can't be bound");
    assert!(err.to_string().contains("logs.position"), "{err}");
    ks.drop().await
}

/// The entity, network policy, and log statements' `IN` lists longer than one chunk return
/// every row
#[tokio::test]
// the test walks the long lists of its tables in one keyspace
#[allow(clippy::too_many_lines)]
async fn entities_long_in_lists_return_every_row() -> Result<(), Error> {
    let ks = Keyspace::new("entities_chunks").await?;
    let (session, queries) = (&ks.session, &ks.queries);
    let metadata = vendor();
    let rules = None;
    // an entity and a policy in the first and last of 101 groups
    let groups = (0..101).map(|i| format!("group{i:03}")).collect::<Vec<_>>();
    for group in [&groups[0], &groups[100]] {
        queries
            .entities
            .insert(session, &entity(group, 7, &metadata))
            .await
            .map_err(fail)?;
        queries
            .network_policies
            .insert(session, &policy(group, "web", false, &rules))
            .await
            .map_err(fail)?;
    }
    eventually(|| async {
        let rows = queries
            .entities_by_id
            .get(session, Uuid::from_u128(7), &groups)
            .await
            .map_err(fail)?;
        Ok(rows.len() == 2)
    })
    .await?;
    // 101 ids with the entity's last
    let mut ids = (100..200).map(Uuid::from_u128).collect::<Vec<_>>();
    ids.push(Uuid::from_u128(7));
    assert_eq!(
        queries
            .entities_by_id
            .get_many(session, &ids, &groups)
            .await
            .map_err(fail)?
            .len(),
        2
    );
    assert_eq!(
        queries
            .entities_by_id
            .get_names_kinds_by_ids(session, &ids)
            .await
            .map_err(fail)?
            .len(),
        2
    );
    eventually(|| async {
        let rows = queries
            .network_policies_by_name
            .get(session, "web", &groups)
            .await
            .map_err(fail)?;
        Ok(rows.len() == 2)
    })
    .await?;
    // deletes over 101 groups remove both rows
    let row = entity(&groups[0], 7, &metadata);
    queries
        .entities
        .delete(
            session,
            &EntityKinds::Vendor,
            &groups,
            row.year,
            row.bucket,
            row.created,
            row.id,
        )
        .await
        .map_err(fail)?;
    queries
        .network_policies
        .delete(session, &groups, "web")
        .await
        .map_err(fail)?;
    assert_eq!(
        ks.rows(&<EntitiesTable as ScyllaObject>::DEF).await?,
        Vec::<Row>::new()
    );
    assert_eq!(
        ks.rows(&<NetworkPoliciesTable as ScyllaObject>::DEF)
            .await?,
        Vec::<Row>::new()
    );
    // a log line in the last of 101 buckets
    let reaction = Uuid::from_u128(3);
    let row = LogsTableRef {
        reaction,
        stage: "s",
        bucket: 100,
        position: 250_001,
        line: "far",
    };
    queries.logs.insert(session, &row).await.map_err(fail)?;
    let buckets = (0..101).collect::<Vec<i32>>();
    assert_eq!(
        queries
            .logs
            .get(session, reaction, "s", &buckets, 0, 5)
            .await
            .map_err(fail)?
            .len(),
        1
    );
    ks.drop().await
}

/// Entity, association, network policy, node, notification, and log rows written by the
/// replaced statements, with the binds the API used, match the rows the generated statements
/// write
#[tokio::test]
// the test writes each table both ways in one keyspace
#[allow(clippy::too_many_lines)]
async fn entities_rows_match_the_replaced_statements() -> Result<(), Error> {
    let ks = Keyspace::new("entities_parity").await?;
    let (session, queries) = (&ks.session, &ks.queries);
    let legacy = legacy_statements(&ks.name);
    let metadata = vendor();
    // an entity, with its metadata serialized first
    let row = entity("old", 1, &metadata);
    let insert = session
        .prepare(legacy["entities.insert"].clone())
        .await
        .map_err(fail)?;
    let serialized = serde_json::to_string(&metadata).map_err(fail)?;
    session
        .execute_unpaged(
            &insert,
            (
                EntityKinds::Vendor,
                "old",
                row.year,
                row.bucket,
                row.created,
                row.id,
                row.name,
                row.submitter,
                &serialized,
                row.description,
                row.image,
            ),
        )
        .await
        .map_err(fail)?;
    queries
        .entities
        .insert(
            session,
            &EntitiesTableRef {
                group: "new",
                ..row
            },
        )
        .await
        .map_err(fail)?;
    let rows = keyed(ks.rows(&<EntitiesTable as ScyllaObject>::DEF).await?, 1);
    assert_eq!(rows["old"], rows["new"]);
    // both directions of an association, the reverse binding each swapped pair
    let insert = session
        .prepare(legacy["associations.insert"].clone())
        .await
        .map_err(fail)?;
    let (forward, reverse) = association("old");
    for row in [&forward, &reverse] {
        session
            .execute_unpaged(
                &insert,
                (
                    "old",
                    row.year,
                    row.bucket,
                    row.created,
                    *row.direction,
                    *row.kind,
                    row.source,
                    row.target,
                    row.submitter,
                    row.extra_source,
                    row.extra_target,
                    row.submissions_source,
                    row.submissions_other,
                ),
            )
            .await
            .map_err(fail)?;
    }
    let (forward, reverse) = association("new");
    queries
        .associations
        .insert(session, &forward)
        .await
        .map_err(fail)?;
    queries
        .associations
        .insert(session, &reverse)
        .await
        .map_err(fail)?;
    let mut rows = ks.rows(&<AssociationsTable as ScyllaObject>::DEF).await?;
    rows.sort_by_key(|row| format!("{:?}", &row.columns[1..]));
    let (old, new): (Vec<_>, Vec<_>) = rows
        .into_iter()
        .partition(|row| row.columns[0] == Some(CqlValue::Text("old".to_owned())));
    let strip = |rows: Vec<Row>| {
        rows.into_iter()
            .map(|row| row.columns[1..].to_vec())
            .collect::<Vec<_>>()
    };
    assert_eq!(strip(old), strip(new));
    // a network policy, with an empty egress stored as the text null
    let rules = None;
    let row = policy("old", "web", true, &rules);
    let insert = session
        .prepare(legacy["network_policies.insert"].clone())
        .await
        .map_err(fail)?;
    session
        .execute_unpaged(
            &insert,
            (
                "old",
                row.name,
                row.id,
                row.k8s_name,
                row.created,
                "null",
                "null",
                true,
                true,
            ),
        )
        .await
        .map_err(fail)?;
    queries
        .network_policies
        .insert(
            session,
            &NetworkPoliciesTableRef {
                group: "new",
                ..row
            },
        )
        .await
        .map_err(fail)?;
    let rows = keyed(
        ks.rows(&<NetworkPoliciesTable as ScyllaObject>::DEF)
            .await?,
        0,
    );
    assert_eq!(rows["old"], rows["new"]);
    // a node, then its update with a heart beat
    let resources = Resources::default();
    let insert = session
        .prepare(legacy["nodes.insert"].clone())
        .await
        .map_err(fail)?;
    let health = serde_json::to_string(&NodeHealth::Registered).map_err(fail)?;
    let raw = serde_json::to_string(&resources).map_err(fail)?;
    session
        .execute_unpaged(&insert, ("old", "n", &health, &raw))
        .await
        .map_err(fail)?;
    let registered = NodesInsert {
        cluster: "new",
        node: "n",
        health: &NodeHealth::Registered,
        resources: &resources,
    };
    queries
        .nodes
        .insert(session, &registered)
        .await
        .map_err(fail)?;
    let beat = Utc.with_ymd_and_hms(2026, 1, 1, 1, 1, 1).unwrap();
    let update = session
        .prepare(legacy["nodes.update_heart_beat"].clone())
        .await
        .map_err(fail)?;
    let healthy = serde_json::to_string(&NodeHealth::Healthy).map_err(fail)?;
    session
        .execute_unpaged(&update, (&healthy, &raw, beat, "old", "n"))
        .await
        .map_err(fail)?;
    queries
        .nodes
        .update_heart_beat(
            session,
            &NodeHealth::Healthy,
            &resources,
            Some(beat),
            "new",
            "n",
        )
        .await
        .map_err(fail)?;
    let rows = keyed(ks.rows(&<NodesTable as ScyllaObject>::DEF).await?, 0);
    assert_eq!(rows["old"], rows["new"]);
    // a notification, keyed by an image key stored as JSON text
    let image = ImageKey {
        group: "g".to_owned(),
        image: "img".to_owned(),
    };
    let insert = session
        .prepare(legacy["notifications.insert"].clone())
        .await
        .map_err(fail)?;
    let note = notification("unused", 1);
    session
        .execute_unpaged(
            &insert,
            (
                NotificationType::Images,
                &image,
                note.created,
                note.id,
                note.msg,
                NotificationLevel::Error,
                note.ban_id,
            ),
        )
        .await
        .map_err(fail)?;
    let stored = text_of(&image).map_err(fail)?;
    assert_eq!(stored, serde_json::to_string(&image).map_err(fail)?);
    let renamed = ImageKey {
        group: "g".to_owned(),
        image: "img2".to_owned(),
    };
    let renamed_text = text_of(&renamed).map_err(fail)?;
    queries
        .notifications
        .insert(
            session,
            &NotificationsTableRef {
                key: &renamed_text,
                ..note
            },
        )
        .await
        .map_err(fail)?;
    let rows = keyed(
        ks.rows(&<NotificationsTable as ScyllaObject>::DEF).await?,
        1,
    );
    let mut old = rows[&stored].clone();
    let new = rows[&renamed_text].clone();
    old.truncate(new.len());
    assert_eq!(old, new);
    // a log line, its position bound as a signed integer by the replaced statement
    let insert = session
        .prepare(legacy["logs.insert"].clone())
        .await
        .map_err(fail)?;
    session
        .execute_unpaged(&insert, (Uuid::from_u128(1), "old", 0_i32, 7_i64, "line"))
        .await
        .map_err(fail)?;
    let row = LogsTableRef {
        reaction: Uuid::from_u128(1),
        stage: "new",
        bucket: 0,
        position: 7,
        line: "line",
    };
    queries.logs.insert(session, &row).await.map_err(fail)?;
    let rows = keyed(ks.rows(&<LogsTable as ScyllaObject>::DEF).await?, 1);
    assert_eq!(rows["old"], rows["new"]);
    ks.drop().await
}

/// Reading rows whose nullable non-key columns are null keeps each projection's outcome, and
/// the existence statements find nothing for an absent row or an unrequested group
#[tokio::test]
// the test walks each projection of its tables in one keyspace
#[allow(clippy::too_many_lines)]
async fn entities_null_columns_and_negatives() -> Result<(), Error> {
    let ks = Keyspace::new("entities_nulls").await?;
    let (session, queries) = (&ks.session, &ks.queries);
    let legacy = legacy_statements(&ks.name);
    let groups = vec!["g".to_owned()];
    // rows with only their key columns, plus the view keys their views need
    for cql in [
        "INSERT INTO {ks}.entities (kind, group, year, bucket, created, id, name) VALUES ('\"Vendor\"', 'g', 2026, 1, '2026-01-01T00:00:00Z', 00000000-0000-0000-0000-000000000001, 'n')",
        "INSERT INTO {ks}.network_policies (group, name) VALUES ('g', 'p')",
        "INSERT INTO {ks}.nodes (cluster, node) VALUES ('c', 'n')",
        "INSERT INTO {ks}.notifications (kind, key, created, id) VALUES ('Images', 'k', '2026-01-01T00:00:00Z', 00000000-0000-0000-0000-000000000002)",
        "INSERT INTO {ks}.logs (reaction, stage, bucket, position) VALUES (00000000-0000-0000-0000-000000000003, 's', 0, 1)",
    ] {
        exec(session, &cql.replace("{ks}", &ks.name)).await?;
    }
    let id = Uuid::from_u128(1);
    eventually(|| async {
        let rows = queries
            .entities_by_id
            .get(session, id, &groups)
            .await
            .map_err(fail)?;
        Ok(rows.len() == 1)
    })
    .await?;
    // each projection with a required column fails as the replaced statement's types did
    let rows = queries
        .entities_by_id
        .get(session, id, &groups)
        .await
        .map_err(fail)?;
    assert!(rows[0].is_err());
    let old = legacy_rows::<
        (
            Uuid,
            String,
            EntityKinds,
            chrono::DateTime<Utc>,
            String,
            String,
            EntityMetadata,
            Option<String>,
            Option<String>,
        ),
        _,
    >(session, &legacy["entities_by_id.get"], (id, &groups))
    .await?;
    assert!(old[0].is_err());
    let names = collect(
        queries
            .entities_by_id
            .get_names_kinds_by_ids(session, &[id])
            .await
            .map_err(fail)?,
    )?;
    assert_eq!(names.len(), 1);
    eventually(|| async {
        let rows = queries
            .network_policies_by_name
            .get(session, "p", &groups)
            .await
            .map_err(fail)?;
        Ok(rows.len() == 1)
    })
    .await?;
    let rows = queries
        .network_policies_by_name
        .get(session, "p", &groups)
        .await
        .map_err(fail)?;
    assert!(rows[0].is_err());
    let old = legacy_rows::<
        (
            String,
            String,
            Uuid,
            String,
            chrono::DateTime<Utc>,
            String,
            String,
            bool,
            bool,
        ),
        _,
    >(
        session,
        &legacy["network_policies_by_name.get"],
        ("p", &groups),
    )
    .await?;
    assert!(old[0].is_err());
    assert!(first(queries.nodes.get(session, "c", "n").await.map_err(fail)?).is_err());
    let old = legacy_rows::<
        (
            String,
            String,
            String,
            String,
            Option<chrono::DateTime<Utc>>,
        ),
        _,
    >(session, &legacy["nodes.get"], ("c", "n"))
    .await?;
    assert!(old[0].is_err());
    let rows = queries
        .notifications
        .get(session, &NotificationType::Images, "k")
        .await
        .map_err(fail)?;
    assert!(rows[0].is_err());
    let old = legacy_rows::<
        (
            String,
            chrono::DateTime<Utc>,
            Uuid,
            String,
            NotificationLevel,
            Option<Uuid>,
        ),
        _,
    >(
        session,
        &legacy["notifications.get"],
        (NotificationType::Images, "k"),
    )
    .await?;
    assert!(old[0].is_err());
    let lines = queries
        .logs
        .get(session, Uuid::from_u128(3), "s", &[0], 0, 10)
        .await
        .map_err(fail)?;
    assert!(lines[0].is_err());
    let old = legacy_rows::<(i64, String), _>(
        session,
        &legacy["logs.get"],
        (Uuid::from_u128(3), "s", vec![0_i32], 0_i64, 10_i32),
    )
    .await?;
    assert!(old[0].is_err());
    // nothing is found for an absent policy, an unrequested group, or an absent entity
    let rules = None;
    queries
        .network_policies
        .insert(session, &policy("seen", "web", false, &rules))
        .await
        .map_err(fail)?;
    eventually(|| async {
        let found = queries
            .network_policies_by_name
            .exists(session, "web", "seen")
            .await
            .map_err(fail)?;
        Ok(found.is_some())
    })
    .await?;
    assert!(
        queries
            .network_policies_by_name
            .exists(session, "web", "unseen")
            .await
            .map_err(fail)?
            .is_none()
    );
    assert!(
        queries
            .network_policies_by_name
            .exists(session, "nope", "seen")
            .await
            .map_err(fail)?
            .is_none()
    );
    assert!(
        queries
            .entities_by_id
            .get(session, id, &["unseen".to_owned()])
            .await
            .map_err(fail)?
            .is_empty()
    );
    assert!(
        queries
            .entities_by_id
            .get(session, Uuid::from_u128(99), &groups)
            .await
            .map_err(fail)?
            .is_empty()
    );
    assert!(
        queries
            .network_policies_default
            .get_default(session, "seen")
            .await
            .map_err(fail)?
            .is_empty()
    );
    ks.drop().await
}

/// The entities cursor's statements read newest first against the table's ascending clustering
/// order: a pull with `IN` on both partition-key columns runs unpaged and returns rows newest
/// first across kinds and buckets, and a tie query reads one kind from its tie downward
#[tokio::test]
async fn entity_cursor_reads_newest_first_across_partitions() -> Result<(), Error> {
    use thorium::models::schema::{Bucket, CursorLimit, DEFAULT_LIMIT_CAP, TimeCursorStatements};
    let ks = Keyspace::new("entity_cursor").await?;
    let (session, queries) = (&ks.session, &ks.queries);
    let metadata = vendor();
    // entities of two kinds in two buckets, each created a minute apart
    let base = Utc.with_ymd_and_hms(2026, 4, 5, 6, 0, 0).unwrap();
    let kinds = [EntityKinds::Vendor, EntityKinds::Other];
    let mut written = Vec::new();
    for index in 0..8_u32 {
        let kind = &kinds[(index % 2) as usize];
        let bucket = i32::try_from(index / 4).unwrap();
        let created = base + chrono::Duration::minutes(i64::from(index));
        let row = EntitiesTableRef {
            kind,
            bucket,
            created,
            id: Uuid::from_u128(u128::from(index) + 1),
            ..entity("g", 0, &metadata)
        };
        queries.entities.insert(session, &row).await.map_err(fail)?;
        written.push(created);
    }
    // pull both kinds from both buckets in one unpaged statement
    let limit = CursorLimit::from_user(10, DEFAULT_LIMIT_CAP, "limit").map_err(fail)?;
    let rows = collect(
        queries
            .entities
            .list
            .pull(
                session,
                &kinds.to_vec(),
                &"g".to_owned(),
                2026,
                &[Bucket::new(0), Bucket::new(1)],
                base + chrono::Duration::hours(1),
                base - chrono::Duration::hours(1),
                limit,
            )
            .await
            .map_err(fail)?,
    )?;
    let created = rows.iter().map(|row| row.created).collect::<Vec<_>>();
    written.sort_unstable_by(|left, right| right.cmp(left));
    assert_eq!(created, written);
    // a tie query reads one kind's rows at a timestamp from the tie downward
    let at = base + chrono::Duration::minutes(2);
    let tie = Uuid::from_u128(3);
    let rows = collect(
        queries
            .entities
            .list
            .ties(
                session,
                &kinds.to_vec(),
                &("g".to_owned(), EntityKinds::Vendor),
                2026,
                Bucket::new(0),
                at,
                &tie,
                limit,
            )
            .await
            .map_err(fail)?,
    )?;
    assert_eq!(rows.iter().map(|row| row.id).collect::<Vec<_>>(), [tie]);
    ks.drop().await
}

/// A negative value stored in an unsigned column fails the row's read with an error naming the
/// table and the column, which the API returns as a 500
#[tokio::test]
async fn stored_negative_unsigned_values_fail() -> Result<(), Error> {
    use thorium::models::schema::ScyllaProjection;
    use thorium::utils::ApiError;
    /// A stage log line's position, read as the unsigned value it's written from
    #[derive(Debug, ScyllaProjection)]
    #[thorium_projection(of = LogsTable)]
    struct PositionRow {
        /// The line's position
        #[thorium(unsigned = "bigint")]
        position: u64,
    }
    let ks = Keyspace::new("logs_negative").await?;
    let session = &ks.session;
    let reaction = Uuid::from_u128(9);
    // a corrupt row whose position is negative
    exec(
        session,
        &format!(
            "INSERT INTO {}.logs (reaction, stage, bucket, position, line) VALUES ({reaction}, 's', 0, -1, 'x')",
            ks.name
        ),
    )
    .await?;
    let rows = session
        .query_unpaged(
            format!(
                "SELECT position FROM {}.logs WHERE reaction = {reaction} AND stage = 's' AND bucket = 0",
                ks.name
            ),
            &(),
        )
        .await
        .map_err(fail)?
        .into_rows_result()
        .map_err(fail)?;
    let err = rows
        .rows::<PositionRow>()
        .map_err(fail)?
        .next()
        .expect("the corrupt row is read")
        .expect_err("a negative position can't be read as unsigned");
    // the error names the table and the column, and the API answers it with a 500
    assert!(err.to_string().contains("logs"), "{err}");
    assert!(err.to_string().contains("position"), "{err}");
    assert_eq!(ApiError::from(err).status().as_u16(), 500);
    ks.drop().await
}
