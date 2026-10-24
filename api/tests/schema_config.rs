//! Tests that `schema apply` records the settings that shape stored data at the first apply, and
//! refuses a later change to one while its data exists

use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHasher, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use chrono::{DateTime, Datelike, SecondsFormat, Utc};
use scylla::client::session::Session;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::Mutex;
use thorium::models::schema::{
    ApplyError, ApplyOptions, ApplyReport, ApplyTarget, ConfigAccept, DATA_TYPES, FILES_PASSWORD,
    apply, apply_accepting, verify_files_password,
};
use thorium::test_utilities::{self, CONF};
use thorium::utils::helpers::partition;
use thorium::{Conf, Error};

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

/// Read every row of a query of text columns
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `cql` - The query, selecting only text columns
async fn texts<const N: usize>(session: &Session, cql: &str) -> Result<Vec<[String; N]>, Error> {
    // run the query and take its rows
    let rows = session
        .query_unpaged(cql, &())
        .await
        .map_err(|err| Error::new(format!("{cql}: {err}")))?
        .into_rows_result()
        .map_err(|err| Error::new(format!("{cql}: {err}")))?;
    // read each row's columns as text, an unset column as empty
    let mut out = Vec::new();
    for row in rows
        .rows::<scylla::value::Row>()
        .map_err(|err| Error::new(err.to_string()))?
    {
        let row = row.map_err(|err| Error::new(err.to_string()))?;
        let mut values = row.columns.into_iter().map(|value| match value {
            Some(scylla::value::CqlValue::Text(text)) => text,
            Some(other) => format!("{other:?}"),
            None => String::new(),
        });
        out.push(std::array::from_fn(|_| values.next().unwrap_or_default()));
    }
    Ok(out)
}

