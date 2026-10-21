//! Tests that generated rows write the same values however they are bound

use chrono::{DateTime, TimeZone, Utc};
use scylla::client::session::Session;
use scylla::value::Row;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use thorium::Error;
use thorium::models::schema::scylla::{SamplesListTable, SamplesListTableRef};
use thorium::models::schema::{
    JsonText, ScyllaObject, ScyllaStoreJson, ScyllaTable, TableStatements, with_keyspace,
};
use thorium::test_utilities::{self, CONF};
use uuid::Uuid;

/// A setting stored as JSON text
#[derive(Debug, Clone, Serialize, Deserialize, ScyllaStoreJson)]
pub struct HarvestSettings {
    /// How deep to plow
    pub depth: i32,
}

/// One column of most kinds a borrowed row maps
#[derive(Debug, Clone, ScyllaTable)]
#[thorium_table(name = "every_kind")]
pub struct EveryKindTable {
    /// How the row was bound, as its partition key
    #[thorium(partition)]
    pub way: String,
    /// A tinyint
    pub tiny: i8,
    /// A smallint
    pub small: i16,
    /// An int
    pub int: i32,
    /// An optional bigint
    pub big: Option<i64>,
    /// A boolean
    pub flag: bool,
    /// A float
    pub float: f32,
    /// A double
    pub double: f64,
    /// A uuid
    pub id: Uuid,
    /// A timestamp
    pub at: DateTime<Utc>,
    /// Optional text
    pub note: Option<String>,
    /// A list
    pub items: Vec<String>,
    /// An optional list
    pub opt_list: Option<Vec<String>>,
    /// A set
    pub tags: HashSet<String>,
    /// A map
    pub lookup: BTreeMap<String, Uuid>,
    /// An unsigned scalar
    #[thorium(unsigned = "bigint")]
    pub count: u64,
    /// A map of unsigned values
    #[thorium(unsigned = "bigint")]
    pub counts: HashMap<String, usize>,
    /// A JSON store type
    pub settings: HarvestSettings,
    /// A JSON text wrapper whose `None` is the text `null`
    pub rules: JsonText<Option<Vec<String>>>,
}

/// Statements on the every-kind table
#[thorium::table_queries(table = EveryKindTable)]
pub struct EveryKindQueries {
    /// Insert or overwrite one row
    #[insert(columns = all, ttl = none)]
    insert: Insert,
}

/// The every-kind table as CQL
const EVERY_KIND: &str = "CREATE TABLE {ns}.every_kind (way TEXT, tiny TINYINT, small SMALLINT, \
    int INT, big BIGINT, flag BOOLEAN, float FLOAT, double DOUBLE, id UUID, at TIMESTAMP, \
    note TEXT, items LIST<TEXT>, opt_list LIST<TEXT>, tags SET<TEXT>, lookup MAP<TEXT, UUID>, \
    count BIGINT, counts MAP<TEXT, BIGINT>, settings TEXT, rules TEXT, PRIMARY KEY (way))";

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

