//! The migration runner for the families a catalog's transforms rewrite: Redis hashes and the
//! objects under an S3 prefix

use async_trait::async_trait;
use std::collections::BTreeMap;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::Mutex;

use crate::models::schema::{
    Batch, IssueReport, MigrateError, MigrationRunner, ObjectClass, Outcome, RunContext, RunMode,
    Slice,
};
use crate::models::upgrades::{ChainOutcome, Family, FusedChain, Record};

/// Runs a fused chain over the records of a Redis family or an S3 prefix
///
/// A Redis record is the hash `{namespace}:{record key}`, so a family `Redis("users")` is every
/// key matching `{namespace}:users:*`. The runner reads with `SCAN`, classifies and converts each
/// record through the chain in memory, and writes the converted record back whole in one `MULTI`.
/// An S3 record is an object, which a transform can only rename: the runner lists the prefix once
/// and writes a converted record by copying the object to its new key and deleting the old one.
pub struct ChainRunner {
    /// The transforms the pass runs
    chain: FusedChain,
    /// The family the pass rewrites
    family: Family,
    /// Cancel the run after this many batches have been read, so the next run resumes from the
    /// checkpoint
    stop_after: Option<u64>,
    /// How many batches have been read
    batches: AtomicU64,
    /// The keys under an S3 prefix, listed once when the first batch is read, so objects the pass
    /// writes never come up in its own listing
    listing: Mutex<Option<Vec<String>>>,
    /// For each converted S3 object, the key it was read under, which its write deletes
    moves: StdMutex<BTreeMap<String, String>>,
}

impl ChainRunner {
    /// A runner for a pass of a chain over a family
    ///
    /// # Arguments
    ///
    /// * `chain` - The transforms the pass runs
    /// * `family` - The family the pass rewrites
    /// * `stop_after` - Cancel the run after this many batches, to interrupt it
    #[must_use]
    pub fn new(chain: FusedChain, family: Family, stop_after: Option<u64>) -> Self {
        // nothing read yet
        ChainRunner {
            chain,
            family,
            stop_after,
            batches: AtomicU64::new(0),
            listing: Mutex::new(None),
            moves: StdMutex::new(BTreeMap::new()),
        }
    }

    /// Check that this runner's family is one it reads: a Redis family or an S3 prefix
    fn check_family(&self) -> Result<(), MigrateError> {
        // only Redis hashes and S3 objects are read and written
        match self.family {
            Family::Redis(_) | Family::S3Prefix { .. } => Ok(()),
            other @ Family::ScyllaTable(_) => Err(MigrateError::NotImplemented(format!(
                "the harness's chain runner reads Redis families and S3 prefixes only, not {other}"
            ))),
        }
    }

    /// The Redis family this runner rewrites, or an error for any other
    fn redis_family(&self) -> Result<&'static str, MigrateError> {
        // only Redis hashes are scanned
        match self.family {
            Family::Redis(name) => Ok(name),
            other => Err(MigrateError::NotImplemented(format!(
                "{other} isn't a Redis family"
            ))),
        }
    }

    /// The bucket and key prefix of the S3 family this runner rewrites
    ///
    /// # Arguments
    ///
    /// * `ctx` - The run's context, whose config names the bucket
    fn s3_location(&self, ctx: &RunContext) -> Result<(String, &'static str), MigrateError> {
        // only the ephemeral bucket is rewritten
        match self.family {
            Family::S3Prefix {
                bucket: "ephemeral",
                prefix,
            } => Ok((ctx.backends.conf.thorium.ephemeral.bucket.clone(), prefix)),
            other => Err(MigrateError::NotImplemented(format!(
                "{other} isn't an S3 prefix of the ephemeral bucket"
            ))),
        }
    }

    /// Read the next batch of an S3 prefix from the listing taken when the first batch was read
    ///
    /// # Arguments
    ///
    /// * `start` - The index in the listing to read from
    /// * `ctx` - The run's context
    async fn s3_batch(&self, start: usize, ctx: &RunContext) -> Result<Batch, MigrateError> {
        // list the prefix once, in key order
        let (bucket, prefix) = self.s3_location(ctx)?;
        let mut listing = self.listing.lock().await;
        if listing.is_none() {
            let mut keys = Vec::new();
            let mut token: Option<String> = None;
            loop {
                let page = ctx
                    .backends
                    .s3
                    .list_objects_v2()
                    .bucket(&bucket)
                    .prefix(prefix)
                    .set_continuation_token(token.take())
                    .send()
                    .await
                    .map_err(|err| MigrateError::Backend(err.to_string()))?;
                keys.extend(
                    page.contents()
                        .iter()
                        .filter_map(|object| object.key().map(str::to_owned)),
                );
                match page.next_continuation_token() {
                    Some(next) => token = Some(next.to_owned()),
                    None => break,
                }
            }
            *listing = Some(keys);
        }
        // serve one batch from the listing
        let keys = listing.as_deref().unwrap_or_default();
        let end = (start + ctx.limits.batch_rows).min(keys.len());
        let records = keys[start.min(end)..end]
            .iter()
            .map(|key| Record::S3 { key: key.clone() })
            .collect();
        Ok(Batch {
            records,
            next: serde_json::json!(end),
            done: end >= keys.len(),
        })
    }
}

