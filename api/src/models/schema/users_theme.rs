//! The runner of the users example's migrations, which move a user's theme from a flat `theme`
//! field of the user's hash into the `settings` field
//!
//! Users live only in Redis: the set `{ns}:users` lists the usernames and each user is the hash
//! `{ns}:user_data:{username}`. A pass splits the usernames into slices by a stable hash of the
//! name, reads each slice in sorted order so its checkpoint is the last username done, and
//! converts one user at a time with the example's transform, writing the two fields it changes in
//! one transaction.

use async_trait::async_trait;
use bb8_redis::RedisConnectionManager;
use bb8_redis::bb8::PooledConnection;
use serde_json::{Value, json};
use std::collections::BTreeMap;

use super::{
    Batch, IssueReport, MigrateError, MigrationRunner, ObjectClass, Outcome, RunContext, RunMode,
    Slice, record_family_layout, record_key,
};
use crate::models::UserSettings;
use crate::models::upgrades::{
    IssueCategory, IssueReason, ReasonBuilder, Record, RecordClass, USERS_SETTINGS_THEME,
    USERS_SETTINGS_THEME_REVERSE_STEP, USERS_SETTINGS_THEME_STEP,
};

/// The runner of a users theme migration
///
/// Every users migration leaves users in layout 2, the layout the first Helm revision and every
/// release after it read: a user already in layout 2 is current, and a user stored with a flat
/// theme (layout 1) has its theme moved into its settings. The
/// migrations differ in the step whose name they carry on the issues they record.
#[derive(Debug, Clone, Copy)]
pub struct UsersTheme {
    /// The id of the step this migration performs, which names the issues it records
    step: &'static str,
}

/// The migration that moves each user's theme into its settings
pub const USERS_THEME_UP: UsersTheme = UsersTheme {
    step: USERS_SETTINGS_THEME_STEP,
};

/// The migration of the way back to the first Helm revision from the revision after it, which
/// leaves each user in the layout that revision reads
pub const USERS_THEME_REVERSE: UsersTheme = UsersTheme {
    step: USERS_SETTINGS_THEME_REVERSE_STEP,
};

