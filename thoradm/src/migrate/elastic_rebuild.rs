//! The Elastic rebuild: drop the indexes whose mappings differ from the declared ones and the init
//! session keys of their sources, so the search-streamer recreates and reingests them
//!
//! Elastic is derived from Scylla, so a breaking mapping change is never migrated in place. The
//! operator pauses the search-streamer before this runs and keeps it paused until the new release's
//! Deployments are applied, so nothing writes to a missing index and Elastic never auto-creates one
//! with dynamic mappings. The run deletes the indexes first and the init keys after them, and
//! leaves the streamer's event queue and in-flight keys alone: events written meanwhile are replayed
//! after the reingest as idempotent upserts.

use async_trait::async_trait;
use bb8_redis::RedisConnectionManager;
use bb8_redis::bb8::Pool;
use elasticsearch::Elasticsearch;
use elasticsearch::indices::IndicesDeleteParts;
use serde_json::json;
use std::collections::BTreeMap;
use thorium::models::elastic::{ElasticIndex, ElasticSource};
use thorium::models::schema::{
    Batch, ElasticIndexDef, MigrateError, MigrationRunner, Outcome, RebuildTarget, RunContext,
    RunMode, Slice, SnapshotObject, elastic_client, probe_elastic,
};
use thorium::models::upgrades::Record;

/// What the rebuild reads and changes in Elastic and Redis
///
/// The live backends implement it for a run; tests implement it over memory.
#[async_trait]
pub trait Rebuild: Send + Sync {
    /// Read the live mappings of Thorium's indexes, leaving out an index that doesn't exist
    async fn observe(&self) -> Result<Vec<SnapshotObject>, MigrateError>;

    /// Delete an index by its stable name; an index that is already gone is not an error
    ///
    /// # Arguments
    ///
    /// * `index` - The stable name of the index (`SampleResults`, …)
    async fn delete_index(&self, index: &str) -> Result<(), MigrateError>;

    /// Delete Redis keys; a key that is already gone is not an error
    ///
    /// # Arguments
    ///
    /// * `keys` - The keys to delete
    async fn delete_keys(&self, keys: &[String]) -> Result<(), MigrateError>;
}

/// Find the indexes the rebuild would drop: the declared indexes that are missing or mapped
/// differently from what this build declares
///
/// # Arguments
///
/// * `backend` - Elastic and Redis
pub async fn classify(backend: &dyn Rebuild) -> Result<RebuildTarget, MigrateError> {
    // an index matches only when it was observed with the declared mappings
    let observed = backend.observe().await?;
    Ok(RebuildTarget::of(ElasticIndexDef::mismatched(&observed)))
}

/// Delete the init session keys of every source that has an index missing or mapped differently
///
/// A source whose indexes are all as declared keeps its keys, since a session in them may be a
/// reingest at work. Run after the indexes are dropped, so the sources of the dropped indexes are the
/// ones left with a missing index.
///
/// Returns the sources whose keys were deleted.
///
/// # Arguments
///
/// * `backend` - Elastic and Redis
/// * `namespace` - The Thorium namespace the keys are prefixed with
pub async fn reset_sources(
    backend: &dyn Rebuild,
    namespace: &str,
) -> Result<Vec<ElasticSource>, MigrateError> {
    // the sources that still have an index to recreate
    let target = classify(backend).await?;
    let mut reset = Vec::new();
    // a new session starts for each, so no stale one may be resumed
    for source in ElasticSource::ALL {
        if target.sources.contains(&source) {
            backend.delete_keys(&source.init_keys(namespace)).await?;
            reset.push(source);
        }
    }
    Ok(reset)
}

/// Run the whole rebuild against a backend: drop what mismatches, then reset the sources
///
/// The runner does the same in its own steps; this is the whole of it in one call, for the tests.
///
/// # Arguments
///
/// * `backend` - Elastic and Redis
/// * `namespace` - The Thorium namespace the keys are prefixed with
#[cfg(test)]
pub async fn rebuild(
    backend: &dyn Rebuild,
    namespace: &str,
) -> Result<RebuildTarget, MigrateError> {
    // the indexes to drop, deleted before any key so a source never restarts against a stale index
    let target = classify(backend).await?;
    for index in &target.indexes {
        backend.delete_index(index).await?;
    }
    // then the init keys of the sources left with a missing index
    reset_sources(backend, namespace).await?;
    Ok(target)
}

/// The live Elastic cluster and Redis a run changes
pub struct Live {
    /// The Elastic client
    client: Elasticsearch,
    /// The Elastic config naming each index
    conf: thorium::conf::Elastic,
    /// The Redis connection pool
    redis: Pool<RedisConnectionManager>,
}