#[async_trait]
impl MigrationRunner for ChainRunner {
    /// The whole family is one slice, read with one `SCAN`
    ///
    /// # Arguments
    ///
    /// * `n` - How many slices were asked for, which a `SCAN` can't split
    /// * `ctx` - The run's context
    async fn slices(&self, n: u32, ctx: &RunContext) -> Result<Vec<Slice>, MigrateError> {
        // a SCAN has one cursor and a listing one order, so one slice covers the family
        let _ = (n, ctx);
        self.check_family()?;
        Ok(vec![Slice {
            index: 0,
            start: serde_json::json!(0),
            end: serde_json::Value::Null,
        }])
    }

    /// Read the next step of the scan with each hash it found
    ///
    /// # Arguments
    ///
    /// * `slice` - The slice, which is the whole family
    /// * `after` - The cursor the last batch ended at
    /// * `ctx` - The run's context
    async fn next_batch(
        &self,
        slice: &Slice,
        after: &serde_json::Value,
        ctx: &RunContext,
    ) -> Result<Batch, MigrateError> {
        // an S3 prefix is read from its listing
        if matches!(self.family, Family::S3Prefix { .. }) {
            let start = after.as_u64().or_else(|| slice.start.as_u64()).unwrap_or(0);
            let batch = self
                .s3_batch(usize::try_from(start).unwrap_or(0), ctx)
                .await?;
            // an interrupted run stops here, after this batch has been read
            let read = self.batches.fetch_add(1, Ordering::SeqCst) + 1;
            if self.stop_after.is_some_and(|limit| read >= limit) {
                ctx.cancel.cancel();
            }
            return Ok(batch);
        }
        // one step of the scan from the cursor
        let family = self.redis_family()?;
        let namespace = &ctx.backends.namespace;
        let cursor = after.as_u64().or_else(|| slice.start.as_u64()).unwrap_or(0);
        let mut conn = ctx
            .backends
            .redis
            .get()
            .await
            .map_err(|err| MigrateError::Backend(err.to_string()))?;
        let (next, keys): (u64, Vec<String>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg(format!("{namespace}:{family}:*"))
            .arg("COUNT")
            .arg(ctx.limits.batch_rows)
            .query_async(&mut *conn)
            .await
            .map_err(|err| MigrateError::Backend(err.to_string()))?;
        // a scan may return a key twice, so each is read once
        let keys: std::collections::BTreeSet<String> = keys.into_iter().collect();
        let mut pipe = redis::pipe();
        for key in &keys {
            pipe.hgetall(key);
        }
        let hashes: Vec<std::collections::BTreeMap<String, String>> = if keys.is_empty() {
            Vec::new()
        } else {
            pipe.query_async(&mut *conn)
                .await
                .map_err(|err| MigrateError::Backend(err.to_string()))?
        };
        drop(conn);
        // each record's key is relative to the namespace
        let prefix = format!("{namespace}:");
        let records = keys
            .iter()
            .zip(hashes)
            .map(|(key, fields)| Record::Redis {
                key: key.strip_prefix(&prefix).unwrap_or(key).to_owned(),
                fields,
            })
            .collect();
        // an interrupted run stops here, after this batch has been read
        let read = self.batches.fetch_add(1, Ordering::SeqCst) + 1;
        if self.stop_after.is_some_and(|limit| read >= limit) {
            ctx.cancel.cancel();
        }
        Ok(Batch {
            records,
            next: serde_json::json!(next),
            done: next == 0,
        })
    }

    /// Run the chain over one record
    ///
    /// # Arguments
    ///
    /// * `record` - The stored record
    /// * `mode` - `Run` or `Check`, which classify the same way
    /// * `ctx` - The run's context
    async fn process(
        &self,
        record: &Record,
        mode: RunMode,
        ctx: &RunContext,
    ) -> Result<Outcome, MigrateError> {
        // a check classifies as a run does and only skips the write
        let _ = (mode, ctx);
        Ok(match self.chain.apply(record) {
            ChainOutcome::Current => Outcome::Current,
            ChainOutcome::Converted(converted) => {
                // a moved S3 object remembers the key it came from, for its write
                if let (Record::S3 { key: old }, Record::S3 { key: new }) = (record, &converted)
                    && let Ok(mut moves) = self.moves.lock()
                {
                    moves.insert(new.clone(), old.clone());
                }
                Outcome::Write(converted)
            }
            ChainOutcome::Issue { step, issue } => Outcome::Issue(IssueReport {
                key: crate::models::schema::record_key(record).to_owned(),
                transform: step,
                class: ObjectClass::Unrecognized,
                reason: issue.reason,
            }),
        })
    }

    /// Nothing is left to do once every slice is done
    ///
    /// # Arguments
    ///
    /// * `ctx` - The run's context
    async fn finish(&self, ctx: &RunContext) -> Result<(), MigrateError> {
        // no census or record to rebuild
        let _ = ctx;
        Ok(())
    }

    /// Replace a hash with the converted record in one transaction
    ///
    /// # Arguments
    ///
    /// * `record` - The converted record
    /// * `ctx` - The run's context
    async fn write(&self, record: &Record, ctx: &RunContext) -> Result<(), MigrateError> {
        // an S3 object moves by copying it to its new key and deleting the old one
        if let Record::S3 { key: new } = record {
            let (bucket, _) = self.s3_location(ctx)?;
            let old = self
                .moves
                .lock()
                .ok()
                .and_then(|mut moves| moves.remove(new));
            if let Some(old) = old.filter(|old| old != new) {
                ctx.backends
                    .s3
                    .copy_object()
                    .bucket(&bucket)
                    .copy_source(format!("{bucket}/{old}"))
                    .key(new)
                    .send()
                    .await
                    .map_err(|err| MigrateError::Backend(err.to_string()))?;
                ctx.backends
                    .s3
                    .delete_object()
                    .bucket(&bucket)
                    .key(old)
                    .send()
                    .await
                    .map_err(|err| MigrateError::Backend(err.to_string()))?;
            }
            return Ok(());
        }
        // otherwise only Redis hashes are written
        let Record::Redis { key, fields } = record else {
            return Err(MigrateError::NotImplemented(
                "the harness's chain runner writes Redis hashes and S3 objects only".to_owned(),
            ));
        };
        let key = format!("{}:{key}", ctx.backends.namespace);
        // the old hash goes and the new one lands together
        let mut pipe = redis::pipe();
        pipe.atomic().del(&key).ignore();
        if !fields.is_empty() {
            let pairs: Vec<(&String, &String)> = fields.iter().collect();
            pipe.hset_multiple(&key, &pairs).ignore();
        }
        let mut conn = ctx
            .backends
            .redis
            .get()
            .await
            .map_err(|err| MigrateError::Backend(err.to_string()))?;
        pipe.query_async::<()>(&mut *conn)
            .await
            .map_err(|err| MigrateError::Backend(err.to_string()))?;
        Ok(())
    }
}
