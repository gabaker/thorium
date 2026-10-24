//! A test of the Elastic rebuild against a real Elasticsearch and Redis
//!
//! It creates and deletes Elasticsearch indexes, which the shared CI Elasticsearch doesn't allow, so
//! it is run by hand against a throwaway cluster. Every index is named with a stamp of the run.

mod common;

use common::migrate::World;
use std::sync::Arc;
use thoradm::migrate::registry::MIGRATIONS;
use thorium::models::elastic::{ElasticIndex, ElasticSource};
use thorium::models::schema::{
    ElasticIndexDef, RunMode, RunOptions, elastic_client, probe_elastic, run_pass,
};

/// Create an index with the mappings Thorium declares
///
/// # Arguments
///
/// * `client` - The Elastic client
/// * `world` - The world naming the indexes
/// * `index` - The index to create
async fn create_declared(
    client: &elasticsearch::Elasticsearch,
    world: &World,
    index: &ElasticIndex,
) {
    // the body the search-streamer creates it with
    let name = index.full_name(&world.conf.elastic);
    let response = client
        .indices()
        .create(elasticsearch::indices::IndicesCreateParts::Index(name))
        .body(index.create_body(&world.conf.elastic))
        .send()
        .await
        .expect("the index creates");
    assert!(response.status_code().is_success(), "{name}");
}

/// Create an index the way Elastic does when a write reaches a missing one: with dynamic mappings
///
/// # Arguments
///
/// * `client` - The Elastic client
/// * `world` - The world naming the indexes
/// * `index` - The index to create
async fn create_dynamic(
    client: &elasticsearch::Elasticsearch,
    world: &World,
    index: &ElasticIndex,
) {
    // one document makes the index with mappings from its fields
    let name = index.full_name(&world.conf.elastic);
    let response = client
        .index(elasticsearch::IndexParts::IndexId(name, "1"))
        .refresh(elasticsearch::params::Refresh::True)
        .body(serde_json::json!({"group": ["g"], "sha256": "abc"}))
        .send()
        .await
        .expect("the document indexes");
    assert!(response.status_code().is_success(), "{name}");
}

/// Whether an index exists
///
/// # Arguments
///
/// * `client` - The Elastic client
/// * `name` - The index's configured name
async fn exists(client: &elasticsearch::Elasticsearch, name: &str) -> bool {
    // a HEAD request answers 200 for an index that exists
    client
        .indices()
        .exists(elasticsearch::indices::IndicesExistsParts::Index(&[name]))
        .send()
        .await
        .expect("the existence check runs")
        .status_code()
        .is_success()
}

/// Whether a Redis key exists
///
/// # Arguments
///
/// * `world` - The world holding the Redis pool
/// * `key` - The key
async fn has(world: &World, key: &str) -> bool {
    // EXISTS counts the keys that exist
    let mut redis_conn = world.backends.redis.get().await.expect("a connection");
    let found: i64 = redis::cmd("EXISTS")
        .arg(key)
        .query_async(&mut *redis_conn)
        .await
        .expect("the key reads");
    found > 0
}

/// Give both sources a session, an event queue, and an in-flight key, returning the queue and
/// in-flight keys
///
/// # Arguments
///
/// * `world` - The world naming the namespace
async fn seed_sessions(world: &World) -> Vec<String> {
    // each source's init keys, then its queue and in-flight key
    let ns = &world.ns;
    let mut queues = Vec::new();
    let mut redis_conn = world.backends.redis.get().await.expect("a connection");
    for source in ElasticSource::ALL {
        for key in source.init_keys(ns) {
            let _: () = redis::cmd("SET")
                .arg(&key)
                .arg("session")
                .query_async(&mut *redis_conn)
                .await
                .expect("a key sets");
        }
        let lower = source.name().to_lowercase();
        for key in [
            format!("{ns}:search-streamer:{lower}:queue"),
            format!("{ns}:search-streamer:{lower}:inflight:abc"),
        ] {
            let _: () = redis::cmd("SET")
                .arg(&key)
                .arg("event")
                .query_async(&mut *redis_conn)
                .await
                .expect("a key sets");
            queues.push(key);
        }
    }
    queues
}

