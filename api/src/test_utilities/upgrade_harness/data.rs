//! Reading and writing the fixture objects of Redis families and S3 prefixes, shared by both
//! drivers

use bb8_redis::RedisConnectionManager;
use bb8_redis::bb8::Pool;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use super::types::{FixtureVerifier, HistoryExpectation};
use super::{FamilyFixture, HarnessError};
use crate::Conf;
use crate::models::upgrades::{AppliedStep, Family, Record};

/// The fixture objects of a namespace's Redis
#[derive(Clone)]
pub struct RedisData {
    /// The Redis pool
    pool: Pool<RedisConnectionManager>,
    /// The namespace every key starts with
    namespace: String,
}

impl RedisData {
    /// Reach the fixture objects of a namespace
    ///
    /// # Arguments
    ///
    /// * `pool` - The Redis pool
    /// * `namespace` - The namespace every key starts with
    #[must_use]
    pub fn new(pool: Pool<RedisConnectionManager>, namespace: &str) -> Self {
        // keep what the keys are built from
        RedisData {
            pool,
            namespace: namespace.to_owned(),
        }
    }

    /// The name of a Redis family, or an error for a family the harness doesn't write
    ///
    /// # Arguments
    ///
    /// * `family` - The family
    pub fn name(family: &Family) -> Result<&'static str, HarnessError> {
        // only Redis hashes are seeded, corrupted, and verified
        match family {
            Family::Redis(name) => Ok(name),
            other => Err(HarnessError::Unsupported(format!(
                "the harness seeds and checks Redis families only, not {other}"
            ))),
        }
    }

    /// Get a connection
    async fn conn(
        &self,
    ) -> Result<bb8_redis::bb8::PooledConnection<'_, RedisConnectionManager>, HarnessError> {
        // a pooled connection
        self.pool
            .get()
            .await
            .map_err(|err| HarnessError::Backend(err.to_string()))
    }

    /// List the keys of a family
    ///
    /// # Arguments
    ///
    /// * `family` - The family
    pub async fn keys(&self, family: &Family) -> Result<Vec<String>, HarnessError> {
        // walk the cursor until it returns to zero
        let pattern = format!("{}:{}:*", self.namespace, Self::name(family)?);
        let mut conn = self.conn().await?;
        let mut cursor = 0_u64;
        let mut keys = BTreeSet::new();
        loop {
            let (next, found): (u64, Vec<String>) = redis::cmd("SCAN")
                .arg(cursor)
                .arg("MATCH")
                .arg(&pattern)
                .arg("COUNT")
                .arg(1000)
                .query_async(&mut *conn)
                .await
                .map_err(|err| HarnessError::Backend(err.to_string()))?;
            keys.extend(found);
            cursor = next;
            if cursor == 0 {
                break;
            }
        }
        Ok(keys.into_iter().collect())
    }

    /// Delete every key of a family
    ///
    /// # Arguments
    ///
    /// * `family` - The family
    pub async fn wipe(&self, family: &Family) -> Result<(), HarnessError> {
        // find the family's keys and delete them
        let keys = self.keys(family).await?;
        if keys.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn().await?;
        redis::cmd("DEL")
            .arg(&keys)
            .query_async::<()>(&mut *conn)
            .await
            .map_err(|err| HarnessError::Backend(err.to_string()))
    }

    /// Read every object of a family
    ///
    /// # Arguments
    ///
    /// * `family` - The family
    pub async fn read(&self, family: &Family) -> Result<Vec<Record>, HarnessError> {
        // read each key's hash
        let keys = self.keys(family).await?;
        let mut records = Vec::with_capacity(keys.len());
        let mut conn = self.conn().await?;
        let prefix = format!("{}:", self.namespace);
        for key in keys {
            let fields: BTreeMap<String, String> = redis::cmd("HGETALL")
                .arg(&key)
                .query_async(&mut *conn)
                .await
                .map_err(|err| HarnessError::Backend(err.to_string()))?;
            records.push(Record::Redis {
                key: key.strip_prefix(&prefix).unwrap_or(&key).to_owned(),
                fields,
            });
        }
        Ok(records)
    }

    /// Replace one object with a record
    ///
    /// # Arguments
    ///
    /// * `record` - The record to store
    pub async fn store(&self, record: &Record) -> Result<(), HarnessError> {
        // only Redis hashes are stored
        let Record::Redis { key, fields } = record else {
            return Err(HarnessError::Unsupported(
                "the harness stores Redis hashes only".to_owned(),
            ));
        };
        let key = format!("{}:{key}", self.namespace);
        // the old hash goes and the new one lands together
        let mut pipe = redis::pipe();
        pipe.atomic().del(&key).ignore();
        if !fields.is_empty() {
            let pairs: Vec<(&String, &String)> = fields.iter().collect();
            pipe.hset_multiple(&key, &pairs).ignore();
        }
        // a user is found through the namespace's user set, which the users migration reads
        let user_data = [self.namespace.as_str(), "user_data:"].join(":");
        if let Some(username) = key.strip_prefix(&user_data) {
            pipe.sadd(format!("{}:users", self.namespace), username)
                .ignore();
        }
        let mut conn = self.conn().await?;
        pipe.query_async::<()>(&mut *conn)
            .await
            .map_err(|err| HarnessError::Backend(err.to_string()))
    }
}

