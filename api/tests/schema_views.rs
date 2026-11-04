//! Tests that declared materialized views are recreated from the registry, that nothing the
//! registry doesn't declare as a view is ever dropped, and that views out of step with their base
//! tables are found
//!
//! A view can't be written directly (Scylla refuses `DELETE` on a view and loading view
//! sstables through `nodetool refresh`), so the tests that need a stale view build one the only
//! way Scylla allows: rows and tombstones written to a scratch table with the view's columns are
//! copied into the view's data directory while Scylla restarts. Those tests are `#[ignore]`d (a CI
//! job cannot restart its Scylla) and need the test Scylla's Docker container name in
//! `THORIUM_TEST_SCYLLA_CONTAINER`.

use scylla::client::session::Session;
use scylla::value::{CqlValue, Row};
use std::collections::BTreeSet;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};
use thorium::models::schema::{
    ApplyError, ApplyOptions, ApplyReport, ApplyTarget, ColumnRole, HostStatus, RangeDifference,
    RangeSelection, RangeSide, RecreateReport, TOKEN_RANGES, VerifyOptions, VerifyReport,
    ViewCheck, ViewDef, ViewError, ViewState, apply, cluster_hosts, declared_views, member_build,
    range_of, recreate_statements, recreate_views, recreate_warning, resolve_views, token_range,
    verify_views, view_build_statuses, views_built,
};
use thorium::test_utilities::{self, CONF};
use thorium::{Conf, Error};
use tokio::sync::RwLock;
use uuid::Uuid;

#[path = "schema_views/manual.rs"]
mod manual;

use manual::make_stale;

/// Held for reading by every test, and for writing by a test that restarts Scylla, so no test is
/// mid-query while the node restarts
static RESTART: LazyLock<RwLock<()>> = LazyLock::new(|| RwLock::new(()));

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

/// Read every row of a query, paging through all of them
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `cql` - The query
async fn rows(session: &Session, cql: &str) -> Result<Vec<Row>, Error> {
    // page through the result, keeping every row
    let mut pager = session
        .query_iter(cql, &())
        .await
        .map_err(|err| Error::new(format!("{cql}: {err}")))?
        .rows_stream::<Row>()
        .map_err(|err| Error::new(format!("{cql}: {err}")))?;
    let mut out = Vec::new();
    while let Some(row) = futures::StreamExt::next(&mut pager).await {
        out.push(row.map_err(|err| Error::new(format!("{cql}: {err}")))?);
    }
    Ok(out)
}

/// Build a config for a keyspace beside the run's own namespace, dropping any keyspace an earlier
/// run left there, and apply the declared schema to it
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `suffix` - What makes this test's keyspace name unique
async fn applied(session: &Session, suffix: &str) -> Result<Conf, Error> {
    // replace any keyspace an earlier run left behind
    let keyspace = format!("{}_{suffix}", CONF.thorium.namespace);
    exec(session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await?;
    // point a copy of the test config at it and converge it
    let mut conf = CONF.clone();
    conf.thorium.namespace = keyspace;
    let options = ApplyOptions {
        target: ApplyTarget::Declared,
        dry_run: false,
        view_wait: None,
    };
    apply(session, &conf, &options, "", &|_| ())
        .await
        .map_err(|err| Error::new(format!("schema apply failed: {err}")))?;
    Ok(conf)
}

/// Get a declared view by name
///
/// # Arguments
///
/// * `name` - The view's name
fn view(name: &str) -> ViewDef {
    // the registry declares every view the tests use
    declared_views()
        .into_iter()
        .find(|view| view.name == name)
        .unwrap_or_else(|| panic!("{name} is a declared view"))
}

/// The names of a view's primary key columns, partition columns first, each in key order
///
/// # Arguments
///
/// * `view` - The view
fn key_columns(view: &ViewDef) -> Vec<&'static str> {
    // partition columns by position, then clustering columns by position
    let mut partition = Vec::new();
    let mut clustering = Vec::new();
    for column in view.columns {
        match column.role {
            ColumnRole::Partition(at) => partition.push((at, column.name)),
            ColumnRole::Cluster(at, _) => clustering.push((at, column.name)),
            _ => (),
        }
    }
    partition.sort_unstable();
    clustering.sort_unstable();
    partition
        .into_iter()
        .chain(clustering)
        .map(|(_, name)| name)
        .collect()
}

/// The view's key of every row the view holds
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace the view is in
/// * `view` - The view
async fn view_keys(
    session: &Session,
    keyspace: &str,
    view: &ViewDef,
) -> Result<BTreeSet<String>, Error> {
    // read the view's key columns of every row
    let columns = key_columns(view).join(", ");
    let cql = format!("SELECT {columns} FROM {keyspace}.{}", view.name);
    Ok(rows(session, &cql)
        .await?
        .into_iter()
        .map(|row| format!("{:?}", row.columns))
        .collect())
}

/// The view's key of every base row the view should hold: rows whose key columns are all set and
/// that pass the view's filter
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace the view is in
/// * `view` - The view
async fn expected_keys(
    session: &Session,
    keyspace: &str,
    view: &ViewDef,
) -> Result<BTreeSet<String>, Error> {
    // read the view's key columns from the base, applying the view's filter there
    let columns = key_columns(view).join(", ");
    let filter = view.filter.map_or_else(String::new, |filter| {
        format!(" WHERE {filter} ALLOW FILTERING")
    });
    let cql = format!("SELECT {columns} FROM {keyspace}.{}{filter}", view.base);
    // a row with an unset key column isn't in the view
    Ok(rows(session, &cql)
        .await?
        .into_iter()
        .filter(|row| row.columns.iter().all(Option::is_some))
        .map(|row| format!("{:?}", row.columns))
        .collect())
}