/// Build a config for a keyspace beside the run's own namespace, dropping any keyspace an earlier
/// run left there
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `suffix` - What makes this test's keyspace name unique
async fn fresh(session: &Session, suffix: &str) -> Result<Conf, Error> {
    // replace any keyspace an earlier run left behind
    let keyspace = format!("{}_{suffix}", CONF.thorium.namespace);
    exec(session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await?;
    // point a copy of the test config at it
    let mut conf = CONF.clone();
    conf.thorium.namespace = keyspace;
    Ok(conf)
}

/// Apply the declared schema, ignoring progress
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `conf` - The config naming the keyspace
async fn apply_declared(session: &Session, conf: &Conf) -> Result<ApplyReport, Error> {
    // converge and record, turning a refusal into a test error
    let options = ApplyOptions {
        target: ApplyTarget::Declared,
        dry_run: false,
    };
    apply(session, conf, &options, "key-config", &|_| ())
        .await
        .map_err(|err| Error::new(format!("schema apply failed: {err:?}")))
}

/// The write times a query of one `WRITETIME` column returns, in microseconds since the epoch
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `cql` - The query, selecting only a `WRITETIME` column
async fn write_times(session: &Session, cql: &str) -> Result<Vec<i64>, Error> {
    // run the query and take every row's time
    let rows = session
        .query_unpaged(cql, &())
        .await
        .map_err(|err| Error::new(format!("{cql}: {err}")))?
        .into_rows_result()
        .map_err(|err| Error::new(format!("{cql}: {err}")))?;
    let mut times = Vec::new();
    for row in rows
        .rows::<(Option<i64>,)>()
        .map_err(|err| Error::new(err.to_string()))?
    {
        let (time,) = row.map_err(|err| Error::new(err.to_string()))?;
        times.push(time.unwrap_or_default());
    }
    Ok(times)
}

/// Every config row of the record, by id
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace to read
async fn config_rows(session: &Session, keyspace: &str) -> Result<BTreeMap<String, String>, Error> {
    // read the config partition of the record
    let rows = texts::<2>(
        session,
        &format!("SELECT id, value FROM {keyspace}.schema_migrations WHERE kind = 'config'"),
    )
    .await?;
    Ok(rows.into_iter().map(|[id, value]| (id, value)).collect())
}

/// The first `schema apply` records the effective partition size of every data type (tags per tag
/// type), every `earliest` value, and the files password's identity as an argon2id PHC string; the
/// password and its unsalted sha256 appear nowhere in the table
#[tokio::test]
async fn the_first_apply_records_every_data_shaping_setting() -> Result<(), Error> {
    // apply with a password of this test's own and some sizes changed from the defaults
    let session = test_utilities::scylla_session().await?;
    let mut conf = fresh(&session, "config_first").await?;
    let keyspace = conf.thorium.namespace.clone();
    let password = "l4c-corn-on-the-cob";
    conf.thorium.files.password = password.to_owned();
    conf.thorium.files.partition_size = 3600;
    conf.thorium.tags.repos.partition_size = 900;
    conf.thorium.associations.earliest = 1_700_000_000;
    apply_declared(&session, &conf).await?;
    // each data type's size and earliest value is recorded as configured
    let rows = config_rows(&session, &keyspace).await?;
    let sizes = [
        ("files", conf.thorium.files.partition_size),
        ("repos", conf.thorium.repos.partition_size),
        ("results", conf.thorium.results.partition_size),
        ("entities", conf.thorium.entities.partition_size),
        ("associations", conf.thorium.associations.partition_size),
        ("tags.files", conf.thorium.tags.files.partition_size),
        ("tags.repos", conf.thorium.tags.repos.partition_size),
        ("tags.entities", conf.thorium.tags.entities.partition_size),
    ];
    let earliest = [
        ("files", conf.thorium.files.earliest),
        ("repos", conf.thorium.repos.earliest),
        ("results", conf.thorium.results.earliest),
        ("entities", conf.thorium.entities.earliest),
        ("associations", conf.thorium.associations.earliest),
        ("tags.files", conf.thorium.tags.files.earliest),
        ("tags.repos", conf.thorium.tags.repos.earliest),
        ("tags.entities", conf.thorium.tags.entities.earliest),
    ];
    assert_eq!(sizes.len(), DATA_TYPES.len());
    for (data_type, size) in sizes {
        assert_eq!(
            rows.get(&format!("partition_size.{data_type}")),
            Some(&size.to_string()),
            "{rows:?}"
        );
    }
    for (data_type, at) in earliest {
        assert_eq!(
            rows.get(&format!("earliest.{data_type}")),
            Some(&at.to_string()),
            "{rows:?}"
        );
    }
    // the password is recorded only as its argon2id identity, which verifies against it
    let identity = rows.get(FILES_PASSWORD).expect("the identity is recorded");
    assert!(
        identity.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"),
        "{identity}"
    );
    assert!(verify_files_password(identity, password).expect("the identity is accepted"));
    assert_eq!(rows.len(), 2 * DATA_TYPES.len() + 1, "{rows:?}");
    // every config row is written before the deployment row, so a recorded fingerprint implies
    // a full record
    let config_written = write_times(
        &session,
        &format!("SELECT WRITETIME(value) FROM {keyspace}.schema_migrations WHERE kind = 'config'"),
    )
    .await?;
    let deployment_written = write_times(
        &session,
        &format!(
            "SELECT WRITETIME(fingerprint) FROM {keyspace}.schema_migrations \
             WHERE kind = 'backend' AND id = 'deployment'"
        ),
    )
    .await?;
    assert_eq!(config_written.len(), rows.len());
    assert_eq!(deployment_written.len(), 1);
    assert!(
        config_written
            .iter()
            .all(|written| *written < deployment_written[0]),
        "{config_written:?} vs {deployment_written:?}"
    );
    // neither the password nor its unsalted sha256 appears in any row of the table
    let sha = hex::encode(Sha256::digest(password.as_bytes()));
    let all = texts::<1>(
        &session,
        &format!("SELECT JSON * FROM {keyspace}.schema_migrations"),
    )
    .await?;
    assert_ne!(all.len(), 0);
    for [row] in all {
        let lower = row.to_lowercase();
        assert!(!row.contains(password), "a row holds the password: {row}");
        assert!(
            !lower.contains(&sha),
            "a row holds the password's sha256: {row}"
        );
    }
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}

/// A second apply with the same settings keeps the recorded identity: the configured password
/// re-verifies against it, so no new hash is written
#[tokio::test]
async fn a_second_apply_keeps_the_recorded_identity() -> Result<(), Error> {
    // apply twice with the same config
    let session = test_utilities::scylla_session().await?;
    let mut conf = fresh(&session, "config_again").await?;
    let keyspace = conf.thorium.namespace.clone();
    conf.thorium.files.password = "l4c-same-password".to_owned();
    apply_declared(&session, &conf).await?;
    let first = config_rows(&session, &keyspace).await?;
    apply_declared(&session, &conf).await?;
    let second = config_rows(&session, &keyspace).await?;
    // every recorded value, the identity included, is unchanged
    assert!(first.contains_key(FILES_PASSWORD), "{first:?}");
    assert_eq!(first.len(), 2 * DATA_TYPES.len() + 1, "{first:?}");
    assert_eq!(first, second);
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}

/// With the default files password configured, `schema apply` reports a warning naming the
/// setting; with another password it reports none
#[tokio::test]
async fn the_default_files_password_is_warned_about() -> Result<(), Error> {
    // the test config uses the public default
    let session = test_utilities::scylla_session().await?;
    let mut conf = fresh(&session, "config_default").await?;
    let keyspace = conf.thorium.namespace.clone();
    conf.thorium.files.password = thorium::conf::DEFAULT_FILES_PASSWORD.to_owned();
    let report = apply_declared(&session, &conf).await?;
    let warned = report
        .warnings
        .iter()
        .filter(|warning| warning.contains("thorium.files.password"))
        .count();
    assert_eq!(warned, 1, "{:?}", report.warnings);
    assert!(
        report
            .warnings
            .iter()
            .all(|warning| !warning.contains(thorium::conf::DEFAULT_FILES_PASSWORD)),
        "{:?}",
        report.warnings
    );
    // another keyspace with its own password is not warned about
    let mut other = fresh(&session, "config_custom").await?;
    other.thorium.files.password = "l4c-not-the-default".to_owned();
    let report = apply_declared(&session, &other).await?;
    assert!(
        report
            .warnings
            .iter()
            .all(|warning| !warning.contains("thorium.files.password")),
        "{:?}",
        report.warnings
    );
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await?;
    exec(
        &session,
        &format!("DROP KEYSPACE IF EXISTS {}", other.thorium.namespace),
    )
    .await
}

/// Apply the declared schema, accepting what `accept` names, and return the refusal
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `conf` - The config naming the keyspace
/// * `accept` - What the apply records instead of refusing
async fn apply_refused(
    session: &Session,
    conf: &Conf,
    accept: &ConfigAccept,
) -> Result<ApplyError, Error> {
    // a successful apply fails the test
    let options = ApplyOptions {
        target: ApplyTarget::Declared,
        dry_run: false,
    };
    match apply_accepting(session, conf, &options, accept, "key-config", &|_| ()).await {
        Ok(report) => Err(Error::new(format!(
            "schema apply wasn't refused: {report:?}"
        ))),
        Err(err) => Ok(err),
    }
}

/// Apply the declared schema, accepting what `accept` names
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `conf` - The config naming the keyspace
/// * `accept` - What the apply records instead of refusing
async fn apply_accepted(
    session: &Session,
    conf: &Conf,
    accept: &ConfigAccept,
) -> Result<ApplyReport, Error> {
    // converge and record, turning a refusal into a test error
    let options = ApplyOptions {
        target: ApplyTarget::Declared,
        dry_run: false,
    };
    apply_accepting(session, conf, &options, accept, "key-config", &|_| ())
        .await
        .map_err(|err| Error::new(format!("schema apply failed: {err:?}")))
}

/// A timestamp some days and seconds into 2026, as CQL text and as a time
///
/// # Arguments
///
/// * `days` - The day of the year, from 0
/// * `seconds` - The seconds into that day
fn at(days: i64, seconds: i64) -> DateTime<Utc> {
    // count from the start of the year
    let start = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
        .expect("the start of the year parses")
        .with_timezone(&Utc);
    start + chrono::Duration::days(days) + chrono::Duration::seconds(seconds)
}

/// Insert a submission's key columns into `samples_list`, bucketed with a partition size
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace to write to
/// * `uploaded` - When the submission was uploaded
/// * `size` - The partition size its bucket is computed with
async fn seed_sample(
    session: &Session,
    keyspace: &str,
    uploaded: DateTime<Utc>,
    size: u16,
) -> Result<(), Error> {
    // only the primary key, which is all the checks read
    let bucket = partition(uploaded, uploaded.year(), size);
    exec(
        session,
        &format!(
            "INSERT INTO {keyspace}.samples_list (group, year, bucket, uploaded, id) \
             VALUES ('g', {}, {bucket}, '{}', uuid())",
            uploaded.year(),
            uploaded.to_rfc3339()
        ),
    )
    .await
}

/// Insert a repo submission's key columns into `repos_list`, bucketed with a partition size
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace to write to
/// * `uploaded` - When the repo was submitted
/// * `size` - The partition size its bucket is computed with
async fn seed_repo(
    session: &Session,
    keyspace: &str,
    uploaded: DateTime<Utc>,
    size: u16,
) -> Result<(), Error> {
    // only the primary key, which is all the checks read
    let bucket = partition(uploaded, uploaded.year(), size);
    exec(
        session,
        &format!(
            "INSERT INTO {keyspace}.repos_list (group, year, bucket, uploaded, id) \
             VALUES ('g', {}, {bucket}, '{}', uuid())",
            uploaded.year(),
            uploaded.to_rfc3339()
        ),
    )
    .await
}

/// Insert a tag's key columns into `tags`, bucketed with a partition size
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace to write to
/// * `tag_type` - The stored tag type, such as `Repos`
/// * `uploaded` - The earliest time the group saw the item
/// * `size` - The partition size its bucket is computed with
async fn seed_tag(
    session: &Session,
    keyspace: &str,
    tag_type: &str,
    uploaded: DateTime<Utc>,
    size: u16,
) -> Result<(), Error> {
    // only the primary key, which is all the checks read
    let bucket = partition(uploaded, uploaded.year(), size);
    exec(
        session,
        &format!(
            "INSERT INTO {keyspace}.tags (type, group, year, bucket, key, value, uploaded, item, \
             key_lower, value_lower) VALUES ('{tag_type}', 'g', {}, {bucket}, 'k', 'v', '{}', \
             'item', 'k', 'v')",
            uploaded.year(),
            uploaded.to_rfc3339()
        ),
    )
    .await
}

/// Every row of the record, as `SELECT JSON` text, to show that a refusal changes none
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace to read
async fn record_rows(session: &Session, keyspace: &str) -> Result<Vec<String>, Error> {
    // the full rows, with when each value was written
    let rows = texts::<1>(
        session,
        &format!(
            "SELECT JSON kind, id, value, fingerprint, apply_key, writetime(updated) \
             FROM {keyspace}.schema_migrations"
        ),
    )
    .await?;
    let mut rows = rows.into_iter().map(|[row]| row).collect::<Vec<_>>();
    rows.sort();
    Ok(rows)
}

/// Whether the keyspace holds a table
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace to look in
/// * `table` - The table's name
async fn has_table(session: &Session, keyspace: &str, table: &str) -> Result<bool, Error> {
    // look the table up in the schema tables
    let found = texts::<1>(
        session,
        &format!(
            "SELECT table_name FROM system_schema.tables WHERE keyspace_name = '{keyspace}' \
             AND table_name = '{table}'"
        ),
    )
    .await?;
    Ok(!found.is_empty())
}

/// With data of the type present, a changed partition size makes `schema apply` refuse, naming
/// the setting with its recorded and configured values, issuing no DDL and changing no row; with
/// no data of the type, the new value is recorded; several refused settings are all named
#[tokio::test]
async fn a_changed_setting_is_refused_while_its_data_exists() -> Result<(), Error> {
    // record the settings, then store a file and a repo written at the recorded sizes
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "config_changed").await?;
    let keyspace = conf.thorium.namespace.clone();
    apply_declared(&session, &conf).await?;
    let files = conf.thorium.files.partition_size;
    let repos = conf.thorium.repos.partition_size;
    seed_sample(&session, &keyspace, at(100, 50), files).await?;
    seed_repo(&session, &keyspace, at(100, 50), repos).await?;
    // drop a table the next apply would create, to see that a refusal issues no DDL
    exec(&session, &format!("DROP TABLE {keyspace}.nodes")).await?;
    let before = record_rows(&session, &keyspace).await?;
    // a changed files partition size is refused, naming both values
    let mut changed = conf.clone();
    changed.thorium.files.partition_size = files * 20;
    let err = apply_refused(&session, &changed, &ConfigAccept::default()).await?;
    match &err {
        ApplyError::ConfigChanged {
            setting,
            recorded,
            configured,
        } => {
            assert_eq!(setting, "thorium.files.partition_size");
            assert_eq!(recorded.as_deref(), Some(files.to_string().as_str()));
            assert_eq!(
                configured.as_deref(),
                Some((files * 20).to_string().as_str())
            );
        }
        other => panic!("not a changed setting: {other:?}"),
    }
    let text = err.to_string();
    assert!(text.contains(&files.to_string()) && text.contains(&(files * 20).to_string()));
    assert_eq!(err.refused_settings(), vec!["thorium.files.partition_size"]);
    // nothing was created and no row changed
    assert!(!has_table(&session, &keyspace, "nodes").await?);
    assert_eq!(record_rows(&session, &keyspace).await?, before);
    // a dry run refuses it the same way
    let options = ApplyOptions {
        target: ApplyTarget::Declared,
        dry_run: true,
    };
    let accept = ConfigAccept::default();
    let dry = apply_accepting(&session, &changed, &options, &accept, "key-config", &|_| ()).await;
    assert!(
        matches!(&dry, Err(err) if err.refused_settings() == vec!["thorium.files.partition_size"]),
        "{dry:?}"
    );
    // the unchanged config converges, creating the dropped table
    apply_declared(&session, &conf).await?;
    assert!(has_table(&session, &keyspace, "nodes").await?);
    // settings of types with no data are recorded with their new values
    let mut empty = conf.clone();
    empty.thorium.entities.partition_size += 1;
    empty.thorium.tags.entities.partition_size += 1;
    empty.thorium.results.earliest += 86_400;
    apply_declared(&session, &empty).await?;
    let rows = config_rows(&session, &keyspace).await?;
    assert_eq!(
        rows.get("partition_size.entities"),
        Some(&empty.thorium.entities.partition_size.to_string())
    );
    assert_eq!(
        rows.get("partition_size.tags.entities"),
        Some(&empty.thorium.tags.entities.partition_size.to_string())
    );
    assert_eq!(
        rows.get("earliest.results"),
        Some(&empty.thorium.results.earliest.to_string())
    );
    // a tag type with data refuses its own size, and every refused setting is named together
    seed_tag(
        &session,
        &keyspace,
        "Repos",
        at(100, 50),
        empty.thorium.tags.repos.partition_size,
    )
    .await?;
    let before = record_rows(&session, &keyspace).await?;
    let mut several = empty.clone();
    several.thorium.files.partition_size = files * 20;
    several.thorium.repos.partition_size = repos * 20;
    several.thorium.tags.repos.partition_size += 1;
    several.thorium.tags.files.partition_size += 1;
    let err = apply_refused(&session, &several, &ConfigAccept::default()).await?;
    assert!(matches!(err, ApplyError::Refused(_)), "{err:?}");
    assert_eq!(
        err.refused_settings(),
        vec![
            "thorium.files.partition_size",
            "thorium.repos.partition_size",
            "thorium.tags.repos.partition_size",
        ]
    );
    assert_eq!(record_rows(&session, &keyspace).await?, before);
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}