/// Hash a username to the slice that reads it
///
/// FNV-1a, written out here so the slice a user belongs to never depends on the standard
/// library's hasher, which a resumed pass may have been built with a different version of.
///
/// # Arguments
///
/// * `name` - The username
fn slice_of(name: &str) -> u64 {
    // fold each byte into the 64 bit FNV-1a state
    name.bytes().fold(0xcbf2_9ce4_8422_2325, |state, byte| {
        (state ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Say a Redis command failed
///
/// # Arguments
///
/// * `err` - The Redis error, whose timeouts and lost connections the runner retries
fn backend(err: &impl std::fmt::Display) -> MigrateError {
    // the runner retries a backend error with fewer writes in flight
    MigrateError::Backend(err.to_string())
}

/// Build the commands that store a converted user's theme fields in one transaction
///
/// The two fields a conversion changes are set when the converted user holds them and removed
/// when it doesn't, so a theme that moved out of `settings`, or a `settings` field left empty,
/// doesn't linger.
///
/// # Arguments
///
/// * `record` - The converted user's hash
/// * `key` - The hash's full key, with the namespace
#[must_use]
pub fn write_commands(record: &Record, key: &str) -> redis::Pipeline {
    // MULTI ... EXEC around both fields
    let mut pipe = redis::pipe();
    pipe.atomic();
    // the settings field is set when it holds something and removed otherwise
    match record.text("settings") {
        Some(settings) => pipe
            .cmd("HSET")
            .arg(key)
            .arg("settings")
            .arg(settings)
            .ignore(),
        None => pipe.cmd("HDEL").arg(key).arg("settings").ignore(),
    };
    // the flat theme is set when the user holds it there and removed otherwise
    match record.text("theme") {
        Some(theme) => pipe.cmd("HSET").arg(key).arg("theme").arg(theme).ignore(),
        None => pipe.cmd("HDEL").arg(key).arg("theme").ignore(),
    };
    pipe
}

/// Check that a user loads the way the API loads it
///
/// The `settings` field is read as the `UserSettings` the API's own loading of a user
/// deserializes, so a user the API can't load fails with the field's reason code. A user that
/// still holds a flat `theme` field fails too: the conversion didn't finish with it.
///
/// # Arguments
///
/// * `record` - The user's hash
pub fn verify_user(record: &Record) -> Result<(), IssueReason> {
    // only a Redis hash holds a user
    let Record::Redis { fields, .. } = record else {
        return Err(IssueReason::new(IssueCategory::InvalidValue, ""));
    };
    // a user with no settings loads as the default, and any other must parse as settings
    if let Some(settings) = fields.get("settings") {
        ReasonBuilder::parse::<UserSettings>(settings, "settings")?;
    }
    // a flat theme is the form the conversion removes
    if fields.contains_key("theme") {
        return Err(IssueReason::new(IssueCategory::UnknownField, ""));
    }
    Ok(())
}

impl UsersTheme {
    /// Get a Redis connection
    ///
    /// # Arguments
    ///
    /// * `ctx` - The run's context
    async fn conn(
        ctx: &RunContext,
    ) -> Result<PooledConnection<'_, RedisConnectionManager>, MigrateError> {
        // a pooled connection from the run's backends
        ctx.backends.redis.get().await.map_err(|err| backend(&err))
    }

    /// Read the hashes of users into records, skipping a name whose hash is gone
    ///
    /// # Arguments
    ///
    /// * `names` - The usernames to read
    /// * `ctx` - The run's context
    async fn read(names: &[String], ctx: &RunContext) -> Result<Vec<Record>, MigrateError> {
        // nothing to read for no names
        if names.is_empty() {
            return Ok(Vec::new());
        }
        // read every hash in one round trip
        let mut conn = Self::conn(ctx).await?;
        let mut pipe = redis::pipe();
        for name in names {
            pipe.cmd("HGETALL")
                .arg(format!("{}:user_data:{name}", ctx.backends.namespace));
        }
        let hashes: Vec<BTreeMap<String, String>> = pipe
            .query_async(&mut *conn)
            .await
            .map_err(|err| backend(&err))?;
        // a name in the index whose hash was removed holds no user
        Ok(names
            .iter()
            .zip(hashes)
            .filter(|(_, fields)| !fields.is_empty())
            .map(|(name, fields)| Record::Redis {
                key: format!("user_data:{name}"),
                fields,
            })
            .collect())
    }
}

#[async_trait]
impl MigrationRunner for UsersTheme {
    /// Split the users into `n` slices by a stable hash of the username
    ///
    /// # Arguments
    ///
    /// * `n` - How many slices to make
    /// * `ctx` - The run's context
    async fn slices(&self, n: u32, ctx: &RunContext) -> Result<Vec<Slice>, MigrateError> {
        // a slice names which part of the hash space it reads and how many parts there are
        let _ = ctx;
        let count = n.max(1);
        Ok((0..count)
            .map(|index| Slice {
                index,
                start: json!(index),
                end: json!(count),
            })
            .collect())
    }

    /// Read the next batch of a slice: the users after the last username done, in sorted order
    ///
    /// # Arguments
    ///
    /// * `slice` - The slice to read
    /// * `after` - The last username done, or the slice's start
    /// * `ctx` - The run's context
    async fn next_batch(
        &self,
        slice: &Slice,
        after: &Value,
        ctx: &RunContext,
    ) -> Result<Batch, MigrateError> {
        // list every username, keep this slice's, and sort them
        let mut conn = Self::conn(ctx).await?;
        let mut names: Vec<String> = redis::cmd("SMEMBERS")
            .arg(format!("{}:users", ctx.backends.namespace))
            .query_async(&mut *conn)
            .await
            .map_err(|err| backend(&err))?;
        drop(conn);
        let count = slice.end.as_u64().unwrap_or(1).max(1);
        let index = slice.start.as_u64().unwrap_or(0);
        names.retain(|name| slice_of(name) % count == index);
        names.sort();
        // start after the last username done; a slice's start is a number, not a username
        if let Some(last) = after.as_str() {
            names.retain(|name| name.as_str() > last);
        }
        // take one batch, and note whether the slice has more
        let rows = ctx.limits.batch_rows.max(1);
        let done = names.len() <= rows;
        names.truncate(rows);
        let next = names
            .last()
            .map_or_else(|| after.clone(), |last| Value::String(last.clone()));
        let records = Self::read(&names, ctx).await?;
        Ok(Batch {
            records,
            next,
            done,
        })
    }

    /// Classify one user and, when it converts, return the converted user
    ///
    /// # Arguments
    ///
    /// * `record` - The user's hash
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
        let class = (USERS_SETTINGS_THEME.classify)(record);
        // build the issue of a user that can't be converted
        let issue = |reason: IssueReason| {
            Outcome::Issue(IssueReport {
                key: record_key(record).to_owned(),
                transform: self.step.to_owned(),
                class: ObjectClass::Unrecognized,
                reason,
            })
        };
        Ok(match class {
            RecordClass::Current => Outcome::Current,
            RecordClass::Unrecognized { reason } => issue(reason),
            RecordClass::Migratable { .. } => match (USERS_SETTINGS_THEME.up)(record.clone()) {
                Ok(converted) => Outcome::Write(converted),
                Err(failed) => issue(failed.reason),
            },
        })
    }

    /// Record the layout the users are in now, once every slice is done
    ///
    /// The engine calls this only when the pass doesn't fail, so the record of the users layout
    /// advances exactly when the data has.
    ///
    /// # Arguments
    ///
    /// * `ctx` - The run's context
    async fn finish(&self, ctx: &RunContext) -> Result<(), MigrateError> {
        // write the layout the users are in now into the Redis backend row
        record_family_layout(
            &ctx.backends.scylla,
            &ctx.backends.conf,
            USERS_SETTINGS_THEME.family,
            USERS_SETTINGS_THEME.to,
        )
        .await
        .map_err(|err| backend(&err))
    }

    /// Store a converted user's theme fields in one transaction
    ///
    /// # Arguments
    ///
    /// * `record` - The converted user
    /// * `ctx` - The run's context
    async fn write(&self, record: &Record, ctx: &RunContext) -> Result<(), MigrateError> {
        // the record's key is relative to the namespace
        let key = format!("{}:{}", ctx.backends.namespace, record_key(record));
        let mut conn = Self::conn(ctx).await?;
        write_commands(record, &key)
            .query_async::<()>(&mut *conn)
            .await
            .map_err(|err| backend(&err))
    }

    /// Read the named users directly, for `migrate run --objects`
    ///
    /// # Arguments
    ///
    /// * `keys` - The users' keys, as `IssueReport::key` renders them (`user_data:<name>`)
    /// * `ctx` - The run's context
    async fn fetch(
        &self,
        keys: &[String],
        ctx: &RunContext,
    ) -> Result<Option<Vec<Record>>, MigrateError> {
        // a key without the prefix names no user of this family
        let names = keys
            .iter()
            .filter_map(|key| key.strip_prefix("user_data:").map(str::to_owned))
            .collect::<Vec<_>>();
        Ok(Some(Self::read(&names, ctx).await?))
    }

    /// Check that a migrated user loads
    ///
    /// # Arguments
    ///
    /// * `record` - The user's hash
    /// * `ctx` - The run's context
    async fn verify(&self, record: &Record, ctx: &RunContext) -> Result<(), IssueReason> {
        // a migrated user is one the API can load
        let _ = ctx;
        verify_user(record)
    }
}
