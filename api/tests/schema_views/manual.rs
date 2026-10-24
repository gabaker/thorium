//! The node control the stale-view tests need, which only a developer or an agent can give them
//!
//! These helpers run `docker` against the test Scylla's container to restart it, so they are used
//! only by `#[ignore]`d tests of the manual tier and live in this file, which the CI preflight
//! does not scan.

use super::{exec, key_columns, object_id, rows};
use scylla::client::session::Session;
use scylla::value::CqlValue;
use std::process::Command;
use std::time::{Duration, Instant};
use thorium::Error;
use thorium::models::schema::{TableDef, ViewDef};
use thorium::test_utilities::CONF;
use uuid::Uuid;

/// The environment variable naming the test Scylla's Docker container
const CONTAINER_ENV: &str = "THORIUM_TEST_SCYLLA_CONTAINER";

/// Run a command in the test Scylla's container
///
/// # Arguments
///
/// * `args` - The `docker` arguments
pub(super) fn docker(args: &[&str]) {
    // fail the test with the command's output when it fails
    let output = Command::new("docker")
        .args(args)
        .output()
        .expect("docker runs");
    assert!(
        output.status.success(),
        "docker {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Open a session once Scylla answers queries again
pub(super) async fn reconnect() -> Result<Session, Error> {
    // retry for up to three minutes while the node starts
    let started = Instant::now();
    loop {
        if let Ok(session) = thorium::models::schema::connect(&CONF).await
            && session
                .query_unpaged("SELECT now() FROM system.local", &())
                .await
                .is_ok()
        {
            return Ok(session);
        }
        if started.elapsed() > Duration::from_secs(180) {
            return Err(Error::new("Scylla didn't come back after the restart"));
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Make a view out of step with its base table: hide some of its rows behind tombstones and add
/// rows no base row gives it, by copying a scratch table's sstables into the view's data directory
/// while Scylla restarts; returns a session opened after the restart
///
/// The caller must hold [`RESTART`] for writing.
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace the view is in
/// * `view` - The view to make stale
/// * `hide` - How many of the view's rows to hide
/// * `add` - Rows of the view's key columns to add
/// * `overwrite` - View rows, by their key columns, whose named non-key column is given a value
///   their base row doesn't hold
pub(super) async fn make_stale(
    session: &Session,
    keyspace: &str,
    view: &ViewDef,
    hide: usize,
    add: &[Vec<CqlValue>],
    overwrite: &[(Vec<CqlValue>, &str, CqlValue)],
) -> Result<Session, Error> {
    // the container comes from the environment
    let container = std::env::var(CONTAINER_ENV).unwrap_or_else(|_| {
        panic!("set {CONTAINER_ENV} to the test Scylla's Docker container to build a stale view")
    });
    // a scratch table with the view's columns and key, so its sstables read as the view's
    let scratch: &'static str = Box::leak(format!("stale_{}", view.name).into_boxed_str());
    let table = TableDef {
        name: scratch,
        columns: view.columns,
        ttl: None,
        partition_size: None,
    };
    exec(
        session,
        &format!("DROP TABLE IF EXISTS {keyspace}.{scratch}"),
    )
    .await?;
    exec(session, &table.create_cql(keyspace, None)).await?;
    // write a tombstone for each row to hide, newer than the row itself
    let keys = key_columns(view);
    let restrict = keys
        .iter()
        .map(|column| format!("{column} = ?"))
        .collect::<Vec<_>>()
        .join(" AND ");
    let delete = format!("DELETE FROM {keyspace}.{scratch} WHERE {restrict}");
    let read = format!(
        "SELECT {} FROM {keyspace}.{} LIMIT {hide}",
        keys.join(", "),
        view.name
    );
    // Scylla reads `LIMIT 0` as no limit, so nothing is read when nothing is hidden
    let shown = if hide == 0 {
        Vec::new()
    } else {
        rows(session, &read).await?
    };
    for row in shown {
        let values = row
            .columns
            .into_iter()
            .map(Option::unwrap)
            .collect::<Vec<_>>();
        session
            .query_unpaged(delete.as_str(), values)
            .await
            .map_err(|err| Error::new(format!("{delete}: {err}")))?;
    }
    // write each row to add
    let insert = format!(
        "INSERT INTO {keyspace}.{scratch} ({}) VALUES ({})",
        keys.join(", "),
        vec!["?"; keys.len()].join(", ")
    );
    for values in add {
        session
            .query_unpaged(insert.as_str(), values.clone())
            .await
            .map_err(|err| Error::new(format!("{insert}: {err}")))?;
    }
    // write each wrong value, newer than the view's own
    for (key, column, value) in overwrite {
        let update = format!("UPDATE {keyspace}.{scratch} SET {column} = ? WHERE {restrict}");
        let mut values = vec![value.clone()];
        values.extend(key.iter().cloned());
        session
            .query_unpaged(update.as_str(), values)
            .await
            .map_err(|err| Error::new(format!("{update}: {err}")))?;
    }
    // flush the scratch table, and copy its sstables into the view's directory
    docker(&["exec", &container, "nodetool", "flush", keyspace, scratch]);
    let scratch_id = object_id(session, keyspace, scratch)
        .await?
        .expect("scratch id");
    let view_id = object_id(session, keyspace, view.name)
        .await?
        .expect("view id");
    let dir =
        |name: &str, id: Uuid| format!("/var/lib/scylla/data/{keyspace}/{name}-{}", id.simple());
    let copy = format!(
        "cp {}/*-big-* {}/",
        dir(scratch, scratch_id),
        dir(view.name, view_id)
    );
    docker(&["exec", &container, "sh", "-c", &copy]);
    // restart Scylla so it loads the copied sstables, then drop the scratch table
    docker(&["restart", "-t", "60", &container]);
    let session = reconnect().await?;
    exec(
        &session,
        &format!("DROP TABLE IF EXISTS {keyspace}.{scratch}"),
    )
    .await?;
    Ok(session)
}