/// With stored files, a changed files password is refused, naming the setting without printing
/// either password; a stored repo alone refuses it too; with nothing stored the new identity is
/// recorded; two deployments with the same password record different hashes
#[tokio::test]
async fn a_changed_files_password_is_refused_while_files_exist() -> Result<(), Error> {
    // record one password, then store a file
    let session = test_utilities::scylla_session().await?;
    let mut conf = fresh(&session, "config_password").await?;
    let keyspace = conf.thorium.namespace.clone();
    let first = "l4c-first-files-password";
    let second = "l4c-second-files-password";
    conf.thorium.files.password = first.to_owned();
    apply_declared(&session, &conf).await?;
    seed_sample(
        &session,
        &keyspace,
        at(10, 5),
        conf.thorium.files.partition_size,
    )
    .await?;
    let before = record_rows(&session, &keyspace).await?;
    // another password is refused, naming only the setting
    let mut changed = conf.clone();
    changed.thorium.files.password = second.to_owned();
    let err = apply_refused(&session, &changed, &ConfigAccept::default()).await?;
    assert!(
        matches!(
            &err,
            ApplyError::ConfigChanged { setting, recorded: None, configured: None }
                if setting == "thorium.files.password"
        ),
        "{err:?}"
    );
    for text in [err.to_string(), format!("{err:?}")] {
        assert!(text.contains("files.password"), "{text}");
        assert!(!text.contains(first) && !text.contains(second), "{text}");
    }
    assert_eq!(err.refused_settings(), vec!["thorium.files.password"]);
    assert_eq!(record_rows(&session, &keyspace).await?, before);
    // approving the sampled sizes doesn't let it through
    let sizes = ConfigAccept {
        partition_sizes: true,
    };
    apply_refused(&session, &changed, &sizes).await?;
    // the same password re-verifies against the recorded identity, which stays as it was
    apply_declared(&session, &conf).await?;
    assert_eq!(
        config_rows(&session, &keyspace).await?.get(FILES_PASSWORD),
        before_identity(&before)?.as_ref()
    );
    // a deployment with only a stored repo refuses the change too
    let mut repo_only = fresh(&session, "config_password_repo").await?;
    repo_only.thorium.files.password = first.to_owned();
    apply_declared(&session, &repo_only).await?;
    let repo_keyspace = repo_only.thorium.namespace.clone();
    seed_repo(
        &session,
        &repo_keyspace,
        at(10, 5),
        repo_only.thorium.repos.partition_size,
    )
    .await?;
    let identity = config_rows(&session, &repo_keyspace).await?[FILES_PASSWORD].clone();
    repo_only.thorium.files.password = second.to_owned();
    let err = apply_refused(&session, &repo_only, &ConfigAccept::default()).await?;
    assert_eq!(err.refused_settings(), vec!["thorium.files.password"]);
    // the same password in two deployments records two different hashes
    assert_ne!(
        Some(&identity),
        config_rows(&session, &keyspace).await?.get(FILES_PASSWORD)
    );
    // with nothing stored, the new password's identity is recorded
    let mut unused = fresh(&session, "config_password_unused").await?;
    unused.thorium.files.password = first.to_owned();
    apply_declared(&session, &unused).await?;
    unused.thorium.files.password = second.to_owned();
    apply_declared(&session, &unused).await?;
    let unused_keyspace = unused.thorium.namespace.clone();
    let recorded = config_rows(&session, &unused_keyspace).await?[FILES_PASSWORD].clone();
    assert!(verify_files_password(&recorded, second).expect("the identity is accepted"));
    for keyspace in [keyspace, repo_keyspace, unused_keyspace] {
        exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await?;
    }
    Ok(())
}