/// Wait until a view holds exactly the rows its base table gives it, since view updates are
/// applied in the background
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace the view is in
/// * `view` - The view
async fn settled(session: &Session, keyspace: &str, view: &ViewDef) -> Result<(), Error> {
    // poll for up to a minute
    let started = Instant::now();
    loop {
        let expected = expected_keys(session, keyspace, view).await?;
        let held = view_keys(session, keyspace, view).await?;
        if expected == held {
            return Ok(());
        }
        if started.elapsed() > Duration::from_secs(60) {
            return Err(Error::new(format!(
                "{} never caught up: {} rows expected, {} held",
                view.name,
                expected.len(),
                held.len()
            )));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Write sample rows to `samples_list`, returning their sha256s
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace to write to
/// * `count` - How many rows to write
async fn seed_samples(session: &Session, keyspace: &str, count: u32) -> Result<Vec<String>, Error> {
    // write each row with its own sha256, spread over 64 buckets
    let cql = format!(
        "INSERT INTO {keyspace}.samples_list (group, year, bucket, uploaded, id, sha256, name) \
         VALUES (?, ?, ?, ?, ?, ?, ?)"
    );
    let insert = session
        .prepare(cql.as_str())
        .await
        .map_err(|err| Error::new(format!("{cql}: {err}")))?;
    let mut shas = Vec::new();
    for i in 0..count {
        let sha = format!("{i:064x}");
        let values = (
            "views",
            2026_i32,
            i32::try_from(i % 64).expect("small bucket"),
            scylla::value::CqlTimestamp(1_700_000_000_000 + i64::from(i)),
            Uuid::new_v4(),
            sha.as_str(),
            format!("sample-{i}"),
        );
        session
            .execute_unpaged(&insert, values)
            .await
            .map_err(|err| Error::new(format!("{cql}: {err}")))?;
        shas.push(sha);
    }
    Ok(shas)
}

/// The id Scylla gives a table or view, as its data directory names it
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace holding the object
/// * `name` - The table or view
async fn object_id(session: &Session, keyspace: &str, name: &str) -> Result<Option<Uuid>, Error> {
    // a view is listed with the views, a table with the tables
    for cql in [
        "SELECT id FROM system_schema.views WHERE keyspace_name = ? AND view_name = ?",
        "SELECT id FROM system_schema.tables WHERE keyspace_name = ? AND table_name = ?",
    ] {
        let found = session
            .query_unpaged(cql, (keyspace, name))
            .await
            .map_err(|err| Error::new(format!("{cql}: {err}")))?
            .into_rows_result()
            .map_err(|err| Error::new(format!("{cql}: {err}")))?
            .maybe_first_row::<(Uuid,)>()
            .map_err(|err| Error::new(format!("{cql}: {err}")))?;
        if let Some((id,)) = found {
            return Ok(Some(id));
        }
    }
    Ok(None)
}

/// Collect the progress lines a call reports
#[derive(Default)]
struct Lines(Mutex<Vec<String>>);

impl Lines {
    /// Record one line
    ///
    /// # Arguments
    ///
    /// * `line` - The line reported
    fn push(&self, line: &str) {
        // keep every line in order
        self.0.lock().unwrap().push(line.to_owned());
    }

    /// Every line recorded
    fn all(&self) -> Vec<String> {
        // copy them out
        self.0.lock().unwrap().clone()
    }
}

/// A name the registry doesn't declare as a view is refused before anything is dropped: an extra
/// view made by hand, a declared table, and a list naming a declared view beside an undeclared
/// one, both by name and as a hand-built definition
#[tokio::test]
async fn an_undeclared_view_is_refused_and_nothing_is_dropped() -> Result<(), Error> {
    // a converged keyspace with an extra view made by hand
    let _guard = RESTART.read().await;
    let session = test_utilities::scylla_session().await?;
    let conf = applied(&session, "views_undeclared").await?;
    let ks = conf.thorium.namespace.clone();
    exec(
        &session,
        &format!(
            "CREATE MATERIALIZED VIEW {ks}.extra_by_name AS SELECT name, group, year, bucket, \
             uploaded, id FROM {ks}.samples_list WHERE name IS NOT NULL AND group IS NOT NULL \
             AND year IS NOT NULL AND bucket IS NOT NULL AND uploaded IS NOT NULL AND id IS NOT \
             NULL PRIMARY KEY ((name), group, year, bucket, uploaded, id)"
        ),
    )
    .await?;
    // each undeclared name is refused, alone or beside a declared view
    for names in [
        vec!["extra_by_name"],
        vec!["samples_list"],
        vec!["samples", "extra_by_name"],
        vec![""],
    ] {
        match resolve_views(&names) {
            Err(ViewError::Undeclared { name }) => assert!(names.contains(&name.as_str())),
            other => panic!("{names:?} wasn't refused: {other:?}"),
        }
    }
    // a hand-built definition of the extra view is refused by the recreate itself
    let extra = ViewDef {
        name: "extra_by_name",
        ..view("samples")
    };
    let refused = recreate_views(
        &session,
        &conf,
        &[view("samples"), extra],
        false,
        None,
        &|_| (),
    )
    .await;
    assert!(
        matches!(&refused, Err(ViewError::Undeclared { name }) if name == "extra_by_name"),
        "{refused:?}"
    );
    // nothing was dropped or recreated
    assert!(object_id(&session, &ks, "extra_by_name").await?.is_some());
    let samples = object_id(&session, &ks, "samples").await?;
    assert!(samples.is_some());
    assert_eq!(object_id(&session, &ks, "samples").await?, samples);
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {ks}")).await?;
    Ok(())
}

/// A declared view resolves to its registry definition, a repeated name once, in the order given
#[test]
fn declared_views_resolve_in_order() {
    // two declared views, one named twice
    let views = resolve_views(&["samples", "tags_by_item", "samples"]).expect("declared views");
    let names = views.iter().map(|view| view.name).collect::<Vec<_>>();
    assert_eq!(names, ["samples", "tags_by_item"]);
    assert_eq!(views[0], view("samples"));
    // the statements drop the view, then create it from the registry
    let [drop, create] = recreate_statements("ks", &views[0]);
    assert_eq!(drop, "DROP MATERIALIZED VIEW IF EXISTS ks.samples");
    assert_eq!(
        create,
        thorium::models::schema::ObjectDef::View(views[0]).create_cql("ks", None)
    );
    // the warning names the view, its base table, and what readers see until the build ends
    let warning = recreate_warning(&views[0]);
    assert!(warning.contains("`samples`"), "{warning}");
    assert!(warning.contains("`samples_list`"), "{warning}");
    assert!(
        warning.contains(
            "routes reading it fail while it is dropped, then see missing rows until its build \
             finishes"
        ),
        "{warning}"
    );
}

/// A create that fails after the drop says the view is now missing and how to create it again
#[tokio::test]
async fn a_failed_create_after_the_drop_says_the_view_is_missing() -> Result<(), Error> {
    // a converged keyspace whose view is gone and whose base table lost a column the view selects,
    // so the drop succeeds and the create that follows it fails
    let _guard = RESTART.read().await;
    let session = test_utilities::scylla_session().await?;
    let conf = applied(&session, "views_create_fails").await?;
    let ks = conf.thorium.namespace.clone();
    exec(
        &session,
        &format!("DROP MATERIALIZED VIEW {ks}.network_policies_by_name"),
    )
    .await?;
    exec(
        &session,
        &format!("ALTER TABLE {ks}.network_policies DROP k8s_name"),
    )
    .await?;
    // the recreate fails, saying the view is missing and that an apply creates it
    let failed = recreate_views(
        &session,
        &conf,
        &[view("network_policies_by_name")],
        false,
        None,
        &|_| (),
    )
    .await
    .expect_err("the create fails");
    let message = failed.to_string();
    assert!(
        message.contains("`network_policies_by_name` is now missing"),
        "{message}"
    );
    assert!(message.contains("thoradm schema apply"), "{message}");
    assert!(
        message.contains("--recreate-view network_policies_by_name"),
        "{message}"
    );
    assert!(message.contains("CREATE MATERIALIZED VIEW"), "{message}");
    assert_eq!(
        object_id(&session, &ks, "network_policies_by_name").await?,
        None
    );
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {ks}")).await?;
    Ok(())
}

/// A declared view made stale (rows hidden from it, and a row no base row gives) is dropped,
/// created again from the registry, and waited for: once the call returns the view holds exactly
/// its base table's rows, and a read prepared against the view before the recreate finds the rows
/// that were missing
#[tokio::test]
#[ignore = "restarts the test Scylla through docker, which a CI job cannot do; run by hand against a disposable Scylla container: THORIUM_TEST_SCYLLA_CONTAINER=<container> cargo test -p thorium-api --features test-utilities --test schema_views -- --ignored"]
async fn a_stale_view_is_recreated_and_matches_its_base_again() -> Result<(), Error> {
    // a converged keyspace with sample rows, their view caught up
    let _guard = RESTART.write().await;
    let session = test_utilities::scylla_session().await?;
    let conf = applied(&session, "views_stale").await?;
    let ks = conf.thorium.namespace.clone();
    let samples = view("samples");
    seed_samples(&session, &ks, 500).await?;
    settled(&session, &ks, &samples).await?;
    // hide 20 of its rows and add one no base row gives
    let extra = vec![
        CqlValue::Text("f".repeat(64)),
        CqlValue::Text("views".to_owned()),
        CqlValue::Timestamp(scylla::value::CqlTimestamp(1_600_000_000_000)),
        CqlValue::Uuid(Uuid::new_v4()),
        CqlValue::Int(2026),
        CqlValue::Int(0),
    ];
    let session = make_stale(&session, &ks, &samples, 20, &[extra], &[]).await?;
    let expected = expected_keys(&session, &ks, &samples).await?;
    let held = view_keys(&session, &ks, &samples).await?;
    assert_eq!(expected.len(), 500);
    assert_eq!(held.len(), 481, "the view wasn't made stale");
    let hidden = expected.difference(&held).count();
    assert_eq!(hidden, 20);
    // a read prepared the way the API reads the view misses a hidden row
    let missing = rows(&session, &format!("SELECT sha256 FROM {ks}.samples_list"))
        .await?
        .into_iter()
        .filter_map(|row| match row.columns.into_iter().next().flatten() {
            Some(CqlValue::Text(sha)) => Some(sha),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    let mut absent = None;
    let reader = session
        .prepare(format!(
            "SELECT sha256, group, id FROM {ks}.samples WHERE sha256 = ?"
        ))
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    for sha in &missing {
        let found = session
            .execute_unpaged(&reader, (sha,))
            .await
            .map_err(|err| Error::new(err.to_string()))?
            .into_rows_result()
            .map_err(|err| Error::new(err.to_string()))?
            .rows_num();
        if found == 0 {
            absent = Some(sha.clone());
            break;
        }
    }
    let absent = absent.expect("a hidden row's sha256");
    // recreate it, keeping the progress it reports
    let lines = Lines::default();
    let report = recreate_views(&session, &conf, &[samples], false, None, &|line| {
        lines.push(line);
    })
    .await
    .map_err(|err| Error::new(err.to_string()))?;
    assert_eq!(
        report,
        RecreateReport {
            statements: recreate_statements(&ks, &samples).to_vec(),
            recreated: vec!["samples".to_owned()],
            dry_run: false,
        }
    );
    // the warning came before the recreate
    assert!(
        lines
            .all()
            .iter()
            .any(|line| line == &recreate_warning(&samples)),
        "{:?}",
        lines.all()
    );
    // without waiting any further, the view holds exactly the base table's rows
    assert_eq!(view_keys(&session, &ks, &samples).await?, expected);
    // the read prepared before the recreate works and finds the hidden row
    let found = session
        .execute_unpaged(&reader, (&absent,))
        .await
        .map_err(|err| Error::new(err.to_string()))?
        .into_rows_result()
        .map_err(|err| Error::new(err.to_string()))?
        .rows_num();
    assert_eq!(found, 1);
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {ks}")).await?;
    Ok(())
}

/// A dry run returns the statements without dropping anything; a real run replaces the view, and a
/// declared view dropped by hand is created again
#[tokio::test]
async fn a_dry_run_drops_nothing_and_a_missing_view_is_created() -> Result<(), Error> {
    // a converged keyspace with one declared view dropped by hand
    let _guard = RESTART.read().await;
    let session = test_utilities::scylla_session().await?;
    let conf = applied(&session, "views_dry").await?;
    let ks = conf.thorium.namespace.clone();
    exec(
        &session,
        &format!("DROP MATERIALIZED VIEW {ks}.comments_by_id"),
    )
    .await?;
    let views =
        resolve_views(&["samples", "comments_by_id"]).map_err(|err| Error::new(err.to_string()))?;
    // a dry run lists both views' statements and changes nothing
    let before = object_id(&session, &ks, "samples").await?;
    let report = recreate_views(&session, &conf, &views, true, None, &|_| ())
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    let statements = views
        .iter()
        .flat_map(|view| recreate_statements(&ks, view))
        .collect::<Vec<_>>();
    assert_eq!(report.statements, statements);
    assert_eq!(report.recreated, ["samples", "comments_by_id"]);
    assert!(report.dry_run);
    assert_eq!(object_id(&session, &ks, "samples").await?, before);
    assert_eq!(object_id(&session, &ks, "comments_by_id").await?, None);
    // a real run replaces the existing view and creates the missing one
    let report = recreate_views(&session, &conf, &views, false, None, &|_| ())
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    assert_eq!(report.recreated, ["samples", "comments_by_id"]);
    let after = object_id(&session, &ks, "samples").await?;
    assert!(after.is_some());
    assert_ne!(after, before);
    assert!(object_id(&session, &ks, "comments_by_id").await?.is_some());
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {ks}")).await?;
    Ok(())
}

/// Write sample rows to `samples_list` many at a time, for tests that need a base table whose
/// view takes a while to build
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace to write to
/// * `count` - How many rows to write
async fn seed_many_samples(session: &Session, keyspace: &str, count: u32) -> Result<(), Error> {
    use futures::StreamExt;
    // prepare the insert once for every row
    let cql = format!(
        "INSERT INTO {keyspace}.samples_list (group, year, bucket, uploaded, id, sha256, name) \
         VALUES (?, ?, ?, ?, ?, ?, ?)"
    );
    let insert = session
        .prepare(cql.as_str())
        .await
        .map_err(|err| Error::new(format!("{cql}: {err}")))?;
    // write each row with its own sha256, spread over 64 buckets, a few hundred in flight
    let failed = futures::stream::iter(0..count)
        .map(|i| {
            let values = (
                "views",
                2026_i32,
                i32::try_from(i % 64).expect("small bucket"),
                scylla::value::CqlTimestamp(1_700_000_000_000 + i64::from(i)),
                Uuid::new_v4(),
                format!("{i:064x}"),
                format!("sample-{i}"),
            );
            let insert = &insert;
            async move { session.execute_unpaged(insert, values).await.map(|_| ()) }
        })
        .buffer_unordered(256)
        .filter_map(|written| async move { written.err() })
        .collect::<Vec<_>>()
        .await;
    // name the first failed write, if any
    match failed.first() {
        Some(err) => Err(Error::new(format!("{cql}: {err}"))),
        None => Ok(()),
    }
}

/// Write a `system.built_views` row for a view, standing in for the row Scylla keeps for a
/// dropped view until its view builder handles the drop
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The view's keyspace
/// * `name` - The view
async fn plant_built_row(session: &Session, keyspace: &str, name: &str) -> Result<(), Error> {
    // a one-node Scylla takes writes to its local list of built views
    session
        .query_unpaged(
            "INSERT INTO system.built_views (keyspace_name, view_name) VALUES (?, ?)",
            (keyspace, name),
        )
        .await
        .map_err(|err| Error::new(format!("planting a built_views row: {err}")))?;
    Ok(())
}

/// Delete a view's `system.built_views` row, as Scylla's view builder does once it handles the
/// view's drop
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The view's keyspace
/// * `name` - The view
async fn clear_built_row(session: &Session, keyspace: &str, name: &str) -> Result<(), Error> {
    // delete the one row
    session
        .query_unpaged(
            "DELETE FROM system.built_views WHERE keyspace_name = ? AND view_name = ?",
            (keyspace, name),
        )
        .await
        .map_err(|err| Error::new(format!("clearing a built_views row: {err}")))?;
    Ok(())
}

/// Wait until Scylla has handled a view's drop and cleared its `system.built_views` row
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The view's keyspace
/// * `name` - The dropped view
async fn drop_handled(session: &Session, keyspace: &str, name: &str) -> Result<(), Error> {
    // poll for up to a minute
    let cql = format!(
        "SELECT view_name FROM system.built_views WHERE keyspace_name = '{keyspace}' AND \
         view_name = '{name}'"
    );
    let started = Instant::now();
    while !rows(session, &cql).await?.is_empty() {
        if started.elapsed() > Duration::from_secs(60) {
            return Err(Error::new(format!("the drop of {name} was never handled")));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(())
}

/// A recreate doesn't take what a dropped view left in `system.built_views` as the new view's
/// build: it creates the view only once that row is gone and returns with the view fully built,
/// and a row that outlasts the wait's limit fails it, naming the view as missing
///
/// After `DROP MATERIALIZED VIEW` returns, Scylla keeps the dropped view's row until its view
/// builder handles the drop, which takes seconds while the builder is busy. A planted row that the
/// test clears later stands in for it on a single node; a cluster's `view_build_status` rows are
/// waited for the same way but can't be planted (Scylla 6.2 doesn't keep a row written there).
#[tokio::test]
async fn a_recreate_waits_out_the_dropped_views_built_row() -> Result<(), Error> {
    // a converged keyspace with sample rows and its samples view dropped by hand
    let _guard = RESTART.read().await;
    let session = test_utilities::scylla_session().await?;
    let peers = rows(&session, "SELECT peer FROM system.peers").await?.len();
    if peers > 0 {
        eprintln!("skipped: a built_views row can only stand in for the build state on one node");
        return Ok(());
    }
    let conf = applied(&session, "views_stale_row").await?;
    let ks = conf.thorium.namespace.clone();
    let samples = view("samples");
    seed_many_samples(&session, &ks, 50_000).await?;
    let expected = expected_keys(&session, &ks, &samples).await?;
    assert_eq!(expected.len(), 50_000);
    exec(&session, &format!("DROP MATERIALIZED VIEW {ks}.samples")).await?;
    // let Scylla clear the dropped view's own row before planting one in its place
    drop_handled(&session, &ks, "samples").await?;
    // a row Scylla clears a few seconds into the recreate holds the create back until then
    plant_built_row(&session, &ks, "samples").await?;
    let recreate = [samples];
    let (held, cleared) = tokio::join!(
        async {
            recreate_views(
                &session,
                &conf,
                &recreate,
                false,
                Some(Duration::from_secs(600)),
                &|_| (),
            )
            .await
            .map_err(|err| Error::new(err.to_string()))?;
            // read the view the moment the recreate returns, without waiting any further
            view_keys(&session, &ks, &samples).await
        },
        async {
            tokio::time::sleep(Duration::from_secs(3)).await;
            clear_built_row(&session, &ks, "samples").await
        }
    );
    cleared?;
    // the view holds every base row once the recreate returns
    assert_eq!(
        held?.len(),
        expected.len(),
        "the recreate returned while the view was still building"
    );
    // a row that outlasts the limit fails the recreate before the create, so the view is missing
    exec(&session, &format!("DROP MATERIALIZED VIEW {ks}.samples")).await?;
    drop_handled(&session, &ks, "samples").await?;
    plant_built_row(&session, &ks, "samples").await?;
    let expired = recreate_views(
        &session,
        &conf,
        &[samples],
        false,
        Some(Duration::from_secs(2)),
        &|_| (),
    )
    .await;
    clear_built_row(&session, &ks, "samples").await?;
    match expired {
        Err(ViewError::NotCreated {
            view,
            error: ApplyError::DropUnfinished { views, .. },
        }) => {
            assert_eq!(view, "samples");
            assert_eq!(views, ["samples"]);
        }
        other => panic!("expected the dropped view's row to fail the recreate: {other:?}"),
    }
    assert_eq!(object_id(&session, &ks, "samples").await?, None);
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {ks}")).await?;
    Ok(())
}

/// A `view_build_status` row left by a host that is no longer a cluster member doesn't hold a
/// built view back, and a member without a `SUCCESS` row is named as the one the view waits on
///
/// Scylla 6.2 doesn't keep a row written straight into `system_distributed.view_build_status`, so
/// the former member's row is added to the rows read back from the live table.
#[tokio::test]
async fn a_former_members_view_build_row_is_ignored() -> Result<(), Error> {
    // a converged keyspace, whose views every member has built
    let _guard = RESTART.read().await;
    let session = test_utilities::scylla_session().await?;
    let conf = applied(&session, "views_former_member").await?;
    let ks = conf.thorium.namespace.to_lowercase();
    // the members the driver knows, each with a host id the rows are keyed by
    let hosts = cluster_hosts(&session);
    assert!(!hosts.is_empty());
    // read the view's rows until every member reports it built, which may trail the apply
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut rows = loop {
        let rows = view_build_statuses(&session, &ks, "samples")
            .await
            .map_err(|err| Error::new(err.to_string()))?;
        if member_build(&hosts, &rows).waiting.is_empty() || Instant::now() > deadline {
            break rows;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    let live = member_build(&hosts, &rows);
    assert!(live.waiting.is_empty(), "{live:?} from {rows:?}");
    assert_eq!(live.built.len(), hosts.len());
    // add the row a removed node leaves when it never finished the build
    let former = Uuid::new_v4().to_string();
    rows.push(HostStatus {
        host_id: former.clone(),
        status: "STARTED".to_owned(),
    });
    // every row counted would never finish, but only the members' rows decide
    assert!(!rows.iter().all(|row| row.status == "SUCCESS"));
    let build = member_build(&hosts, &rows);
    assert!(build.waiting.is_empty(), "{build:?}");
    assert_eq!(build.built.len(), hosts.len());
    assert_eq!(build.ignored, std::slice::from_ref(&former));
    // with only the former member's row, every member is named as one the view waits on
    let only_former = rows
        .iter()
        .filter(|row| row.host_id == former)
        .cloned()
        .collect::<Vec<_>>();
    let waiting = member_build(&hosts, &only_former);
    assert_eq!(waiting.built, Vec::<String>::new());
    assert_eq!(waiting.waiting, hosts.values().cloned().collect::<Vec<_>>());
    for (id, name) in &hosts {
        assert!(name.contains(id.as_str()), "{name} names {id}");
    }
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {ks}")).await?;
    Ok(())
}

/// A view whose base table is missing is refused before any view is dropped
#[tokio::test]
async fn a_view_without_its_base_table_is_refused() -> Result<(), Error> {
    // a converged keyspace whose s3_ids table and its view were dropped by hand
    let _guard = RESTART.read().await;
    let session = test_utilities::scylla_session().await?;
    let conf = applied(&session, "views_nobase").await?;
    let ks = conf.thorium.namespace.clone();
    exec(&session, &format!("DROP MATERIALIZED VIEW {ks}.s3_sha256s")).await?;
    exec(&session, &format!("DROP TABLE {ks}.s3_ids")).await?;
    // recreating it beside a healthy view refuses, naming the table, and drops nothing
    let before = object_id(&session, &ks, "samples").await?;
    let views =
        resolve_views(&["samples", "s3_sha256s"]).map_err(|err| Error::new(err.to_string()))?;
    let refused = recreate_views(&session, &conf, &views, false, None, &|_| ()).await;
    assert!(
        matches!(&refused, Err(ViewError::MissingBase { view, base }) if view == "s3_sha256s" && base == "s3_ids"),
        "{refused:?}"
    );
    assert_eq!(object_id(&session, &ks, "samples").await?, before);
    assert_eq!(object_id(&session, &ks, "s3_sha256s").await?, None);
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {ks}")).await?;
    Ok(())
}

/// A view whose name a table holds is refused before any view is dropped, naming the view
#[tokio::test]
async fn a_view_name_held_by_a_table_is_refused_before_any_drop() -> Result<(), Error> {
    // a converged keyspace whose results_auth view was replaced by a table of that name
    let _guard = RESTART.read().await;
    let session = test_utilities::scylla_session().await?;
    let conf = applied(&session, "views_held").await?;
    let ks = conf.thorium.namespace.clone();
    exec(
        &session,
        &format!("DROP MATERIALIZED VIEW {ks}.results_auth"),
    )
    .await?;
    exec(
        &session,
        &format!("CREATE TABLE {ks}.results_auth (id uuid PRIMARY KEY)"),
    )
    .await?;
    // recreating it beside a healthy view refuses, naming the view, and drops nothing
    let before = object_id(&session, &ks, "samples").await?;
    let views =
        resolve_views(&["samples", "results_auth"]).map_err(|err| Error::new(err.to_string()))?;
    let refused = recreate_views(&session, &conf, &views, false, None, &|_| ()).await;
    assert!(
        matches!(&refused, Err(ViewError::Apply(ApplyError::ViewHeldByTable { views })) if views == &["results_auth"]),
        "{refused:?}"
    );
    assert_eq!(object_id(&session, &ks, "samples").await?, before);
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {ks}")).await?;
    Ok(())
}

/// The index of the range of the token ring holding a token, computed independently of the code
/// under test: 256 equal ranges, the first starting at the smallest token
///
/// # Arguments
///
/// * `token` - The token
fn expected_range(token: i64) -> u32 {
    // each range spans 2^56 tokens
    u32::try_from((i128::from(token) - i128::from(i64::MIN)) >> 56).expect("a range index")
}

/// The first and last token of a range, computed independently of the code under test
///
/// # Arguments
///
/// * `index` - The range's index
fn expected_bounds(index: u32) -> (i64, i64) {
    // each range spans 2^56 tokens from the smallest token
    let start = i128::from(i64::MIN) + (i128::from(index) << 56);
    let end = start + (1_i128 << 56) - 1;
    (
        i64::try_from(start).expect("a token"),
        i64::try_from(end).expect("a token"),
    )
}

/// Read each row's token and view key from a query selecting a token and then the view's key
/// columns
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `cql` - The query
async fn tokens_and_keys(session: &Session, cql: &str) -> Result<Vec<(i64, String)>, Error> {
    // split each row into its token and the rest
    Ok(rows(session, cql)
        .await?
        .into_iter()
        .map(|row| {
            let mut columns = row.columns.into_iter();
            let Some(Some(CqlValue::BigInt(token))) = columns.next() else {
                panic!("the first column is a token");
            };
            (token, format!("{:?}", columns.collect::<Vec<_>>()))
        })
        .collect())
}

/// Write network policies: three the default view takes, three it rejects, and two with the
/// filtered column unset
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace to write to
async fn seed_policies(session: &Session, keyspace: &str) -> Result<(), Error> {
    // each policy in its own group, with the default flag set, cleared, or unset
    for (i, default) in [
        Some(true),
        Some(true),
        Some(true),
        Some(false),
        Some(false),
        Some(false),
        None,
        None,
    ]
    .into_iter()
    .enumerate()
    {
        session
            .query_unpaged(
                format!(
                    "INSERT INTO {keyspace}.network_policies (group, name, id, default_policy) \
                     VALUES (?, ?, ?, ?)"
                ),
                (
                    format!("group-{i}"),
                    format!("policy-{i}"),
                    Uuid::new_v4(),
                    default,
                ),
            )
            .await
            .map_err(|err| Error::new(err.to_string()))?;
    }
    Ok(())
}

/// Write `s3_ids` rows, each with its own sha256
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace to write to
/// * `count` - How many rows to write
async fn seed_s3_ids(session: &Session, keyspace: &str, count: u32) -> Result<(), Error> {
    // each row names a file object by id
    for i in 0..count {
        session
            .query_unpaged(
                format!("INSERT INTO {keyspace}.s3_ids (type, id, sha256) VALUES (?, ?, ?)"),
                (
                    "files",
                    Uuid::new_v4(),
                    format!("{:064x}", u64::from(i) + 1_000_000),
                ),
            )
            .await
            .map_err(|err| Error::new(err.to_string()))?;
    }
    Ok(())
}

/// Write `samples_list` rows without a sha256, which the `samples` view excludes
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace to write to
/// * `count` - How many rows to write
async fn seed_unhashed_samples(session: &Session, keyspace: &str, count: u32) -> Result<(), Error> {
    // the rows have every key column but no sha256
    for i in 0..count {
        session
            .query_unpaged(
                format!(
                    "INSERT INTO {keyspace}.samples_list (group, year, bucket, uploaded, id, name) \
                     VALUES (?, ?, ?, ?, ?, ?)"
                ),
                (
                    "unhashed",
                    2026_i32,
                    0_i32,
                    scylla::value::CqlTimestamp(1_650_000_000_000 + i64::from(i)),
                    Uuid::new_v4(),
                    format!("unhashed-{i}"),
                ),
            )
            .await
            .map_err(|err| Error::new(err.to_string()))?;
    }
    Ok(())
}

/// Write `samples_list` rows that set some non-key columns to empty strings and leave others unset,
/// half of them with an empty-string sha256, which the `samples` view keeps
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace to write to
/// * `count` - How many rows to write
async fn seed_sparse_samples(session: &Session, keyspace: &str, count: u32) -> Result<(), Error> {
    // empty strings and unset values in non-key columns, and empty sha256s in every other row
    for i in 0..count {
        let sha = if i % 2 == 0 {
            String::new()
        } else {
            format!("{:064x}", u64::from(i) + 2_000_000)
        };
        session
            .query_unpaged(
                format!(
                    "INSERT INTO {keyspace}.samples_list (group, year, bucket, uploaded, id, \
                     sha256, description, md5) VALUES (?, ?, ?, ?, ?, ?, ?, ?)"
                ),
                (
                    "sparse",
                    2026_i32,
                    i32::try_from(i % 4).expect("small bucket"),
                    scylla::value::CqlTimestamp(1_660_000_000_000 + i64::from(i)),
                    Uuid::new_v4(),
                    sha,
                    "",
                    (i % 3 == 0).then(|| format!("md5-{i}")),
                ),
            )
            .await
            .map_err(|err| Error::new(err.to_string()))?;
    }
    Ok(())
}

/// Write `tags` rows, whose views have composite partition keys
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace to write to
/// * `count` - How many rows to write
async fn seed_tags(session: &Session, keyspace: &str, count: u32) -> Result<(), Error> {
    // each tag on its own item, spread over a few groups and buckets
    for i in 0..count {
        session
            .query_unpaged(
                format!(
                    "INSERT INTO {keyspace}.tags (type, group, year, bucket, key, value, uploaded, \
                     item, key_lower, value_lower) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
                ),
                (
                    "Files",
                    format!("group-{}", i % 3),
                    2026_i32,
                    i32::try_from(i % 5).expect("small bucket"),
                    format!("Key{}", i % 4),
                    format!("Value{i}"),
                    scylla::value::CqlTimestamp(1_670_000_000_000 + i64::from(i)),
                    format!("item-{i}"),
                    format!("key{}", i % 4),
                    format!("value{i}"),
                ),
            )
            .await
            .map_err(|err| Error::new(err.to_string()))?;
    }
    Ok(())
}

/// Write `results_stream` rows: some with a tool, which `results_auth` takes, and some without,
/// which it leaves out; non-key columns are set, empty, or unset
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace to write to
/// * `with_tool` - How many rows name a tool
/// * `without_tool` - How many rows leave the tool unset
async fn seed_results(
    session: &Session,
    keyspace: &str,
    with_tool: u32,
    without_tool: u32,
) -> Result<(), Error> {
    // composite partition keys, with the tool and the version varying per row
    for i in 0..with_tool + without_tool {
        session
            .query_unpaged(
                format!(
                    "INSERT INTO {keyspace}.results_stream (kind, group, year, bucket, uploaded, \
                     id, key, tool, tool_version, cmd) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
                ),
                (
                    "Files",
                    format!("group-{}", i % 2),
                    2026_i32,
                    i32::try_from(i % 3).expect("small bucket"),
                    scylla::value::CqlTimestamp(1_680_000_000_000 + i64::from(i)),
                    Uuid::new_v4(),
                    format!("{:064x}", u64::from(i) + 3_000_000),
                    (i < with_tool).then(|| format!("tool-{}", i % 4)),
                    if i % 2 == 0 { "" } else { "1.0" },
                    (i % 3 == 0).then(|| "run --fast".to_owned()),
                ),
            )
            .await
            .map_err(|err| Error::new(err.to_string()))?;
    }
    Ok(())
}

/// Check views, turning an error into a test error
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `conf` - The config naming the keyspace
/// * `view` - The view to check, or every declared view
/// * `ranges` - The ranges to check
async fn verify(
    session: &Session,
    conf: &Conf,
    view: Option<&str>,
    ranges: RangeSelection,
) -> Result<VerifyReport, Error> {
    // check with the options given, ignoring progress
    let options = VerifyOptions {
        view: view.map(ToOwned::to_owned),
        ranges,
    };
    verify_views(session, conf, &options, &|_| ())
        .await
        .map_err(|err| Error::new(format!("verify-views failed: {err}")))
}

/// Get one view's result from a report
///
/// # Arguments
///
/// * `report` - The report
/// * `view` - The view
fn check<'a>(report: &'a VerifyReport, view: &str) -> &'a ViewCheck {
    // every view checked is in the report
    report
        .views
        .iter()
        .find(|check| check.view == view)
        .unwrap_or_else(|| panic!("{view} was checked"))
}

/// The ranges are 256 equal slices of the token ring, the first starting at the smallest token
/// and the last ending at the largest
#[test]
fn token_ranges_cut_the_ring_evenly() {
    // the bounds and the index agree with an independent computation
    assert_eq!(TOKEN_RANGES, 256);
    assert_eq!(token_range(0).0, i64::MIN);
    assert_eq!(token_range(TOKEN_RANGES - 1).1, i64::MAX);
    for index in [0, 1, 127, 128, 200, 255] {
        assert_eq!(token_range(index), expected_bounds(index));
        let (start, end) = token_range(index);
        assert_eq!(range_of(start), index);
        assert_eq!(range_of(end), index);
    }
    for token in [i64::MIN, -1, 0, 1, 12_345_678_901, i64::MAX] {
        assert_eq!(range_of(token), expected_range(token));
    }
}

/// Views in step with their base tables report in step in the full and the sampled modes, with
/// composite partition keys, empty-string keys and values, and unset non-key values compared as
/// they are, and a base row a view excludes (an unset key column, or a row its filter rejects)
/// isn't counted as missing; an undeclared view and an unusable range selection are refused
#[tokio::test]
async fn views_in_step_report_in_step_honouring_their_filters() -> Result<(), Error> {
    // a keyspace with rows in several views' base tables, some of which the views exclude
    let _guard = RESTART.read().await;
    let session = test_utilities::scylla_session().await?;
    let conf = applied(&session, "verify_in_step").await?;
    let ks = conf.thorium.namespace.clone();
    seed_samples(&session, &ks, 200).await?;
    seed_unhashed_samples(&session, &ks, 20).await?;
    seed_policies(&session, &ks).await?;
    seed_s3_ids(&session, &ks, 30).await?;
    seed_sparse_samples(&session, &ks, 20).await?;
    seed_tags(&session, &ks, 25).await?;
    seed_results(&session, &ks, 12, 6).await?;
    for name in [
        "samples",
        "network_policies_default",
        "network_policies_by_name",
        "s3_sha256s",
        "tags_by_item",
        "tags_case_insensitive",
        "results_auth",
        "results_ids",
    ] {
        settled(&session, &ks, &view(name)).await?;
    }
    // the full mode reports every declared view in step over every range
    let report = verify(&session, &conf, None, RangeSelection::All).await?;
    assert!(!report.sampled);
    assert_eq!(report.views.len(), declared_views().len());
    assert!(
        report.out_of_step().is_empty(),
        "{:?}",
        report.out_of_step()
    );
    let samples = check(&report, "samples");
    assert_eq!(samples.ranges, (0..TOKEN_RANGES).collect::<Vec<_>>());
    assert_eq!((samples.base_rows, samples.view_rows), (220, 220));
    let default = check(&report, "network_policies_default");
    assert_eq!((default.base_rows, default.view_rows), (3, 3));
    let by_name = check(&report, "network_policies_by_name");
    assert_eq!((by_name.base_rows, by_name.view_rows), (8, 8));
    for (name, held) in [
        ("tags_by_item", 25),
        ("tags_case_insensitive", 25),
        ("results_auth", 12),
        ("results_ids", 18),
    ] {
        let check = check(&report, name);
        assert_eq!(
            (check.base_rows, check.view_rows),
            (held, held),
            "{check:?}"
        );
    }
    // sampling every range finds every view in step too, the filtered ones included
    let report = verify(&session, &conf, None, RangeSelection::Sample(TOKEN_RANGES)).await?;
    assert!(
        report.out_of_step().is_empty(),
        "{:?}",
        report.out_of_step()
    );
    // the sampled mode checks the requested number of distinct ranges and finds them in step
    let report = verify(&session, &conf, None, RangeSelection::Sample(8)).await?;
    assert!(report.sampled);
    assert!(
        report.out_of_step().is_empty(),
        "{:?}",
        report.out_of_step()
    );
    for check in &report.views {
        assert_eq!(check.ranges.len(), 8, "{check:?}");
        assert!(
            check.ranges.windows(2).all(|pair| pair[0] < pair[1]),
            "{check:?}"
        );
        assert!(check.ranges.iter().all(|index| *index < TOKEN_RANGES));
    }
    // one view can be checked alone
    let report = verify(&session, &conf, Some("samples"), RangeSelection::All).await?;
    assert_eq!(report.views.len(), 1);
    assert_eq!(report.views[0].state, ViewState::InStep);
    // undeclared views and unusable ranges are refused
    for name in ["extra_by_name", "samples_list"] {
        let options = VerifyOptions {
            view: Some(name.to_owned()),
            ranges: RangeSelection::All,
        };
        let refused = verify_views(&session, &conf, &options, &|_| ()).await;
        assert!(
            matches!(refused, Err(ViewError::Undeclared { .. })),
            "{refused:?}"
        );
    }
    for ranges in [
        RangeSelection::Sample(0),
        RangeSelection::Ranges(Vec::new()),
        RangeSelection::Ranges(vec![TOKEN_RANGES]),
    ] {
        let options = VerifyOptions { view: None, ranges };
        let refused = verify_views(&session, &conf, &options, &|_| ()).await;
        assert!(
            matches!(refused, Err(ViewError::BadRanges { .. })),
            "{refused:?}"
        );
    }
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {ks}")).await?;
    Ok(())
}

/// A declared view that doesn't exist reports missing, and counts as out of step
#[tokio::test]
async fn a_missing_view_reports_missing() -> Result<(), Error> {
    // a converged keyspace with one view dropped by hand
    let _guard = RESTART.read().await;
    let session = test_utilities::scylla_session().await?;
    let conf = applied(&session, "verify_missing").await?;
    let ks = conf.thorium.namespace.clone();
    exec(
        &session,
        &format!("DROP MATERIALIZED VIEW {ks}.comments_by_id"),
    )
    .await?;
    // the full check names it missing and the rest in step
    let report = verify(&session, &conf, None, RangeSelection::All).await?;
    let out = report.out_of_step();
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].view, "comments_by_id");
    assert_eq!(out[0].state, ViewState::Missing);
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {ks}")).await?;
    Ok(())
}

/// A view with rows hidden from it, and one with a row added that no base row gives, report out
/// of step with the first differing range in the full mode; the sampled mode finds each only in
/// the ranges that hold the difference; recreating both brings them back in step
#[tokio::test]
#[ignore = "restarts the test Scylla through docker, which a CI job cannot do; run by hand against a disposable Scylla container: THORIUM_TEST_SCYLLA_CONTAINER=<container> cargo test -p thorium-api --features test-utilities --test schema_views -- --ignored"]
#[allow(clippy::too_many_lines)]
async fn stale_views_report_the_first_differing_range_until_recreated() -> Result<(), Error> {
    // a keyspace with sample and object rows, their views caught up
    let _guard = RESTART.write().await;
    let session = test_utilities::scylla_session().await?;
    let conf = applied(&session, "verify_stale").await?;
    let ks = conf.thorium.namespace.clone();
    let samples = view("samples");
    let s3 = view("s3_sha256s");
    seed_samples(&session, &ks, 300).await?;
    seed_s3_ids(&session, &ks, 40).await?;
    settled(&session, &ks, &samples).await?;
    settled(&session, &ks, &s3).await?;
    // hide 15 sample rows, and add an object row no base row gives
    let session = make_stale(&session, &ks, &samples, 15, &[], &[]).await?;
    let (extra_type, extra_id, extra_sha) = ("files", Uuid::new_v4(), "e".repeat(64));
    let extra = vec![
        CqlValue::Text(extra_sha.clone()),
        CqlValue::Text(extra_type.to_owned()),
        CqlValue::Uuid(extra_id),
    ];
    let session = make_stale(&session, &ks, &s3, 0, &[extra], &[]).await?;
    // work out where the samples view differs: the lowest range holding a hidden row
    let held = view_keys(&session, &ks, &samples).await?;
    let base = tokens_and_keys(
        &session,
        &format!(
            "SELECT token(group, year, bucket), {} FROM {ks}.samples_list",
            key_columns(&samples).join(", ")
        ),
    )
    .await?;
    let hidden = base
        .iter()
        .filter(|(_, key)| !held.contains(key))
        .map(|(token, _)| expected_range(*token))
        .collect::<Vec<_>>();
    assert_eq!(hidden.len(), 15, "the view wasn't made stale");
    let first = *hidden.iter().min().expect("a hidden row");
    let in_first = base
        .iter()
        .filter(|(token, _)| expected_range(*token) == first)
        .count() as u64;
    let hidden_in_first = hidden.iter().filter(|index| **index == first).count() as u64;
    let clean = base
        .iter()
        .map(|(token, _)| expected_range(*token))
        .find(|index| !hidden.contains(index))
        .expect("a range with no hidden row");
    // and where the object view differs: the range of the extra row's base token
    let base_token = session
        .prepare(format!(
            "SELECT type FROM {ks}.s3_ids WHERE type = ? AND id = ?"
        ))
        .await
        .map_err(|err| Error::new(err.to_string()))?
        .calculate_token(&(extra_type, extra_id))
        .map_err(|err| Error::new(err.to_string()))?
        .expect("a token")
        .value();
    let extra_base_range = expected_range(base_token);
    let objects = tokens_and_keys(
        &session,
        &format!("SELECT token(type, id), sha256 FROM {ks}.s3_ids"),
    )
    .await?;
    let objects_in_range = objects
        .iter()
        .filter(|(token, _)| expected_range(*token) == extra_base_range)
        .count() as u64;
    // the full mode reports both views out of step at those ranges, and every other in step
    let report = verify(&session, &conf, None, RangeSelection::All).await?;
    let (start, end) = expected_bounds(first);
    assert_eq!(
        check(&report, "samples").state,
        ViewState::OutOfStep {
            first: RangeDifference {
                side: RangeSide::Base,
                index: first,
                start,
                end,
                base_rows: in_first,
                view_rows: in_first - hidden_in_first,
            }
        }
    );
    let (start, end) = expected_bounds(extra_base_range);
    assert_eq!(
        check(&report, "s3_sha256s").state,
        ViewState::OutOfStep {
            first: RangeDifference {
                side: RangeSide::Base,
                index: extra_base_range,
                start,
                end,
                base_rows: objects_in_range,
                view_rows: objects_in_range + 1,
            }
        }
    );
    assert_eq!(report.out_of_step().len(), 2, "{:?}", report.out_of_step());
    // the sampled mode finds the hidden rows only in a range that holds one
    let report = verify(
        &session,
        &conf,
        Some("samples"),
        RangeSelection::Ranges(vec![first]),
    )
    .await?;
    assert_eq!(report.views[0].ranges, [first]);
    assert!(
        matches!(&report.views[0].state, ViewState::OutOfStep { first: difference } if difference.side == RangeSide::Base && difference.index == first && difference.base_rows - difference.view_rows == hidden_in_first),
        "{:?}",
        report.views[0]
    );
    let report = verify(
        &session,
        &conf,
        Some("samples"),
        RangeSelection::Ranges(vec![clean]),
    )
    .await?;
    assert_eq!(report.views[0].state, ViewState::InStep);
    // and the extra row only in the range of the view's own tokens that holds it
    let view_token = session
        .prepare(format!(
            "SELECT sha256 FROM {ks}.s3_sha256s WHERE sha256 = ?"
        ))
        .await
        .map_err(|err| Error::new(err.to_string()))?
        .calculate_token(&(extra_sha.as_str(),))
        .map_err(|err| Error::new(err.to_string()))?
        .expect("a token")
        .value();
    let extra_view_range = expected_range(view_token);
    let view_rows = tokens_and_keys(
        &session,
        &format!("SELECT token(sha256), sha256 FROM {ks}.s3_sha256s"),
    )
    .await?
    .iter()
    .filter(|(token, _)| expected_range(*token) == extra_view_range)
    .count() as u64;
    let report = verify(
        &session,
        &conf,
        Some("s3_sha256s"),
        RangeSelection::Ranges(vec![extra_view_range]),
    )
    .await?;
    let (start, end) = expected_bounds(extra_view_range);
    assert_eq!(
        report.views[0].state,
        ViewState::OutOfStep {
            first: RangeDifference {
                side: RangeSide::View,
                index: extra_view_range,
                start,
                end,
                base_rows: view_rows - 1,
                view_rows,
            }
        }
    );
    let elsewhere = (0..TOKEN_RANGES)
        .find(|index| *index != extra_view_range && *index != extra_base_range)
        .expect("another range");
    let report = verify(
        &session,
        &conf,
        Some("s3_sha256s"),
        RangeSelection::Ranges(vec![elsewhere]),
    )
    .await?;
    assert_eq!(report.views[0].state, ViewState::InStep);
    // recreating both views brings every view back in step
    recreate_views(&session, &conf, &[samples, s3], false, None, &|_| ())
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    let report = verify(&session, &conf, None, RangeSelection::All).await?;
    assert!(
        report.out_of_step().is_empty(),
        "{:?}",
        report.out_of_step()
    );
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {ks}")).await?;
    Ok(())
}

/// The token of a row's partition key, computed by the driver for a read restricting it
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `cql` - A read whose bind markers are the partition key columns, in key order
/// * `values` - The partition key values
async fn token_of(session: &Session, cql: &str, values: &[CqlValue]) -> Result<i64, Error> {
    // prepare the read so the driver knows the partition key, then hash the values
    Ok(session
        .prepare(cql)
        .await
        .map_err(|err| Error::new(format!("{cql}: {err}")))?
        .calculate_token(&values)
        .map_err(|err| Error::new(format!("{cql}: {err}")))?
        .expect("a token")
        .value())
}

/// A view row whose key matches its base row but whose non-key value differs (a missed update of
/// `name` in one row and of `md5` in another) reports out of step in the full mode and when every
/// range is sampled, at the first range holding a changed row; recreating it brings it back in
/// step
#[tokio::test]
#[ignore = "restarts the test Scylla through docker, which a CI job cannot do; run by hand against a disposable Scylla container: THORIUM_TEST_SCYLLA_CONTAINER=<container> cargo test -p thorium-api --features test-utilities --test schema_views -- --ignored"]
#[allow(clippy::too_many_lines)]
async fn a_wrong_value_in_a_view_row_reports_out_of_step() -> Result<(), Error> {
    // a keyspace with sample rows, their view caught up
    let _guard = RESTART.write().await;
    let session = test_utilities::scylla_session().await?;
    let conf = applied(&session, "verify_wrong").await?;
    let ks = conf.thorium.namespace.clone();
    let samples = view("samples");
    seed_samples(&session, &ks, 300).await?;
    settled(&session, &ks, &samples).await?;
    // give one view row a wrong name and another a wrong md5, keeping both keys
    let keys = key_columns(&samples);
    let changed = rows(
        &session,
        &format!("SELECT {} FROM {ks}.samples LIMIT 2", keys.join(", ")),
    )
    .await?
    .into_iter()
    .map(|row| {
        row.columns
            .into_iter()
            .map(Option::unwrap)
            .collect::<Vec<_>>()
    })
    .collect::<Vec<_>>();
    assert_eq!(changed.len(), 2);
    let wrong = CqlValue::Text("WRONG".to_owned());
    let overwrite = [
        (changed[0].clone(), "name", wrong.clone()),
        (changed[1].clone(), "md5", wrong.clone()),
    ];
    let session = make_stale(&session, &ks, &samples, 0, &[], &overwrite).await?;
    // the view holds every key, but the two rows read the wrong values
    assert_eq!(
        view_keys(&session, &ks, &samples).await?,
        expected_keys(&session, &ks, &samples).await?
    );
    let wrong_rows = rows(&session, &format!("SELECT name, md5 FROM {ks}.samples"))
        .await?
        .into_iter()
        .filter(|row| {
            row.columns
                .iter()
                .any(|column| column.as_ref() == Some(&wrong))
        })
        .count();
    assert_eq!(wrong_rows, 2, "the view wasn't given wrong values");
    // work out each changed row's range of the base table's tokens and of the view's own
    let at = |name: &str| {
        keys.iter()
            .position(|key| *key == name)
            .expect("a key column")
    };
    let (group, year, bucket, sha) = (at("group"), at("year"), at("bucket"), at("sha256"));
    let mut base_ranges = Vec::new();
    let mut view_ranges = Vec::new();
    for key in &changed {
        let base_token = token_of(
            &session,
            &format!(
                "SELECT group FROM {ks}.samples_list WHERE group = ? AND year = ? AND bucket = ?"
            ),
            &[key[group].clone(), key[year].clone(), key[bucket].clone()],
        )
        .await?;
        base_ranges.push(expected_range(base_token));
        let view_token = token_of(
            &session,
            &format!("SELECT sha256 FROM {ks}.samples WHERE sha256 = ?"),
            &[key[sha].clone()],
        )
        .await?;
        view_ranges.push(expected_range(view_token));
    }
    let base_tokens = tokens_and_keys(
        &session,
        &format!("SELECT token(group, year, bucket), sha256 FROM {ks}.samples_list"),
    )
    .await?;
    let view_tokens = tokens_and_keys(
        &session,
        &format!("SELECT token(sha256), sha256 FROM {ks}.samples"),
    )
    .await?;
    let in_range = |tokens: &[(i64, String)], index: u32| {
        tokens
            .iter()
            .filter(|(token, _)| expected_range(*token) == index)
            .count() as u64
    };
    // the full mode reports the lowest base range holding a changed row, with equal counts
    let first = *base_ranges.iter().min().expect("a changed row");
    let (start, end) = expected_bounds(first);
    let rows_in_first = in_range(&base_tokens, first);
    let report = verify(&session, &conf, Some("samples"), RangeSelection::All).await?;
    assert_eq!(
        report.views[0].state,
        ViewState::OutOfStep {
            first: RangeDifference {
                side: RangeSide::Base,
                index: first,
                start,
                end,
                base_rows: rows_in_first,
                view_rows: rows_in_first,
            }
        }
    );
    // sampling every range reports the first range, in order, where a lookup finds other values:
    // the base side of a range is checked before its view side
    let changed_in =
        |ranges: &[u32], index: u32| ranges.iter().filter(|at| **at == index).count() as u64;
    let base_first = *base_ranges.iter().min().expect("a changed row");
    let view_first = *view_ranges.iter().min().expect("a changed row");
    let expected = if base_first <= view_first {
        let (start, end) = expected_bounds(base_first);
        let held = in_range(&base_tokens, base_first);
        RangeDifference {
            side: RangeSide::Base,
            index: base_first,
            start,
            end,
            base_rows: held,
            view_rows: held - changed_in(&base_ranges, base_first),
        }
    } else {
        let (start, end) = expected_bounds(view_first);
        let held = in_range(&view_tokens, view_first);
        RangeDifference {
            side: RangeSide::View,
            index: view_first,
            start,
            end,
            base_rows: held - changed_in(&view_ranges, view_first),
            view_rows: held,
        }
    };
    let report = verify(
        &session,
        &conf,
        Some("samples"),
        RangeSelection::Sample(TOKEN_RANGES),
    )
    .await?;
    assert_eq!(
        report.views[0].state,
        ViewState::OutOfStep { first: expected }
    );
    // recreating the view brings it back in step in both modes
    recreate_views(&session, &conf, &[samples], false, None, &|_| ())
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    for ranges in [RangeSelection::All, RangeSelection::Sample(TOKEN_RANGES)] {
        let report = verify(&session, &conf, Some("samples"), ranges).await?;
        assert_eq!(report.views[0].state, ViewState::InStep, "{report:?}");
    }
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {ks}")).await?;
    Ok(())
}

/// Apply the declared schema to a test's keyspace, keeping the progress it reports
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `conf` - The config naming the test's keyspace
/// * `view_wait` - The longest to wait for view builds, or `None` to wait as long as they take
/// * `lines` - Receives each progress line
async fn apply_declared(
    session: &Session,
    conf: &Conf,
    view_wait: Option<Duration>,
    lines: &Lines,
) -> Result<ApplyReport, ApplyError> {
    // converge to this build's registry like `thoradm schema apply`
    let options = ApplyOptions {
        target: ApplyTarget::Declared,
        dry_run: false,
        view_wait,
    };
    apply(session, conf, &options, "", &|line| lines.push(line)).await
}

/// The base table Scylla records for a view, if the view exists
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace holding the view
/// * `view` - The view
async fn view_base(session: &Session, keyspace: &str, view: &str) -> Result<Option<String>, Error> {
    // read the view's base table from the schema tables
    let cql =
        "SELECT base_table_name FROM system_schema.views WHERE keyspace_name = ? AND view_name = ?";
    Ok(session
        .query_unpaged(cql, (keyspace, view))
        .await
        .map_err(|err| Error::new(format!("{cql}: {err}")))?
        .into_rows_result()
        .map_err(|err| Error::new(format!("{cql}: {err}")))?
        .maybe_first_row::<(String,)>()
        .map_err(|err| Error::new(format!("{cql}: {err}")))?
        .map(|(base,)| base))
}

/// Count the rows a table or view holds
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace holding the table or view
/// * `name` - The table or view
async fn count_rows(session: &Session, keyspace: &str, name: &str) -> Result<usize, Error> {
    // page through every row rather than trusting a count over a large table to finish in time
    Ok(rows(session, &format!("SELECT * FROM {keyspace}.{name}"))
        .await?
        .len())
}

/// Write commit rows to `commitish_list`, each backed by one of a few repo data blobs
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace to write to
/// * `count` - How many rows to write
async fn seed_commitishes(session: &Session, keyspace: &str, count: u32) -> Result<(), Error> {
    // write each commit with its own key, spread over three blobs
    let cql = format!(
        "INSERT INTO {keyspace}.commitish_list (kind, group, year, bucket, repo, timestamp, key, \
         repo_data) VALUES ('Commit', 'views', 2026, 1, 'github.com/views/repo', ?, ?, ?)"
    );
    for i in 0..count {
        let values = (
            scylla::value::CqlTimestamp(1_700_000_000_000 + i64::from(i)),
            format!("c{i}"),
            format!("blob{}", i % 3),
        );
        session
            .query_unpaged(cql.as_str(), values)
            .await
            .map_err(|err| Error::new(format!("{cql}: {err}")))?;
    }
    Ok(())
}

/// Replace a keyspace's `committed_repo_data` view with the one internal builds before April 2024
/// created, which selects from the `commits_list` table nothing writes any more
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace whose view is replaced
async fn old_committed_repo_data(session: &Session, keyspace: &str) -> Result<(), Error> {
    // drop the declared view, then lay down the old table and the view built on it
    for cql in [
        format!("DROP MATERIALIZED VIEW IF EXISTS {keyspace}.committed_repo_data"),
        format!(
            "CREATE TABLE IF NOT EXISTS {keyspace}.commits_list (group TEXT, year INT, bucket INT, \
             repo TEXT, timestamp TIMESTAMP, hash TEXT, repo_data TEXT, PRIMARY KEY ((group, \
             year, bucket, repo), timestamp, hash)) WITH CLUSTERING ORDER BY (timestamp DESC, \
             hash DESC)"
        ),
        format!(
            "CREATE MATERIALIZED VIEW IF NOT EXISTS {keyspace}.committed_repo_data AS SELECT \
             group, repo, repo_data, hash FROM {keyspace}.commits_list WHERE group IS NOT NULL \
             AND repo IS NOT NULL AND repo_data IS NOT NULL AND hash IS NOT NULL AND year IS NOT \
             NULL AND bucket IS NOT NULL AND timestamp IS NOT NULL PRIMARY KEY ((repo_data, year, \
             bucket), group, repo, hash, timestamp)"
        ),
    ] {
        exec(session, &cql).await?;
    }
    Ok(())
}

/// A keyspace an old release created, whose `committed_repo_data` still selects from
/// `commits_list`, gets the declared view back from `schema apply`: the view is dropped and
/// created on `commitish_list`, the reason is logged, and the view holds every commit row once
/// the apply returns
#[tokio::test]
async fn an_old_committed_repo_data_view_is_recreated_on_its_base() -> Result<(), Error> {
    // a converged keyspace with commits, whose view is swapped for the old one
    let _guard = RESTART.read().await;
    let session = test_utilities::scylla_session().await?;
    let conf = applied(&session, "views_old_repo_data").await?;
    let ks = conf.thorium.namespace.clone();
    seed_commitishes(&session, &ks, 300).await?;
    old_committed_repo_data(&session, &ks).await?;
    assert_eq!(
        view_base(&session, &ks, "committed_repo_data")
            .await?
            .as_deref(),
        Some("commits_list")
    );
    assert_eq!(count_rows(&session, &ks, "committed_repo_data").await?, 0);
    // apply recreates the view, saying how it differed
    let lines = Lines::default();
    let report = apply_declared(&session, &conf, None, &lines)
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    let names = report
        .views_recreated
        .iter()
        .map(|stale| stale.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, ["committed_repo_data"], "{report:?}");
    let differences = &report.views_recreated[0].differences;
    assert!(
        differences
            .iter()
            .any(|line| line == "base: commits_list -> commitish_list"),
        "{differences:?}"
    );
    assert!(
        report
            .views_built
            .iter()
            .any(|view| view == "committed_repo_data"),
        "{report:?}"
    );
    assert!(
        report.statements.iter().any(|statement| statement
            == &format!("DROP MATERIALIZED VIEW IF EXISTS {ks}.committed_repo_data")),
        "{:?}",
        report.statements
    );
    assert!(
        lines
            .all()
            .iter()
            .any(|line| line.starts_with("view `committed_repo_data` differs")),
        "{:?}",
        lines.all()
    );
    // the view is on its declared base and already holds every commit
    assert_eq!(
        view_base(&session, &ks, "committed_repo_data")
            .await?
            .as_deref(),
        Some("commitish_list")
    );
    assert_eq!(count_rows(&session, &ks, "committed_repo_data").await?, 300);
    assert!(
        views_built(&session, &ks, &["committed_repo_data"])
            .await
            .map_err(|err| Error::new(err.to_string()))?
    );
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {ks}")).await?;
    Ok(())
}

/// The `samples` view's creating statement with a filter the registry doesn't declare, which
/// keeps only the rows of bucket 0
///
/// Scylla 6 refuses the `IS NOT NULL` filters on non-key columns that views created on Scylla 5
/// carry, so the tests stand in this filter, which hides rows the same way.
///
/// # Arguments
///
/// * `keyspace` - The keyspace the view is in
fn filtered_samples(keyspace: &str) -> String {
    // add the restriction ahead of the view's primary key
    let declared = view("samples").create_cql(keyspace);
    let filtered = declared.replacen(" PRIMARY KEY", " AND bucket = 0 PRIMARY KEY", 1);
    assert_ne!(filtered, declared, "the filter was added");
    filtered
}

/// A `samples` view created with a filter the registry doesn't declare, as older releases'
/// views were, hides rows; `schema apply` recreates it from the registry and it holds every sample
/// again
#[tokio::test]
async fn a_samples_view_with_an_old_filter_is_recreated() -> Result<(), Error> {
    // a converged keyspace with samples spread over 64 buckets
    let _guard = RESTART.read().await;
    let session = test_utilities::scylla_session().await?;
    let conf = applied(&session, "views_old_filter").await?;
    let ks = conf.thorium.namespace.clone();
    seed_samples(&session, &ks, 200).await?;
    // swap the view for one that keeps only bucket 0, which hides most rows
    exec(&session, &format!("DROP MATERIALIZED VIEW {ks}.samples")).await?;
    // the dropped view's built row would otherwise pass for the filtered view's build
    drop_handled(&session, &ks, "samples").await?;
    exec(&session, &filtered_samples(&ks)).await?;
    settled_build(&session, &ks).await?;
    assert_eq!(count_rows(&session, &ks, "samples").await?, 4);
    // apply recreates it, naming the filter as the difference
    let lines = Lines::default();
    let report = apply_declared(&session, &conf, None, &lines)
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    assert_eq!(report.views_recreated.len(), 1, "{report:?}");
    assert_eq!(report.views_recreated[0].name, "samples");
    assert!(
        report.views_recreated[0]
            .differences
            .iter()
            .any(|line| line.starts_with("filter: ")),
        "{report:?}"
    );
    // every sample is in the view once the apply returns
    assert_eq!(count_rows(&session, &ks, "samples").await?, 200);
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {ks}")).await?;
    Ok(())
}

/// Applying again to a converged keyspace recreates no view and issues no statement
#[tokio::test]
async fn a_converged_keyspace_recreates_no_view() -> Result<(), Error> {
    // a converged keyspace, applied a second time
    let _guard = RESTART.read().await;
    let session = test_utilities::scylla_session().await?;
    let conf = applied(&session, "views_converged").await?;
    let ks = conf.thorium.namespace.clone();
    let before = object_id(&session, &ks, "committed_repo_data").await?;
    let report = apply_declared(&session, &conf, None, &Lines::default())
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    // nothing differs, so nothing is dropped or created
    assert!(report.views_recreated.is_empty(), "{report:?}");
    assert!(report.statements.is_empty(), "{report:?}");
    assert!(report.views_built.is_empty(), "{report:?}");
    assert_eq!(
        object_id(&session, &ks, "committed_repo_data").await?,
        before
    );
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {ks}")).await?;
    Ok(())
}

/// A view the registry doesn't declare is never compared, dropped, or recreated by `schema
/// apply`, even on a base table whose declared view is recreated in the same run
#[tokio::test]
async fn apply_leaves_an_undeclared_view_alone() -> Result<(), Error> {
    // a converged keyspace with a hand-made view on samples_list and a stale samples view
    let _guard = RESTART.read().await;
    let session = test_utilities::scylla_session().await?;
    let conf = applied(&session, "views_undecl_apply").await?;
    let ks = conf.thorium.namespace.clone();
    exec(
        &session,
        &format!(
            "CREATE MATERIALIZED VIEW {ks}.extra_by_name AS SELECT name, group, year, bucket, \
             uploaded, id FROM {ks}.samples_list WHERE name IS NOT NULL AND group IS NOT NULL AND \
             year IS NOT NULL AND bucket IS NOT NULL AND uploaded IS NOT NULL AND id IS NOT NULL \
             PRIMARY KEY ((name), group, year, bucket, uploaded, id)"
        ),
    )
    .await?;
    exec(&session, &format!("DROP MATERIALIZED VIEW {ks}.samples")).await?;
    exec(&session, &filtered_samples(&ks)).await?;
    let extra = object_id(&session, &ks, "extra_by_name").await?;
    assert!(extra.is_some());
    // apply recreates only the declared view
    let report = apply_declared(&session, &conf, None, &Lines::default())
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    let names = report
        .views_recreated
        .iter()
        .map(|stale| stale.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, ["samples"], "{report:?}");
    assert!(
        !report
            .statements
            .iter()
            .any(|statement| statement.contains("extra_by_name")),
        "{:?}",
        report.statements
    );
    assert_eq!(object_id(&session, &ks, "extra_by_name").await?, extra);
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {ks}")).await?;
    Ok(())
}

/// A recreated view still building when the view wait runs out fails the apply while Scylla keeps
/// building it, and the next apply waits for that build before returning
#[tokio::test]
async fn a_view_build_past_the_view_wait_fails_and_the_retry_waits_for_it() -> Result<(), Error> {
    // a converged keyspace with enough samples that rebuilding the view takes a moment
    let _guard = RESTART.read().await;
    let session = test_utilities::scylla_session().await?;
    let conf = applied(&session, "views_wait_expired").await?;
    let ks = conf.thorium.namespace.clone();
    seed_many_samples(&session, &ks, 40_000).await?;
    exec(&session, &format!("DROP MATERIALIZED VIEW {ks}.samples")).await?;
    // the dropped view's built row would otherwise pass for the filtered view's build
    drop_handled(&session, &ks, "samples").await?;
    exec(&session, &filtered_samples(&ks)).await?;
    settled_build(&session, &ks).await?;
    // an apply that waits no time at all drops the view, then fails: on its build when Scylla
    // cleared the dropped view's build state at once, else before creating it again
    let failed = apply_declared(&session, &conf, Some(Duration::ZERO), &Lines::default()).await;
    assert!(
        matches!(&failed, Err(ApplyError::ViewWaitExpired { stuck, .. })
            if stuck.len() == 1 && stuck[0].view == "samples")
            || matches!(&failed, Err(ApplyError::DropUnfinished { views, .. }) if views == &["samples"]),
        "{failed:?}"
    );
    // a drop that outlasted the wait leaves the view missing once Scylla clears its built row
    if matches!(&failed, Err(ApplyError::DropUnfinished { .. })) {
        drop_handled(&session, &ks, "samples").await?;
    }
    assert!(
        !views_built(&session, &ks, &["samples"])
            .await
            .map_err(|err| Error::new(err.to_string()))?
    );
    // the retry finds the definition right (or the view missing), and waits for its build
    let report = apply_declared(&session, &conf, None, &Lines::default())
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    assert!(report.views_recreated.is_empty(), "{report:?}");
    assert!(
        views_built(&session, &ks, &["samples"])
            .await
            .map_err(|err| Error::new(err.to_string()))?
    );
    assert_eq!(count_rows(&session, &ks, "samples").await?, 40_000);
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {ks}")).await?;
    Ok(())
}

/// Wait until the keyspace's `samples` view has finished its own build, so a test starts from a
/// built view
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace holding the view
async fn settled_build(session: &Session, keyspace: &str) -> Result<(), Error> {
    // poll for up to ten minutes, since builds run slowly while other tests keep the builder busy
    let started = Instant::now();
    while !views_built(session, keyspace, &["samples"])
        .await
        .map_err(|err| Error::new(err.to_string()))?
    {
        if started.elapsed() > Duration::from_secs(600) {
            return Err(Error::new("samples never finished building"));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Ok(())
}