/// The fixture objects under a prefix of one of the test namespace's S3 buckets
#[derive(Clone)]
pub struct S3Data {
    /// The S3 client
    client: aws_sdk_s3::Client,
    /// The cluster's config, which names the bucket of each role
    conf: Arc<Conf>,
}

impl S3Data {
    /// Reach the fixture objects of the test buckets
    ///
    /// # Arguments
    ///
    /// * `client` - The S3 client
    /// * `conf` - The cluster's config
    #[must_use]
    pub fn new(client: aws_sdk_s3::Client, conf: Arc<Conf>) -> Self {
        // keep what the buckets are found from
        S3Data { client, conf }
    }

    /// The bucket and key prefix of an S3 family, or an error for any other family
    ///
    /// # Arguments
    ///
    /// * `family` - The family
    pub fn location(&self, family: &Family) -> Result<(String, &'static str), HarnessError> {
        // only a prefix of a bucket whose role the config names is seeded
        match family {
            Family::S3Prefix { bucket, prefix } => match *bucket {
                "ephemeral" => Ok((self.conf.thorium.ephemeral.bucket.clone(), prefix)),
                other => Err(HarnessError::Unsupported(format!(
                    "the harness seeds the ephemeral bucket only, not {other}"
                ))),
            },
            other => Err(HarnessError::Unsupported(format!(
                "the harness seeds Redis families and S3 prefixes only, not {other}"
            ))),
        }
    }

    /// List the keys under a family's prefix, in key order
    ///
    /// # Arguments
    ///
    /// * `family` - The family
    pub async fn keys(&self, family: &Family) -> Result<Vec<String>, HarnessError> {
        // page through the listing until it ends
        let (bucket, prefix) = self.location(family)?;
        let mut keys = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let page = self
                .client
                .list_objects_v2()
                .bucket(&bucket)
                .prefix(prefix)
                .set_continuation_token(token.take())
                .send()
                .await
                .map_err(|err| HarnessError::Backend(err.to_string()))?;
            keys.extend(
                page.contents()
                    .iter()
                    .filter_map(|object| object.key().map(str::to_owned)),
            );
            match page.next_continuation_token() {
                Some(next) => token = Some(next.to_owned()),
                None => return Ok(keys),
            }
        }
    }

    /// Delete every object under a family's prefix
    ///
    /// # Arguments
    ///
    /// * `family` - The family
    pub async fn wipe(&self, family: &Family) -> Result<(), HarnessError> {
        // delete each listed object
        let (bucket, _) = self.location(family)?;
        for key in self.keys(family).await? {
            self.client
                .delete_object()
                .bucket(&bucket)
                .key(key)
                .send()
                .await
                .map_err(|err| HarnessError::Backend(err.to_string()))?;
        }
        Ok(())
    }

    /// Write one object whose body is its own key
    ///
    /// # Arguments
    ///
    /// * `family` - The family the object belongs to
    /// * `record` - The object's record
    pub async fn store(&self, family: &Family, record: &Record) -> Result<(), HarnessError> {
        // only S3 records are stored here
        let Record::S3 { key } = record else {
            return Err(HarnessError::Unsupported(
                "an S3 prefix holds S3 records only".to_owned(),
            ));
        };
        let (bucket, _) = self.location(family)?;
        self.client
            .put_object()
            .bucket(bucket)
            .key(key)
            .body(key.clone().into_bytes().into())
            .send()
            .await
            .map_err(|err| HarnessError::Backend(err.to_string()))?;
        Ok(())
    }
}

/// The fixture objects of both backends the direct driver seeds: Redis hashes and S3 objects
#[derive(Clone)]
pub struct FixtureData {
    /// The Redis hashes
    redis: RedisData,
    /// The S3 objects
    s3: S3Data,
}