/// Create a fresh keyspace holding the every-kind table
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace to create, replacing any left by an earlier run
async fn fresh_keyspace(session: &Session, keyspace: &str) -> Result<(), Error> {
    // replace any keyspace an earlier run left behind
    exec(session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await?;
    exec(
        session,
        &format!(
            "CREATE KEYSPACE {keyspace} WITH replication = \
            {{'class': 'SimpleStrategy', 'replication_factor': 1}}"
        ),
    )
    .await?;
    exec(session, &EVERY_KIND.replace("{ns}", keyspace)).await
}

/// Read every row of the every-kind table through its select-all statement, keyed by how each
/// row was bound
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace the table is in
async fn read_all(session: &Session, keyspace: &str) -> Result<HashMap<String, Row>, Error> {
    let cql = with_keyspace(<EveryKindTable as TableStatements>::SELECT_ALL, keyspace);
    let rows = session
        .query_unpaged(cql, &())
        .await
        .map_err(|err| Error::new(err.to_string()))?
        .into_rows_result()
        .map_err(|err| Error::new(err.to_string()))?
        .rows::<Row>()
        .map_err(|err| Error::new(err.to_string()))?
        .collect::<Result<Vec<Row>, _>>()
        .map_err(|err| Error::new(err.to_string()))?;
    // the first column is the partition key naming how the row was bound
    let mut keyed = HashMap::new();
    for mut row in rows {
        let way = row
            .columns
            .remove(0)
            .and_then(scylla::value::CqlValue::into_string);
        keyed.insert(way.expect("every row has its way"), row);
    }
    Ok(keyed)
}

/// The same values written through the owned row, its `as_ref`, and a borrowed row built from
/// separate parts read back as identical rows
#[tokio::test]
async fn bound_rows_write_identical_values() -> Result<(), Error> {
    let keyspace = format!("{}_schema_rows", CONF.thorium.namespace);
    let session = test_utilities::scylla_session().await?;
    fresh_keyspace(&session, &keyspace).await?;
    let queries = EveryKindQueries::prepare(&session, &keyspace)
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    // one row with a distinct value in every column
    let owned = EveryKindTable {
        way: "owned".to_owned(),
        tiny: 7,
        small: 300,
        int: 70_000,
        big: Some(5_000_000_000),
        flag: true,
        float: 1.5,
        double: 2.25,
        id: Uuid::from_u128(0xc0de),
        at: Utc.with_ymd_and_hms(2026, 10, 19, 4, 5, 6).unwrap(),
        note: None,
        items: vec!["corn".to_owned(), "wheat".to_owned()],
        opt_list: Some(vec!["barley".to_owned()]),
        tags: HashSet::from(["oats".to_owned()]),
        lookup: BTreeMap::from([("rye".to_owned(), Uuid::from_u128(9))]),
        count: u64::try_from(i64::MAX).unwrap(),
        counts: HashMap::from([("Device".to_owned(), 3)]),
        settings: HarvestSettings { depth: 4 },
        rules: JsonText(None),
    };
    // bind the owned row directly
    session
        .execute_unpaged(&queries.insert, &owned)
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    // bind its borrowed form through the generated insert
    let as_ref = EveryKindTable {
        way: "as_ref".to_owned(),
        ..owned.clone()
    };
    queries
        .insert(&session, &as_ref.as_ref())
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    // bind a borrowed row built field by field from separate values
    let items = owned.items.clone();
    let settings = owned.settings.clone();
    let parts = EveryKindTableRef {
        way: "parts",
        tiny: owned.tiny,
        small: owned.small,
        int: owned.int,
        big: owned.big,
        flag: owned.flag,
        float: owned.float,
        double: owned.double,
        id: owned.id,
        at: owned.at,
        note: None,
        items: &items,
        opt_list: owned.opt_list.as_deref(),
        tags: &owned.tags,
        lookup: &owned.lookup,
        count: owned.count,
        counts: &owned.counts,
        settings: &settings,
        rules: JsonText(&owned.rules.0),
    };
    queries
        .insert(&session, &parts)
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    // all three rows read back with the same values
    let rows = read_all(&session, &keyspace).await?;
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert_eq!(rows["owned"], rows["as_ref"]);
    assert_eq!(rows["owned"], rows["parts"]);
    // the JSON columns hold the text the types serialize to, with `None` as the text `null`
    let rules = rows["owned"].columns.last().cloned().flatten();
    assert_eq!(
        rules.and_then(scylla::value::CqlValue::into_string),
        Some("null".to_owned())
    );
    // remove our keyspace
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}

/// The same submission written to `samples_list` through the owned row, its `as_ref`, and a
/// borrowed row built from separate parts reads back as identical rows, in a table created from
/// its declaration
#[tokio::test]
async fn samples_list_rows_write_identical_values() -> Result<(), Error> {
    let keyspace = format!("{}_schema_samples", CONF.thorium.namespace);
    let session = test_utilities::scylla_session().await?;
    // create the declared table in a fresh keyspace
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await?;
    exec(
        &session,
        &format!(
            "CREATE KEYSPACE {keyspace} WITH replication = \
            {{'class': 'SimpleStrategy', 'replication_factor': 1}}"
        ),
    )
    .await?;
    let create = <SamplesListTable as ScyllaObject>::DEF.create_cql(&keyspace, None);
    exec(&session, &create).await?;
    let insert = session
        .prepare(with_keyspace(
            <SamplesListTable as TableStatements>::INSERT,
            &keyspace,
        ))
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    // one submission, written to three groups three ways
    let owned = SamplesListTable {
        group: "minerva".to_owned(),
        year: 2026,
        bucket: 7,
        uploaded: Utc.with_ymd_and_hms(2026, 10, 19, 4, 5, 6).unwrap(),
        id: Uuid::from_u128(0x5eed),
        sha256: "a".repeat(64),
        sha1: "b".repeat(40),
        md5: "c".repeat(32),
        name: Some("corn.exe".to_owned()),
        description: None,
        submitter: "europa".to_owned(),
        origin: Some("{\"Downloaded\":{}}".to_owned()),
    };
    session
        .execute_unpaged(&insert, &owned)
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    session
        .execute_unpaged(
            &insert,
            &SamplesListTableRef {
                group: "mars",
                ..owned.as_ref()
            },
        )
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    let sha256 = owned.sha256.clone();
    let parts = SamplesListTableRef {
        group: "vesta",
        year: owned.year,
        bucket: owned.bucket,
        uploaded: owned.uploaded,
        id: owned.id,
        sha256: &sha256,
        sha1: &owned.sha1,
        md5: &owned.md5,
        name: owned.name.as_deref(),
        description: None,
        submitter: &owned.submitter,
        origin: owned.origin.as_deref(),
    };
    session
        .execute_unpaged(&insert, &parts)
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    // read every row back through the select-all statement, keyed by group
    let cql = with_keyspace(<SamplesListTable as TableStatements>::SELECT_ALL, &keyspace);
    let rows = session
        .query_unpaged(cql, &())
        .await
        .map_err(|err| Error::new(err.to_string()))?
        .into_rows_result()
        .map_err(|err| Error::new(err.to_string()))?
        .rows::<Row>()
        .map_err(|err| Error::new(err.to_string()))?
        .collect::<Result<Vec<Row>, _>>()
        .map_err(|err| Error::new(err.to_string()))?;
    let mut keyed = HashMap::new();
    for mut row in rows {
        let group = row
            .columns
            .remove(0)
            .and_then(scylla::value::CqlValue::into_string);
        keyed.insert(group.expect("every row has a group"), row);
    }
    assert_eq!(keyed.len(), 3, "{keyed:?}");
    assert_eq!(keyed["minerva"], keyed["mars"]);
    assert_eq!(keyed["minerva"], keyed["vesta"]);
    // remove our keyspace
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}