/// Find the recorded password identity in a dump of the record's rows
///
/// # Arguments
///
/// * `rows` - The rows, as `SELECT JSON` text
fn before_identity(rows: &[String]) -> Result<Option<String>, Error> {
    // parse each row and keep the password's value
    for row in rows {
        let value: serde_json::Value =
            serde_json::from_str(row).map_err(|err| Error::new(err.to_string()))?;
        if value["id"] == FILES_PASSWORD {
            return Ok(value["value"].as_str().map(ToOwned::to_owned));
        }
    }
    Ok(None)
}

/// Overwrite the recorded files-password identity
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace to write to
/// * `identity` - The PHC string to record
async fn set_identity(session: &Session, keyspace: &str, identity: &str) -> Result<(), Error> {
    // replace only the value of the password's config row
    exec(
        session,
        &format!(
            "UPDATE {keyspace}.schema_migrations SET value = '{identity}' \
             WHERE kind = 'config' AND id = '{FILES_PASSWORD}'"
        ),
    )
    .await
}

/// A recorded files-password identity whose parameters differ from `Params::default()`, or whose
/// algorithm isn't argon2id, is refused, even when it would verify against the configured
/// password, so its costs are never computed
#[tokio::test]
async fn a_recorded_identity_with_other_parameters_is_refused() -> Result<(), Error> {
    // record the password's identity
    let session = test_utilities::scylla_session().await?;
    let mut conf = fresh(&session, "config_params").await?;
    let keyspace = conf.thorium.namespace.clone();
    let password = "l4c-params-password";
    conf.thorium.files.password = password.to_owned();
    apply_declared(&session, &conf).await?;
    // identities of the same password with another algorithm or other costs
    let salt = SaltString::generate(&mut OsRng);
    let hash = |algorithm, params| {
        Argon2::new(algorithm, Version::V0x13, params)
            .hash_password(password.as_bytes(), &salt)
            .map(|hash| hash.to_string())
            .map_err(|err| Error::new(err.to_string()))
    };
    let crafted = [
        hash(Algorithm::Argon2i, Params::default())?,
        hash(Algorithm::Argon2d, Params::default())?,
        hash(
            Algorithm::Argon2id,
            Params::new(8, 1, 1, None).map_err(|err| Error::new(err.to_string()))?,
        )?,
        hash(
            Algorithm::Argon2id,
            Params::new(19_456, 3, 1, None).map_err(|err| Error::new(err.to_string()))?,
        )?,
    ];
    for identity in crafted {
        // each would verify against the password, but its parameters refuse it first
        set_identity(&session, &keyspace, &identity).await?;
        let err = apply_refused(&session, &conf, &ConfigAccept::default()).await?;
        assert!(
            matches!(err, ApplyError::PasswordHashParams),
            "{identity}: {err:?}"
        );
        assert_eq!(err.refused_settings(), vec!["thorium.files.password"]);
        assert_eq!(
            config_rows(&session, &keyspace).await?.get(FILES_PASSWORD),
            Some(&identity)
        );
    }
    // passes that would keep a core busy for hours are refused at once
    set_identity(
        &session,
        &keyspace,
        "$argon2id$v=19$m=8,t=4294967295,p=1$c2FsdHNhbHRzYWx0$\
         aGFzaGhhc2hoYXNoaGFzaGhhc2hoYXNoaGFzaGhhc2g",
    )
    .await?;
    let err = apply_refused(&session, &conf, &ConfigAccept::default()).await?;
    assert!(matches!(err, ApplyError::PasswordHashParams), "{err:?}");
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}