impl FixtureData {
    /// Reach the fixture objects of both backends
    ///
    /// # Arguments
    ///
    /// * `redis` - The Redis hashes
    /// * `s3` - The S3 objects
    #[must_use]
    pub fn new(redis: RedisData, s3: S3Data) -> Self {
        // one handle for both
        FixtureData { redis, s3 }
    }

    /// Delete every object of a family
    ///
    /// # Arguments
    ///
    /// * `family` - The family
    pub async fn wipe(&self, family: &Family) -> Result<(), HarnessError> {
        // each backend deletes its own
        match family {
            Family::S3Prefix { .. } => self.s3.wipe(family).await,
            _ => self.redis.wipe(family).await,
        }
    }

    /// Read every object of a family
    ///
    /// # Arguments
    ///
    /// * `family` - The family
    pub async fn read(&self, family: &Family) -> Result<Vec<Record>, HarnessError> {
        // each backend reads its own
        match family {
            Family::S3Prefix { .. } => Ok(self
                .s3
                .keys(family)
                .await?
                .into_iter()
                .map(|key| Record::S3 { key })
                .collect()),
            _ => self.redis.read(family).await,
        }
    }

    /// Write one object of a family
    ///
    /// # Arguments
    ///
    /// * `family` - The family the object belongs to
    /// * `record` - The object's record
    pub async fn store(&self, family: &Family, record: &Record) -> Result<(), HarnessError> {
        // each backend stores its own
        match family {
            Family::S3Prefix { .. } => self.s3.store(family, record).await,
            _ => self.redis.store(record).await,
        }
    }
}

/// Check that every object but the flagged ones is in a layout and holds its sentinels
///
/// # Arguments
///
/// * `records` - Every object of the family
/// * `fixture` - How the family's objects are read
/// * `verifier` - What the objects must look like
pub fn verify_layouts(
    records: &[Record],
    fixture: &FamilyFixture,
    verifier: &FixtureVerifier,
) -> Result<(), HarnessError> {
    // every object is in the layout or holds its sentinels, except the flagged ones
    let mut wrong = Vec::new();
    for record in records {
        let key = crate::models::schema::record_key(record);
        if verifier.flagged.iter().any(|flagged| flagged == key) {
            continue;
        }
        let layout = (fixture.layout_of)(record);
        let sentinels = verifier
            .sentinels
            .iter()
            .all(|(field, value)| record.text(field).as_deref() == Some(value.as_str()));
        if layout != Some(verifier.layout) || !sentinels {
            wrong.push(format!(
                "{key} (layout {:?})",
                layout.map(|layout| layout.0)
            ));
        }
    }
    if wrong.is_empty() {
        return Ok(());
    }
    Err(HarnessError::Failed(format!(
        "{} of {} objects of {} aren't in layout {}{}: {}",
        wrong.len(),
        records.len(),
        verifier.family,
        verifier.layout.0,
        if verifier.sentinels.is_empty() {
            ""
        } else {
            " with their sentinels"
        },
        wrong.iter().take(5).cloned().collect::<Vec<_>>().join(", ")
    )))
}

/// Check that an applied history holds the expected entries, in order
///
/// # Arguments
///
/// * `found` - The history
/// * `expected` - The entries it must hold
pub fn check_history(
    found: &[AppliedStep],
    expected: &[HistoryExpectation],
) -> Result<String, HarnessError> {
    // the same steps in the same order
    let render = |steps: &[AppliedStep]| {
        steps
            .iter()
            .map(|applied| format!("{}:{:?}", applied.step, applied.outcome))
            .collect::<Vec<_>>()
    };
    if found.len() != expected.len() {
        return Err(HarnessError::Failed(format!(
            "the history holds {:?}, but {} entries were expected: {:?}",
            render(found),
            expected.len(),
            expected
                .iter()
                .map(|want| format!("{}:{:?}", want.step, want.outcome))
                .collect::<Vec<_>>()
        )));
    }
    // each entry's outcome and audit fields
    for (applied, want) in found.iter().zip(expected) {
        let matches = applied.step == want.step
            && applied.outcome == want.outcome
            && applied.backup == want.backup
            && applied.confirm.is_some() == want.confirmed
            && applied.loss.is_some() == want.loss;
        if !matches {
            return Err(HarnessError::Failed(format!(
                "the history entry {}:{:?} (backup {:?}, confirmed {}, loss {}) doesn't match the \
                 expected {}:{:?} (backup {:?}, confirmed {}, loss {})",
                applied.step,
                applied.outcome,
                applied.backup,
                applied.confirm.is_some(),
                applied.loss.is_some(),
                want.step,
                want.outcome,
                want.backup,
                want.confirmed,
                want.loss
            )));
        }
    }
    Ok(format!("{} history entries", found.len()))
}
