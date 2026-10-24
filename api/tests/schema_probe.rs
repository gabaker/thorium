//! Tests that the read-only schema probe observes a live keyspace in the registry's canonical
//! form, and that a keyspace this release creates or an older release created compares with the
//! current snapshot without drift beyond the registered legacy objects

use elasticsearch::indices::{IndicesCreateParts, IndicesDeleteParts, IndicesGetMappingParts};
use scylla::client::session::Session;
use std::path::PathBuf;
use thorium::models::backends::setup;
use thorium::models::elastic::ElasticIndex;
use thorium::models::schema::{
    ApplyOptions, ApplyTarget, DriftReport, ObservedSchema, RevisionSnapshot, SchemaBackend,
    SchemaRows, SnapshotObject, apply, probe_elastic, probe_scylla,
};
use thorium::models::upgrades::{BackendSchemaStatus, DriftKind, latest};
use thorium::test_utilities::{self, CONF};
use thorium::{Conf, Error};

#[path = "schema_probe/manual.rs"]
mod manual;

/// The `events` table an older release's setup created, which the registry doesn't declare
const LEGACY_EVENTS: &str = "CREATE TABLE IF NOT EXISTS {ns}.events (event_type TEXT, year INT, \
    bucket INT, timestamp TIMESTAMP, id UUID, parent UUID, user TEXT, trigger_depth SMALLINT, \
    data TEXT, PRIMARY KEY ((event_type, year, bucket), timestamp, id)) \
    WITH default_time_to_live = 2592000";

/// The `entities_by_name` view an older release's setup created, which the registry doesn't
/// declare
const LEGACY_ENTITIES_BY_NAME: &str = "CREATE MATERIALIZED VIEW IF NOT EXISTS \
    {ns}.entities_by_name AS SELECT name, group, kind, created, year, bucket, id, submitter, \
    kind_data, description, image FROM {ns}.entities WHERE name IS NOT NULL AND group IS NOT NULL \
    AND kind IS NOT NULL AND created IS NOT NULL AND year IS NOT NULL AND bucket IS NOT NULL \
    AND id IS NOT NULL PRIMARY KEY (name, group, kind, created, year, bucket, id)";

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

/// Build a config for a keyspace beside the run's own namespace, dropping any keyspace an earlier
/// run left there
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `suffix` - What makes this test's keyspace name unique
async fn fresh(session: &Session, suffix: &str) -> Result<Conf, Error> {
    // replace any keyspace an earlier run left behind
    let keyspace = format!("{}_{suffix}", CONF.thorium.namespace).to_lowercase();
    exec(session, &format!("DROP KEYSPACE IF EXISTS {keyspace}")).await?;
    // point a copy of the test config at it
    let mut conf = CONF.clone();
    conf.thorium.namespace = keyspace;
    Ok(conf)
}

/// Apply the declared schema to a config's keyspace, ignoring progress
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `conf` - The config naming the keyspace
async fn apply_declared(session: &Session, conf: &Conf) -> Result<(), Error> {
    // converge and record, turning a refusal into a test error
    let options = ApplyOptions {
        target: ApplyTarget::Declared,
        dry_run: false,
    };
    apply(session, conf, &options, "probe-test", &|_| ())
        .await
        .map_err(|err| Error::new(format!("schema apply failed: {err:?}")))?;
    Ok(())
}

/// Create a keyspace the way an older release did: its setup statements, with the `events` table
/// and the `entities_by_name` view
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `keyspace` - The keyspace to create
async fn create_legacy(session: &Session, keyspace: &str) -> Result<(), Error> {
    // the keyspace, then every statement of the older setup
    exec(
        session,
        &format!(
            "CREATE KEYSPACE {keyspace} WITH replication = \
             {{'class': 'NetworkTopologyStrategy', 'replication_factor': 1}}"
        ),
    )
    .await?;
    let legacy = include_str!("../src/models/schema/goldens/legacy_ddl.cql")
        .replace("{ns}", keyspace)
        .replace("{ttl}", "2592000");
    let statements = legacy
        .split(';')
        .map(str::trim)
        .filter(|statement| !statement.is_empty())
        .map(ToOwned::to_owned)
        .chain([LEGACY_EVENTS, LEGACY_ENTITIES_BY_NAME].map(|cql| cql.replace("{ns}", keyspace)));
    for statement in statements {
        exec(session, &statement).await?;
    }
    Ok(())
}