/// Before the first data-shaping record, rows written at a partition size other than the
/// configured one make `schema apply` record nothing and report the data type, the mismatched
/// rows, and the size they suggest; `--accept-partition-sizes` records the configured sizes
#[tokio::test]
async fn sampled_rows_at_another_size_stop_the_first_record() -> Result<(), Error> {
    // a deployment whose rows were written at 3600 seconds but whose config says otherwise, and
    // whose record holds no data-shaping rows yet, as an apply before them left it
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "config_sampled").await?;
    let keyspace = conf.thorium.namespace.clone();
    apply_declared(&session, &conf).await?;
    exec(
        &session,
        &format!("DELETE FROM {keyspace}.schema_migrations WHERE kind = 'config'"),
    )
    .await?;
    assert_ne!(conf.thorium.files.partition_size, 3600);
    assert_ne!(conf.thorium.tags.repos.partition_size, 3600);
    for day in 0..12 {
        seed_sample(
            &session,
            &keyspace,
            at(40 + day * 7, 1_000 + day * 977),
            3600,
        )
        .await?;
    }
    for day in 0..5 {
        seed_tag(
            &session,
            &keyspace,
            "Repos",
            at(60 + day, 2_000 + day),
            3600,
        )
        .await?;
    }
    // many more rows at the configured size, each in a partition of its own, so the sample must
    // spread over the token ring to find every mismatched row
    for row in 0..200 {
        let uploaded = at(row % 300, 37 * row);
        let bucket = partition(uploaded, uploaded.year(), conf.thorium.files.partition_size);
        exec(
            &session,
            &format!(
                "INSERT INTO {keyspace}.samples_list (group, year, bucket, uploaded, id) \
                 VALUES ('spread-{row}', {}, {bucket}, '{}', uuid())",
                uploaded.year(),
                uploaded.to_rfc3339()
            ),
        )
        .await?;
    }
    // rows of a type written at the configured size don't stop it
    seed_repo(
        &session,
        &keyspace,
        at(50, 123),
        conf.thorium.repos.partition_size,
    )
    .await?;
    let before = record_rows(&session, &keyspace).await?;
    let err = apply_refused(&session, &conf, &ConfigAccept::default()).await?;
    assert_eq!(err.sampled_data_types(), vec!["files", "tags.repos"]);
    let ApplyError::Refused(refusals) = &err else {
        panic!("both data types aren't reported: {err:?}");
    };
    for refusal in refusals {
        match refusal {
            ApplyError::SampledSizeMismatch {
                data_type,
                mismatched,
                suggested,
            } => {
                let expected = if data_type == "files" { 12 } else { 5 };
                assert_eq!(*mismatched, expected, "{data_type}");
                assert_eq!(*suggested, Some(3600), "{data_type}");
            }
            other => panic!("not a sampled mismatch: {other:?}"),
        }
    }
    // nothing is recorded
    assert_eq!(record_rows(&session, &keyspace).await?, before);
    assert!(config_rows(&session, &keyspace).await?.is_empty());
    // once approved, the configured sizes are recorded, with a warning
    let accept = ConfigAccept {
        partition_sizes: true,
    };
    let report = apply_accepted(&session, &conf, &accept).await?;
    let rows = config_rows(&session, &keyspace).await?;
    assert_eq!(
        rows.get("partition_size.files"),
        Some(&conf.thorium.files.partition_size.to_string())
    );
    assert_eq!(rows.len(), 2 * DATA_TYPES.len() + 1, "{rows:?}");
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("thorium.files.partition_size")),
        "{:?}",
        report.warnings
    );
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}