/// Delete the tags indexes and the keys the rebuild test made
///
/// # Arguments
///
/// * `client` - The Elastic client
/// * `world` - The world naming the indexes and the namespace
/// * `queues` - The queue and in-flight keys the test set
async fn clean_up(client: &elasticsearch::Elasticsearch, world: &World, queues: &[String]) {
    // the tags indexes the rebuild left, then every key
    let conf = &world.conf.elastic;
    let ns = &world.ns;
    for index in [ElasticIndex::SampleTags, ElasticIndex::RepoTags] {
        let _ = client
            .indices()
            .delete(elasticsearch::indices::IndicesDeleteParts::Index(&[
                index.full_name(conf)
            ]))
            .send()
            .await;
    }
    let mut redis_conn = world.backends.redis.get().await.expect("a connection");
    for source in ElasticSource::ALL {
        for key in source.init_keys(ns) {
            let _: i64 = redis::cmd("DEL")
                .arg(key)
                .query_async(&mut *redis_conn)
                .await
                .unwrap_or(0);
        }
    }
    for key in queues {
        let _: i64 = redis::cmd("DEL")
            .arg(key)
            .query_async(&mut *redis_conn)
            .await
            .unwrap_or(0);
    }
}

/// Run the rebuild through the runner against real indexes whose results mappings are dynamic: the
/// results indexes and their init keys are dropped, and the tags indexes, their keys, and the event
/// queues are left as they were
#[tokio::test]
#[ignore = "creates and deletes Elasticsearch indexes named with a run stamp and sets Redis keys: \
            point THORIUM__ELASTIC__*, THORIUM__REDIS__*, and the Scylla and S3 overrides at a \
            throwaway cluster, then run `cargo test -p thoradm --test migrate_elastic -- --ignored`"]
async fn the_rebuild_drops_real_indexes_and_keeps_the_queues() {
    // a world whose indexes are named with the run's stamp, so nothing else is touched
    let mut world = World::new("elrb").await;
    let stamp = format!(
        "elrb-{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("a time")
            .as_nanos()
    );
    let mut stamped = world.conf.clone();
    stamped.elastic.results.samples = format!("{stamp}-sample-results");
    stamped.elastic.results.repos = format!("{stamp}-repo-results");
    stamped.elastic.tags.samples = format!("{stamp}-sample-tags");
    stamped.elastic.tags.repos = format!("{stamp}-repo-tags");
    world.conf = stamped.clone();
    world.backends.conf = Arc::new(stamped);
    let client = elastic_client(&world.conf.elastic)
        .await
        .expect("Elastic is reachable");
    // the results indexes have dynamic mappings and the tags indexes the declared ones
    create_dynamic(&client, &world, &ElasticIndex::SampleResults).await;
    create_dynamic(&client, &world, &ElasticIndex::RepoResults).await;
    create_declared(&client, &world, &ElasticIndex::SampleTags).await;
    create_declared(&client, &world, &ElasticIndex::RepoTags).await;
    // both sources have a session, and each has an event queue and an in-flight key
    let ns = world.ns.clone();
    let queues = seed_sessions(&world).await;
    // the probe sees the two results indexes as mismatched
    let observed = probe_elastic(&client, &world.conf.elastic)
        .await
        .expect("the probe runs");
    assert_eq!(
        ElasticIndexDef::mismatched(&observed),
        ["SampleResults", "RepoResults"]
    );
    // run the migration through the runner
    let def = MIGRATIONS
        .iter()
        .find(|def| def.id == "elastic-rebuild")
        .expect("registered");
    let lease = world.lease("test").await;
    let ctx = world.ctx(RunMode::Run, 50);
    let opts = RunOptions {
        slices: 1,
        ..RunOptions::default()
    };
    let outcome = run_pass(
        def.runner,
        &World::spec(def),
        &opts,
        &ctx,
        &world.store,
        &lease,
    )
    .await
    .expect("the pass runs");
    lease.release().await;
    assert_eq!(outcome.counts.written, 2, "{:?}", outcome.counts);
    // the results indexes and their session are gone, and everything else stayed
    let conf = &world.conf.elastic;
    assert!(!exists(&client, ElasticIndex::SampleResults.full_name(conf)).await);
    assert!(!exists(&client, ElasticIndex::RepoResults.full_name(conf)).await);
    assert!(exists(&client, ElasticIndex::SampleTags.full_name(conf)).await);
    assert!(exists(&client, ElasticIndex::RepoTags.full_name(conf)).await);
    for key in ElasticSource::Results.init_keys(&ns) {
        assert!(!has(&world, &key).await, "{key}");
    }
    for key in ElasticSource::Tags.init_keys(&ns) {
        assert!(has(&world, &key).await, "{key}");
    }
    for key in &queues {
        assert!(has(&world, key).await, "{key}");
    }
    // clean up what the test made
    clean_up(&client, &world, &queues).await;
    world.drop_keyspace().await;
}