/// Whether an object is one of the two registered legacy objects
///
/// # Arguments
///
/// * `backend` - The object's backend
/// * `name` - The object's name
fn legacy(backend: SchemaBackend, name: &str) -> bool {
    // the `events` table and the `entities_by_name` view
    backend == SchemaBackend::Scylla && matches!(name, "events" | "entities_by_name")
}

/// Probe a keyspace's Scylla objects and recorded layouts
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `conf` - The config naming the keyspace
async fn observe(session: &Session, conf: &Conf) -> Result<ObservedSchema, Error> {
    // probe, then fingerprint what was seen
    let (objects, probed) = probe_scylla(session, &conf.thorium.namespace)
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    Ok(ObservedSchema::new(objects, &probed))
}

/// The snapshot of the schema this build declares, under the latest revision
fn current() -> RevisionSnapshot {
    // the registry's objects, labelled with the newest catalog revision
    RevisionSnapshot::declared(latest())
}

/// A keyspace this release creates, with the two migration-state tables and the record the first
/// apply writes, probes with zero drift against the current snapshot: Scylla, Redis, and S3 are
/// all `ok`, each observed backend fingerprint equals the snapshot's, and neither legacy object is
/// reported
#[tokio::test]
async fn a_keyspace_this_release_creates_probes_with_no_drift() -> Result<(), Error> {
    // create the keyspace from the registry and record it
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "probe_new").await?;
    apply_declared(&session, &conf).await?;
    // probe it and compare with the current snapshot
    let observed = observe(&session, &conf).await?;
    let expected = current();
    let report = DriftReport::compare(&expected, &observed, &legacy);
    let backends = report.backends.keys().copied().collect::<Vec<_>>();
    assert_eq!(
        backends,
        [
            SchemaBackend::Scylla,
            SchemaBackend::Redis,
            SchemaBackend::S3
        ]
    );
    for (backend, drift) in &report.backends {
        assert_eq!(
            drift.status,
            BackendSchemaStatus::Ok,
            "{backend:?}: {drift:?}"
        );
        assert!(drift.objects.is_empty(), "{backend:?}: {:?}", drift.objects);
        assert_eq!(
            observed.backends.get(backend),
            expected.backends.get(backend),
            "{backend:?}"
        );
    }
    // the legacy objects are absent, and absent isn't missing even against a snapshot listing them
    let v02 = RevisionSnapshot::committed("2026-10-v02")
        .expect("2026-10-v02 has a snapshot")
        .map_err(|err| Error::new(format!("{err:?}")))?;
    let against_v02 = DriftReport::compare(&v02, &observed, &legacy);
    let named = against_v02.backends[&SchemaBackend::Scylla]
        .objects
        .iter()
        .map(|object| object.object.as_str())
        .collect::<Vec<_>>();
    assert!(!named.contains(&"events"), "{named:?}");
    assert!(!named.contains(&"entities_by_name"), "{named:?}");
    // drop the keyspace again
    exec(
        &session,
        &format!("DROP KEYSPACE IF EXISTS {}", conf.thorium.namespace),
    )
    .await
}