/// Before the first data-shaping record, rows written at the configured partition size let
/// `schema apply` record the sizes without an approval or a warning
#[tokio::test]
async fn sampled_rows_at_the_configured_size_are_recorded() -> Result<(), Error> {
    // a deployment whose rows match its sizes, with no data-shaping rows recorded yet
    let session = test_utilities::scylla_session().await?;
    let matching = fresh(&session, "config_matching").await?;
    let matching_keyspace = matching.thorium.namespace.clone();
    apply_declared(&session, &matching).await?;
    exec(
        &session,
        &format!("DELETE FROM {matching_keyspace}.schema_migrations WHERE kind = 'config'"),
    )
    .await?;
    for day in 0..12 {
        seed_sample(
            &session,
            &matching_keyspace,
            at(40 + day * 7, 1_000 + day * 977),
            matching.thorium.files.partition_size,
        )
        .await?;
    }
    // its sizes are recorded without approval or warning
    let report = apply_declared(&session, &matching).await?;
    assert!(
        report
            .warnings
            .iter()
            .all(|warning| !warning.contains("partition_size")),
        "{:?}",
        report.warnings
    );
    assert_eq!(
        config_rows(&session, &matching_keyspace)
            .await?
            .get("partition_size.files"),
        Some(&matching.thorium.files.partition_size.to_string())
    );
    exec(
        &session,
        &format!("DROP KEYSPACE IF EXISTS {matching_keyspace}"),
    )
    .await
}