impl Live {
    /// Connect to the backends a run's context names
    ///
    /// # Arguments
    ///
    /// * `ctx` - The run's context
    pub async fn connect(ctx: &RunContext) -> Result<Live, MigrateError> {
        // the Elastic client validates certificates the way the API does
        let conf = ctx.backends.conf.elastic.clone();
        let client = elastic_client(&conf)
            .await
            .map_err(|error| MigrateError::Backend(error.to_string()))?;
        Ok(Live {
            client,
            conf,
            redis: ctx.backends.redis.clone(),
        })
    }
}

#[async_trait]
impl Rebuild for Live {
    /// Read the live mappings of Thorium's indexes
    async fn observe(&self) -> Result<Vec<SnapshotObject>, MigrateError> {
        // a failed read is a backend failure the runner retries
        probe_elastic(&self.client, &self.conf)
            .await
            .map_err(|error| MigrateError::Backend(error.to_string()))
    }

    /// Delete an index by its stable name
    ///
    /// # Arguments
    ///
    /// * `index` - The stable name of the index
    async fn delete_index(&self, index: &str) -> Result<(), MigrateError> {
        // find the index by its stable name, then the configured name it is stored under
        let Some(found) = ElasticIndex::ALL
            .iter()
            .find(|candidate| ElasticIndexDef::of(candidate).name == index)
        else {
            return Err(MigrateError::Systematic(format!(
                "{index} isn't one of Thorium's Elastic indexes"
            )));
        };
        let name = found.full_name(&self.conf);
        // delete it; an index that is already gone is what was wanted
        let response = self
            .client
            .indices()
            .delete(IndicesDeleteParts::Index(&[name]))
            .send()
            .await
            .map_err(|error| MigrateError::Backend(format!("delete {name}: {error}")))?;
        let status = response.status_code();
        if status.is_success() || status.as_u16() == 404 {
            Ok(())
        } else {
            let body = response.text().await.unwrap_or_default();
            Err(MigrateError::Backend(format!(
                "delete {name}: {status}: {body}"
            )))
        }
    }

    /// Delete Redis keys
    ///
    /// # Arguments
    ///
    /// * `keys` - The keys to delete
    async fn delete_keys(&self, keys: &[String]) -> Result<(), MigrateError> {
        // one DEL for the keys, which counts only the ones that existed
        let mut connection = self
            .redis
            .get()
            .await
            .map_err(|error| MigrateError::Backend(error.to_string()))?;
        redis::cmd("DEL")
            .arg(keys)
            .query_async::<i64>(&mut *connection)
            .await
            .map_err(|error| MigrateError::Backend(error.to_string()))?;
        Ok(())
    }
}

/// The runner of the Elastic rebuild
pub struct ElasticRebuild;

/// The record of an index to drop, named by its stable name
///
/// # Arguments
///
/// * `index` - The stable name of the index
fn index_record(index: &str) -> Record {
    // the record is the index and nothing else
    Record::Redis {
        key: index.to_owned(),
        fields: BTreeMap::new(),
    }
}

#[async_trait]
impl MigrationRunner for ElasticRebuild {
    /// Make one slice, since the indexes to drop are few
    ///
    /// # Arguments
    ///
    /// * `n` - How many slices were asked for
    /// * `ctx` - The run's context
    async fn slices(&self, n: u32, ctx: &RunContext) -> Result<Vec<Slice>, MigrateError> {
        // the whole rebuild is one small slice
        let _ = (n, ctx);
        Ok(vec![Slice {
            index: 0,
            start: json!(null),
            end: json!(null),
        }])
    }

    /// Read the indexes to drop as records, all in one batch
    ///
    /// # Arguments
    ///
    /// * `slice` - The slice to read
    /// * `after` - The last position done
    /// * `ctx` - The run's context
    async fn next_batch(
        &self,
        slice: &Slice,
        after: &serde_json::Value,
        ctx: &RunContext,
    ) -> Result<Batch, MigrateError> {
        // a slice that was read is done
        let _ = slice;
        if !after.is_null() {
            return Ok(Batch {
                records: Vec::new(),
                next: after.clone(),
                done: true,
            });
        }
        // every index missing or mapped differently from what this build declares, which a run
        // resumed after deleting some finds again and deletes without error
        let live = Live::connect(ctx).await?;
        let target = classify(&live).await?;
        Ok(Batch {
            records: target
                .indexes
                .iter()
                .map(|index| index_record(index))
                .collect(),
            next: json!("read"),
            done: true,
        })
    }