/// A keyspace an older release created, holding the `events` table and the `entities_by_name`
/// view, probes after `schema apply` with no `missing` or `changed` object and no `unexpected`
/// object but those two, which the legacy registry acknowledges; without the registry they are
/// drift, and against the `2026-10-v02` snapshot, which lists them, they match
#[tokio::test]
async fn a_keyspace_an_older_release_created_drifts_only_by_its_legacy_objects() -> Result<(), Error>
{
    // create the keyspace as an older release did, then converge and record it
    let session = test_utilities::scylla_session().await?;
    let conf = fresh(&session, "probe_legacy").await?;
    create_legacy(&session, &conf.thorium.namespace).await?;
    apply_declared(&session, &conf).await?;
    // probe it and compare with the current snapshot
    let observed = observe(&session, &conf).await?;
    let report = DriftReport::compare(&current(), &observed, &legacy);
    for (backend, drift) in &report.backends {
        assert_eq!(
            drift.status,
            BackendSchemaStatus::Ok,
            "{backend:?}: {drift:?}"
        );
    }
    // only the two legacy objects are reported, both acknowledged
    let scylla = &report.backends[&SchemaBackend::Scylla].objects;
    let found = scylla
        .iter()
        .map(|object| (object.object.as_str(), object.kind, object.acknowledged))
        .collect::<Vec<_>>();
    assert_eq!(
        found,
        [
            ("entities_by_name", DriftKind::Unexpected, true),
            ("events", DriftKind::Unexpected, true),
        ]
    );
    assert!(scylla.iter().all(|object| object.fingerprint.is_some()));
    for backend in [SchemaBackend::Redis, SchemaBackend::S3] {
        assert!(report.backends[&backend].objects.is_empty(), "{backend:?}");
    }
    // without the legacy registry the same two objects are drift
    let unregistered = DriftReport::compare(&current(), &observed, &|_, _: &str| false);
    let scylla = &unregistered.backends[&SchemaBackend::Scylla];
    assert_eq!(scylla.status, BackendSchemaStatus::Drift);
    assert!(scylla.objects.iter().all(|object| !object.acknowledged));
    assert_eq!(scylla.objects.len(), 2);
    // against the 2026-10-v02 snapshot, which lists both, they match
    let v02 = RevisionSnapshot::committed("2026-10-v02")
        .expect("2026-10-v02 has a snapshot")
        .map_err(|err| Error::new(format!("{err:?}")))?;
    let against_v02 = DriftReport::compare(&v02, &observed, &|_, _: &str| false);
    let named = against_v02.backends[&SchemaBackend::Scylla]
        .objects
        .iter()
        .map(|object| object.object.as_str())
        .collect::<Vec<_>>();
    assert!(!named.contains(&"events"), "{named:?}");
    assert!(!named.contains(&"entities_by_name"), "{named:?}");
    // drop the keyspace again
    exec(
        &session,
        &format!("DROP KEYSPACE IF EXISTS {}", conf.thorium.namespace),
    )
    .await
}

/// A config whose Elastic index names are unique to this test
///
/// # Arguments
///
/// * `suffix` - What makes the names unique
fn elastic_conf(suffix: &str) -> Conf {
    // name each index after the run's namespace
    let mut conf = CONF.clone();
    let prefix = format!("{}_{suffix}", CONF.thorium.namespace).to_lowercase();
    conf.elastic.results.samples = format!("{prefix}_results_samples");
    conf.elastic.results.repos = format!("{prefix}_results_repos");
    conf.elastic.tags.samples = format!("{prefix}_tags_samples");
    conf.elastic.tags.repos = format!("{prefix}_tags_repos");
    conf
}

/// The Elastic probe reads each configured index's mappings from the cluster, and every index the
/// search-streamer created observes exactly as the registry declares it; an index that doesn't
/// exist is left out
#[tokio::test]
#[ignore = "creates and deletes Elasticsearch indexes, which the CI user cannot do; run by hand against your own Elasticsearch: cargo test -p thorium-api --features test-utilities --test schema_probe -- --ignored the_elastic_probe_observes_each_configured_index"]
async fn the_elastic_probe_observes_each_configured_index() -> Result<(), Error> {
    // create three of the four indexes as the search-streamer does
    let conf = elastic_conf("probe");
    let client = setup::elastic(&conf).await;
    manual::create_indexes(&client, &conf, Some(&ElasticIndex::RepoTags)).await?;
    // probe them
    let observed = probe_elastic(&client, &conf.elastic)
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    let declared = SnapshotObject::declared()
        .iter()
        .filter(|object| object.backend == SchemaBackend::Elastic && object.name != "RepoTags")
        .cloned()
        .collect::<Vec<_>>();
    let mut observed_sorted = observed.clone();
    observed_sorted.sort_by(|left, right| left.name.cmp(&right.name));
    assert_eq!(observed_sorted, declared);
    // drop the indexes again
    for index in &ElasticIndex::ALL {
        let name = index.full_name(&conf.elastic);
        client
            .indices()
            .delete(IndicesDeleteParts::Index(&[name]))
            .send()
            .await
            .map_err(|err| Error::new(err.to_string()))?;
    }
    Ok(())
}