/// The warning an apply gives when it records a raised `earliest` value
///
/// # Arguments
///
/// * `data_type` - The data type, such as `files` or `tags.repos`
/// * `at` - The raised value, in seconds since the epoch
fn raised_warning(data_type: &str, at: i64) -> String {
    // the date the raised value names, to the second
    let date = DateTime::from_timestamp(at, 0)
        .expect("the earliest value is a date")
        .to_rfc3339_opts(SecondsFormat::Secs, true);
    format!(
        "earliest.{data_type} raised: data older than {date} no longer appears in default \
         listings and searches. Nothing is deleted. Lower it again to see it."
    )
}

/// Apply the declared schema, returning the report and every progress line
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `conf` - The config naming the keyspace
/// * `dry_run` - Whether to only plan the statements
async fn apply_watched(
    session: &Session,
    conf: &Conf,
    dry_run: bool,
) -> Result<(ApplyReport, Vec<String>), Error> {
    // keep each progress line the apply reports
    let lines = Mutex::new(Vec::new());
    let progress = |line: &str| {
        lines
            .lock()
            .expect("the progress lines lock")
            .push(line.to_owned());
    };
    let options = ApplyOptions {
        target: ApplyTarget::Declared,
        dry_run,
    };
    // converge and record, turning a refusal into a test error
    let report = apply(session, conf, &options, "key-config", &progress)
        .await
        .map_err(|err| Error::new(format!("schema apply failed: {err:?}")))?;
    let lines = lines
        .into_inner()
        .map_err(|err| Error::new(err.to_string()))?;
    Ok((report, lines))
}