    /// Count an index to drop as work, in a check and in a run alike
    ///
    /// # Arguments
    ///
    /// * `record` - The index
    /// * `mode` - `Run` or `Check`
    /// * `ctx` - The run's context
    async fn process(
        &self,
        record: &Record,
        mode: RunMode,
        ctx: &RunContext,
    ) -> Result<Outcome, MigrateError> {
        // a check classifies exactly as a run does and only skips the write
        let _ = (mode, ctx);
        Ok(Outcome::Write(record.clone()))
    }

    /// Delete one index
    ///
    /// # Arguments
    ///
    /// * `record` - The index to delete
    /// * `ctx` - The run's context
    async fn write(&self, record: &Record, ctx: &RunContext) -> Result<(), MigrateError> {
        // only an index record names something to delete
        let Record::Redis { key, .. } = record else {
            return Err(MigrateError::Systematic(
                "an elastic rebuild record isn't an index".to_owned(),
            ));
        };
        Live::connect(ctx).await?.delete_index(key).await
    }

    /// Delete the init session keys of the sources whose indexes were dropped
    ///
    /// # Arguments
    ///
    /// * `ctx` - The run's context
    async fn finish(&self, ctx: &RunContext) -> Result<(), MigrateError> {
        // a check changes nothing
        if ctx.mode == RunMode::Check {
            return Ok(());
        }
        // the sources with an index left to recreate start over, the others keep their sessions
        let live = Live::connect(ctx).await?;
        reset_sources(&live, &ctx.backends.namespace).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::sync::Mutex;

    /// The namespace the fake's keys are in
    const NS: &str = "testing";

    /// What the fake Elastic and Redis hold, and what was done to them in order
    #[derive(Default)]
    struct State {
        /// The existing indexes by stable name, with their mappings
        indexes: BTreeMap<String, serde_json::Value>,
        /// The Redis keys that exist
        keys: BTreeSet<String>,
        /// Every change, in order
        log: Vec<String>,
    }

    /// Elastic and Redis in memory
    struct Fake {
        /// What it holds
        state: Mutex<State>,
    }

    impl Fake {
        /// A fake whose indexes are as declared, with an init session, a queue, and in-flight keys
        /// for both sources
        fn declared() -> Fake {
            // every declared index with its declared mappings
            let mut state = State::default();
            for index in ElasticIndex::ALL {
                let def = ElasticIndexDef::of(&index);
                state
                    .indexes
                    .insert(def.name.to_owned(), def.body["mappings"].clone());
            }
            // each source's session and event queues
            for (source, lower) in [
                (ElasticSource::Results, "results"),
                (ElasticSource::Tags, "tags"),
            ] {
                state.keys.extend(source.init_keys(NS));
                state
                    .keys
                    .insert(format!("{NS}:search-streamer:{lower}:queue"));
                state
                    .keys
                    .insert(format!("{NS}:search-streamer:{lower}:inflight:abc"));
            }
            Fake {
                state: Mutex::new(state),
            }
        }

        /// Give an index the dynamic mappings Elastic builds for an index created by a write
        ///
        /// # Arguments
        ///
        /// * `index` - The stable name of the index
        fn make_dynamic(&self, index: &str) {
            // a text `group` with a keyword subfield, as the keyword reindex found
            self.state.lock().expect("lock").indexes.insert(
                index.to_owned(),
                json!({"properties": {"group": {"type": "text", "fields": {"keyword": {"type": "keyword"}}}}}),
            );
        }

        /// Get the log of changes
        fn log(&self) -> Vec<String> {
            // the changes so far, in order
            self.state.lock().expect("lock").log.clone()
        }

        /// Whether a key exists
        ///
        /// # Arguments
        ///
        /// * `key` - The key
        fn has(&self, key: &str) -> bool {
            // the keys the fake holds
            self.state.lock().expect("lock").keys.contains(key)
        }
    }

    #[async_trait]
    impl Rebuild for Fake {
        /// Observe the indexes that exist
        async fn observe(&self) -> Result<Vec<SnapshotObject>, MigrateError> {
            // the probe's form of each existing index's `_mapping` response
            let state = self.state.lock().expect("lock");
            Ok(ElasticIndex::ALL
                .iter()
                .filter_map(|index| {
                    let name = ElasticIndexDef::of(index).name;
                    let mappings = state.indexes.get(name)?;
                    let response = json!({"concrete": {"mappings": mappings}});
                    Some(thorium::models::schema::mapping_object(index, &response).expect("object"))
                })
                .collect())
        }

        /// Delete an index, logging it
        ///
        /// # Arguments
        ///
        /// * `index` - The stable name of the index
        async fn delete_index(&self, index: &str) -> Result<(), MigrateError> {
            // a missing index is not an error
            let mut state = self.state.lock().expect("lock");
            state.indexes.remove(index);
            state.log.push(format!("delete index {index}"));
            Ok(())
        }

        /// Delete keys, logging them
        ///
        /// # Arguments
        ///
        /// * `keys` - The keys to delete
        async fn delete_keys(&self, keys: &[String]) -> Result<(), MigrateError> {
            // a missing key is not an error
            let mut state = self.state.lock().expect("lock");
            for key in keys {
                state.keys.remove(key);
            }
            state.log.push(format!("delete keys {}", keys.join(" ")));
            Ok(())
        }
    }

    /// The rebuild deletes the affected indexes, then the source's init keys, in that order, and
    /// leaves the event queue, the in-flight keys, and the other source alone
    #[tokio::test]
    async fn the_rebuild_drops_indexes_then_init_keys_and_keeps_the_queue() {
        // the results indexes were created with dynamic mappings
        let fake = Fake::declared();
        fake.make_dynamic("SampleResults");
        fake.make_dynamic("RepoResults");
        let target = rebuild(&fake, NS).await.expect("rebuild");
        // the target names both results indexes and their source
        assert_eq!(
            target
                .indexes
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["RepoResults", "SampleResults"]
        );
        assert_eq!(
            target.sources.iter().copied().collect::<Vec<_>>(),
            [ElasticSource::Results]
        );
        // both indexes are deleted before the source's keys
        let results_keys = ElasticSource::Results.init_keys(NS).join(" ");
        assert_eq!(
            fake.log(),
            [
                "delete index RepoResults".to_owned(),
                "delete index SampleResults".to_owned(),
                format!("delete keys {results_keys}"),
            ]
        );
        // the init keys are gone, and the queue and the in-flight keys stayed
        for key in ElasticSource::Results.init_keys(NS) {
            assert!(!fake.has(&key), "{key}");
        }
        assert!(fake.has(&format!("{NS}:search-streamer:results:queue")));
        assert!(fake.has(&format!("{NS}:search-streamer:results:inflight:abc")));
        // the tags source was fine, so its session and queue were left as they were
        for key in ElasticSource::Tags.init_keys(NS) {
            assert!(fake.has(&key), "{key}");
        }
        assert!(fake.has(&format!("{NS}:search-streamer:tags:queue")));
    }

    /// Indexes that all match the declared mappings drop nothing and keep every session
    #[tokio::test]
    async fn current_indexes_are_left_alone() {
        // everything as declared, with the sessions of a reingest at work
        let fake = Fake::declared();
        let target = rebuild(&fake, NS).await.expect("rebuild");
        // nothing to drop, nothing deleted
        assert!(target.is_empty());
        assert_eq!(fake.log(), Vec::<String>::new());
        for source in ElasticSource::ALL {
            for key in source.init_keys(NS) {
                assert!(fake.has(&key), "{key}");
            }
        }
    }

    /// A run that stopped after deleting the indexes finishes the keys when it runs again: an index
    /// that is gone counts as one to drop, and deleting it again is not an error
    #[tokio::test]
    async fn a_run_resumed_after_the_indexes_were_dropped_deletes_the_keys() {
        // the indexes are gone and the session keys remain
        let fake = Fake::declared();
        {
            let mut state = fake.state.lock().expect("lock");
            state.indexes.remove("SampleTags");
            state.indexes.remove("RepoTags");
        }
        let target = rebuild(&fake, NS).await.expect("rebuild");
        // the missing indexes are the target, and their source's keys are gone afterwards
        assert_eq!(target.indexes.len(), 2);
        for key in ElasticSource::Tags.init_keys(NS) {
            assert!(!fake.has(&key), "{key}");
        }
        // the results source was left alone
        for key in ElasticSource::Results.init_keys(NS) {
            assert!(fake.has(&key), "{key}");
        }
    }

    /// The classification alone changes nothing, which is what a check does
    #[tokio::test]
    async fn classifying_changes_nothing() {
        // one index mapped differently
        let fake = Fake::declared();
        fake.make_dynamic("SampleTags");
        let target = classify(&fake).await.expect("classify");
        assert_eq!(target.indexes.len(), 1);
        assert_eq!(fake.log(), Vec::<String>::new());
    }

    /// The registered migration is the rebuild, which thoradm runs through the runner
    #[test]
    fn the_rebuild_is_registered_and_supported() {
        // the stub is replaced by the runnable rebuild
        let def =
            super::super::find(thorium::models::schema::REBUILD_MIGRATION).expect("registered");
        assert_eq!(def.support, super::super::registry::Support::Yes);
        assert!(super::super::find("elastic-keyword-mappings").is_err());
    }
}