/// Capture the fixtures the probe's unit tests replay: the `system_schema` rows of a keyspace this
/// release creates and of one an older release created (both after `schema apply`), and each
/// index's `_mapping` response, from the Scylla and Elasticsearch the test config names
///
/// Run by hand with `--ignored` once per supported version; it writes into the directory
/// `THORIUM_PROBE_CAPTURE` names, each file named after the server's version.
#[tokio::test]
#[ignore = "captures fixtures from real servers and creates Elasticsearch indexes; run by hand: THORIUM_PROBE_CAPTURE=<directory> cargo test -p thorium-api --features test-utilities --test schema_probe -- --ignored capture_probe_fixtures"]
async fn capture_probe_fixtures() -> Result<(), Error> {
    // where to write, and which Scylla version is answering
    let dir = PathBuf::from(
        std::env::var("THORIUM_PROBE_CAPTURE")
            .map_err(|_| Error::new("set THORIUM_PROBE_CAPTURE to the fixture directory"))?,
    );
    let session = test_utilities::scylla_session().await?;
    let version = session
        .query_unpaged("SELECT version FROM system.versions", ())
        .await
        .map_err(|err| Error::new(err.to_string()))?
        .into_rows_result()
        .map_err(|err| Error::new(err.to_string()))?
        .first_row::<(String,)>()
        .map_err(|err| Error::new(err.to_string()))?
        .0;
    let version = version.split('-').next().unwrap_or(&version).to_owned();
    // a keyspace this release creates, and one an older release created, each converged
    let fresh_conf = fresh(&session, "capture_new").await?;
    apply_declared(&session, &fresh_conf).await?;
    let legacy_conf = fresh(&session, "capture_legacy").await?;
    create_legacy(&session, &legacy_conf.thorium.namespace).await?;
    apply_declared(&session, &legacy_conf).await?;
    for (conf, kind) in [(&fresh_conf, "new"), (&legacy_conf, "legacy")] {
        let rows = SchemaRows::read(&session, &conf.thorium.namespace)
            .await
            .map_err(|err| Error::new(err.to_string()))?;
        let text =
            serde_json::to_string_pretty(&rows).map_err(|err| Error::new(err.to_string()))?;
        let path = dir.join(format!("scylla-{version}-{kind}.json"));
        std::fs::write(&path, text + "\n").map_err(|err| Error::new(err.to_string()))?;
        exec(
            &session,
            &format!("DROP KEYSPACE IF EXISTS {}", conf.thorium.namespace),
        )
        .await?;
    }
    // each index's mappings as Elasticsearch returns them, keyed by the index's stable name
    let conf = elastic_conf("capture");
    let client = setup::elastic(&conf).await;
    let info = client
        .info()
        .send()
        .await
        .map_err(|err| Error::new(err.to_string()))?
        .json::<serde_json::Value>()
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    let es_version = info["version"]["number"]
        .as_str()
        .unwrap_or("unknown")
        .to_owned();
    manual::create_indexes(&client, &conf, None).await?;
    let mut responses = serde_json::Map::new();
    for index in &ElasticIndex::ALL {
        let name = index.full_name(&conf.elastic);
        let body = client
            .indices()
            .get_mapping(IndicesGetMappingParts::Index(&[name]))
            .send()
            .await
            .map_err(|err| Error::new(err.to_string()))?
            .json::<serde_json::Value>()
            .await
            .map_err(|err| Error::new(err.to_string()))?;
        responses.insert(index.to_string(), body);
        client
            .indices()
            .delete(IndicesDeleteParts::Index(&[name]))
            .send()
            .await
            .map_err(|err| Error::new(err.to_string()))?;
    }
    let text =
        serde_json::to_string_pretty(&responses).map_err(|err| Error::new(err.to_string()))?;
    let path = dir.join(format!("elastic-{es_version}-mappings.json"));
    std::fs::write(&path, text + "\n").map_err(|err| Error::new(err.to_string()))
}