/// With data of the type present, a raised `earliest` is recorded with a warning saying what
/// default listings no longer show, a lowered one is recorded without a warning, a dry run
/// reports either without recording it, and a refused partition size beside raised `earliest`
/// values names only the size
#[tokio::test]
async fn an_earliest_change_is_recorded_while_its_data_exists() -> Result<(), Error> {
    // record the settings, then store a file and a repo at the recorded sizes
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "config_earliest").await?;
    let keyspace = conf.thorium.namespace.clone();
    apply_declared(&session, &conf).await?;
    let files = conf.thorium.files.partition_size;
    seed_sample(&session, &keyspace, at(100, 50), files).await?;
    seed_repo(
        &session,
        &keyspace,
        at(100, 50),
        conf.thorium.repos.partition_size,
    )
    .await?;
    let was = conf.thorium.files.earliest;
    // a dry run with a raised value warns and says it would record it, but records nothing
    let mut raised = conf.clone();
    raised.thorium.files.earliest += 86_400;
    let now = raised.thorium.files.earliest;
    let before = record_rows(&session, &keyspace).await?;
    let (report, lines) = apply_watched(&session, &raised, true).await?;
    assert!(
        report.warnings.contains(&raised_warning("files", now)),
        "{:?}",
        report.warnings
    );
    let line =
        format!("thorium.files.earliest changed from {was} to {now}; would record the new value");
    assert!(lines.contains(&line), "{lines:?}");
    assert_eq!(record_rows(&session, &keyspace).await?, before);
    // a real run records it, with the same warning and its progress line
    let (report, lines) = apply_watched(&session, &raised, false).await?;
    assert!(
        report.warnings.contains(&raised_warning("files", now)),
        "{:?}",
        report.warnings
    );
    let line =
        format!("thorium.files.earliest changed from {was} to {now}; recording the new value");
    assert!(lines.contains(&line), "{lines:?}");
    let rows = config_rows(&session, &keyspace).await?;
    assert_eq!(rows.get("earliest.files"), Some(&now.to_string()));
    // a lowered value, below the first one, is recorded without a warning
    let mut lowered = conf.clone();
    lowered.thorium.files.earliest -= 86_400;
    let (report, lines) = apply_watched(&session, &lowered, false).await?;
    assert!(
        report
            .warnings
            .iter()
            .all(|warning| !warning.contains("earliest")),
        "{:?}",
        report.warnings
    );
    let line = format!(
        "thorium.files.earliest changed from {now} to {}; recording the new value",
        lowered.thorium.files.earliest
    );
    assert!(lines.contains(&line), "{lines:?}");
    let rows = config_rows(&session, &keyspace).await?;
    assert_eq!(
        rows.get("earliest.files"),
        Some(&lowered.thorium.files.earliest.to_string())
    );
    // every earliest value raised beside a changed size: only the size is refused, and nothing
    // is recorded
    let mut several = lowered.clone();
    several.thorium.files.partition_size = files * 20;
    for earliest in [
        &mut several.thorium.files.earliest,
        &mut several.thorium.repos.earliest,
        &mut several.thorium.results.earliest,
        &mut several.thorium.entities.earliest,
        &mut several.thorium.associations.earliest,
        &mut several.thorium.tags.files.earliest,
        &mut several.thorium.tags.repos.earliest,
        &mut several.thorium.tags.entities.earliest,
    ] {
        *earliest += 86_400 * 7;
    }
    let before = record_rows(&session, &keyspace).await?;
    let err = apply_refused(&session, &several, &ConfigAccept::default()).await?;
    assert_eq!(err.refused_settings(), vec!["thorium.files.partition_size"]);
    assert!(!err.to_string().contains("earliest"), "{err}");
    assert_eq!(record_rows(&session, &keyspace).await?, before);
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}

/// With only an `s3_ids` row stored, a changed files password is refused, since the object it
/// maps was carted with the recorded password
#[tokio::test]
async fn a_changed_files_password_is_refused_with_only_s3_ids_stored() -> Result<(), Error> {
    // record one password, then map a stored object without any other row
    let session = test_utilities::scylla_session().await?;
    let mut conf = fresh(&session, "config_password_s3").await?;
    let keyspace = conf.thorium.namespace.clone();
    conf.thorium.files.password = "l4c-s3-first-password".to_owned();
    apply_declared(&session, &conf).await?;
    exec(
        &session,
        &format!(
            "INSERT INTO {keyspace}.s3_ids (type, id, sha256) VALUES ('File', uuid(), \
             'a2c4e6a8c0e2a4c6e8a0c2e4a6c8e0a2c4e6a8c0e2a4c6e8a0c2e4a6c8e0a2c4')"
        ),
    )
    .await?;
    let before = record_rows(&session, &keyspace).await?;
    // another password is refused, and the record is unchanged
    conf.thorium.files.password = "l4c-s3-second-password".to_owned();
    let err = apply_refused(&session, &conf, &ConfigAccept::default()).await?;
    assert_eq!(err.refused_settings(), vec!["thorium.files.password"]);
    assert_eq!(record_rows(&session, &keyspace).await?, before);
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}

/// Before the first data-shaping record, a stored row whose year no date can hold is left out of
/// the sample instead of stopping the apply
#[tokio::test]
async fn a_row_with_a_year_no_date_holds_is_left_out_of_the_sample() -> Result<(), Error> {
    // a deployment with no data-shaping rows yet and one row in a year past any date
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "config_year").await?;
    let keyspace = conf.thorium.namespace.clone();
    apply_declared(&session, &conf).await?;
    exec(
        &session,
        &format!("DELETE FROM {keyspace}.schema_migrations WHERE kind = 'config'"),
    )
    .await?;
    exec(
        &session,
        &format!(
            "INSERT INTO {keyspace}.samples_list (group, year, bucket, uploaded, id) \
             VALUES ('g', 300000, 5, '2026-02-03T04:05:06Z', uuid())"
        ),
    )
    .await?;
    // the apply records the configured sizes
    apply_declared(&session, &conf).await?;
    assert_eq!(
        config_rows(&session, &keyspace)
            .await?
            .get("partition_size.files"),
        Some(&conf.thorium.files.partition_size.to_string())
    );
    exec(&session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await
}
