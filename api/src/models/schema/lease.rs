//! The namespace's writer lease: at most one writing thoradm command at a time
//!
//! The lease is `SET {ns}:schema-writer <holder>|<token> NX PX <ttl>` in Redis, renewed by a
//! heartbeat while it is held. The token is a fresh UUID per acquisition and doubles as a fencing
//! token: a worker writes it with each checkpoint batch and checks before each batch that the
//! lease still holds it, so a worker whose lease was broken or lost stops at its next batch. The
//! lease guards writers against each other; it isn't a schema marker.

use bb8_redis::RedisConnectionManager;
use bb8_redis::bb8::Pool;
use std::fmt;
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::warn;
use uuid::Uuid;

/// How long the lease lives without a heartbeat, in milliseconds
pub const LEASE_TTL_MS: u64 = 30_000;

/// How often the heartbeat renews the lease
const HEARTBEAT: Duration = Duration::from_millis(LEASE_TTL_MS / 3);

/// Why a lease couldn't be taken
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseError {
    /// Another writer holds the lease
    Held {
        /// The holder the lease names
        holder: String,
    },
    /// Redis couldn't be reached or refused the command
    Backend(String),
}

impl fmt::Display for LeaseError {
    /// Write the error for the person who ran the command
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // name the holder so the person can find the other writer
        match self {
            LeaseError::Held { holder } => write!(
                f,
                "the namespace's writer lease is held by {holder}; wait for it to finish"
            ),
            LeaseError::Backend(message) => {
                write!(f, "the writer lease couldn't be taken: {message}")
            }
        }
    }
}

impl std::error::Error for LeaseError {}

/// The Redis key a namespace's lease lives at
///
/// # Arguments
///
/// * `ns` - The namespace
#[must_use]
pub fn lease_key(ns: &str) -> String {
    // the key is namespaced like every other Thorium Redis key
    format!("{ns}:schema-writer")
}

/// The holder a lease value names
///
/// # Arguments
///
/// * `value` - The stored value, `<holder>|<token>`
fn holder_of(value: &str) -> &str {
    // the token is a UUID, so the last bar splits it from a holder that has bars of its own
    value.rsplit_once('|').map_or(value, |(holder, _)| holder)
}

/// A held writer lease, kept alive by a heartbeat until it is released or dropped
pub struct WriterLease {
    /// The Redis pool the lease lives in
    redis: Pool<RedisConnectionManager>,
    /// The Redis key
    key: String,
    /// The value this holder wrote: `<holder>|<token>`
    value: String,
    /// The holder's name
    holder: String,
    /// The fencing token
    token: String,
    /// Cancelled when the heartbeat finds the lease gone or taken
    lost: CancellationToken,
    /// The heartbeat task
    heartbeat: JoinHandle<()>,
}

/// Renew a lease only while it still holds this holder's value
const RENEW: &str = "if redis.call('GET', KEYS[1]) == ARGV[1] then \
                     return redis.call('PEXPIRE', KEYS[1], ARGV[2]) else return 0 end";

/// Take a lease over only while it names this holder, whose value ends in a 36-character token
const RETAKE: &str = "local cur = redis.call('GET', KEYS[1]); \
                      if cur and string.sub(cur, 1, #cur - 37) == ARGV[1] then \
                      redis.call('SET', KEYS[1], ARGV[2], 'PX', ARGV[3]); return 1 else return 0 end";

/// Delete a lease only while it still holds this holder's value
const RELEASE: &str = "if redis.call('GET', KEYS[1]) == ARGV[1] then \
                       return redis.call('DEL', KEYS[1]) else return 0 end";

impl WriterLease {
    /// Take the namespace's writer lease
    ///
    /// # Arguments
    ///
    /// * `redis` - The Redis pool the lease lives in
    /// * `ns` - The namespace
    /// * `holder` - Who takes it: a Job's name, or the host and OS user of a manual run
    /// * `force` - Take it even though another writer holds it, which makes that writer's
    ///   workers stop at their next batch; a holder that already holds it always takes it again
    pub async fn acquire(
        redis: &Pool<RedisConnectionManager>,
        ns: &str,
        holder: &str,
        force: bool,
    ) -> Result<Self, LeaseError> {
        // build the value this holder writes, with a fresh fencing token
        let key = lease_key(ns);
        let token = Uuid::new_v4().to_string();
        let value = format!("{holder}|{token}");
        let mut conn = redis
            .get()
            .await
            .map_err(|err| LeaseError::Backend(err.to_string()))?;
        // take the lease only while nobody holds it, unless the holder breaks it
        let mut set = redis::cmd("SET");
        set.arg(&key).arg(&value);
        if !force {
            set.arg("NX");
        }
        set.arg("PX").arg(LEASE_TTL_MS);
        let reply: Option<String> = set
            .query_async(&mut *conn)
            .await
            .map_err(|err| LeaseError::Backend(err.to_string()))?;
        // an unset reply means somebody else holds it, unless it is this very holder's earlier hold,
        // such as a retried pod of the same Job, which takes it over at once
        if reply.is_none() && !force {
            let own: i64 = redis::cmd("EVAL")
                .arg(RETAKE)
                .arg(1)
                .arg(&key)
                .arg(holder)
                .arg(&value)
                .arg(LEASE_TTL_MS)
                .query_async(&mut *conn)
                .await
                .map_err(|err| LeaseError::Backend(err.to_string()))?;
            if own == 1 {
                return Ok(Self::held(redis, key, value, holder, token));
            }
        }
        // still unset means somebody else holds it; name them
        if reply.is_none() {
            let current: Option<String> = redis::cmd("GET")
                .arg(&key)
                .query_async(&mut *conn)
                .await
                .map_err(|err| LeaseError::Backend(err.to_string()))?;
            return Err(LeaseError::Held {
                holder: current
                    .as_deref()
                    .map_or("another writer", holder_of)
                    .to_owned(),
            });
        }
        drop(conn);
        Ok(Self::held(redis, key, value, holder, token))
    }

    /// Start keeping a lease that was just written alive
    ///
    /// # Arguments
    ///
    /// * `redis` - The Redis pool the lease lives in
    /// * `key` - The Redis key
    /// * `value` - The value this holder wrote
    /// * `holder` - The holder's name
    /// * `token` - The fencing token
    fn held(
        redis: &Pool<RedisConnectionManager>,
        key: String,
        value: String,
        holder: &str,
        token: String,
    ) -> Self {
        // renew the lease in the background, noting when it is lost
        let lost = CancellationToken::new();
        let heartbeat = tokio::spawn(Self::beat(
            redis.clone(),
            key.clone(),
            value.clone(),
            lost.clone(),
        ));
        WriterLease {
            redis: redis.clone(),
            key,
            value,
            holder: holder.to_owned(),
            token,
            lost,
            heartbeat,
        }
    }

    /// Renew the lease until it is lost or the task is aborted
    ///
    /// # Arguments
    ///
    /// * `redis` - The Redis pool the lease lives in
    /// * `key` - The Redis key
    /// * `value` - The value this holder wrote
    /// * `lost` - Cancelled when the lease is found gone or taken
    async fn beat(
        redis: Pool<RedisConnectionManager>,
        key: String,
        value: String,
        lost: CancellationToken,
    ) {
        loop {
            // renew every third of the lease's life
            tokio::time::sleep(HEARTBEAT).await;
            let renewed: Result<i64, String> = async {
                let mut conn = redis.get().await.map_err(|err| err.to_string())?;
                redis::cmd("EVAL")
                    .arg(RENEW)
                    .arg(1)
                    .arg(&key)
                    .arg(&value)
                    .arg(LEASE_TTL_MS)
                    .query_async(&mut *conn)
                    .await
                    .map_err(|err| err.to_string())
            }
            .await;
            match renewed {
                // the lease still holds this holder's value
                Ok(1) => {}
                // somebody broke it or it expired
                Ok(_) => {
                    warn!("the writer lease was lost; stopping at the next batch");
                    lost.cancel();
                    return;
                }
                // a failed renewal is retried until the lease would have expired anyway
                Err(err) => warn!("the writer lease couldn't be renewed: {err}"),
            }
        }
    }

    /// The fencing token to write with each checkpoint batch
    #[must_use]
    pub fn token(&self) -> &str {
        // the token is fixed for the life of the lease
        &self.token
    }

    /// The holder's name
    #[must_use]
    pub fn holder(&self) -> &str {
        // the holder is fixed for the life of the lease
        &self.holder
    }

    /// A token cancelled when the heartbeat finds the lease lost
    #[must_use]
    pub fn lost(&self) -> CancellationToken {
        // the token is shared with the heartbeat
        self.lost.clone()
    }

    /// Whether the lease still holds this holder's value, read from Redis now
    pub async fn is_current(&self) -> Result<bool, String> {
        // a broken lease holds another holder's value, or none
        let mut conn = self.redis.get().await.map_err(|err| err.to_string())?;
        let current: Option<String> = redis::cmd("GET")
            .arg(&self.key)
            .query_async(&mut *conn)
            .await
            .map_err(|err| err.to_string())?;
        Ok(current.as_deref() == Some(self.value.as_str()))
    }

    /// Give the lease up, so the next writer needn't wait for it to expire
    pub async fn release(self) {
        // stop renewing, then delete the key only while it is still this holder's
        self.heartbeat.abort();
        if let Ok(mut conn) = self.redis.get().await {
            let released: Result<i64, _> = redis::cmd("EVAL")
                .arg(RELEASE)
                .arg(1)
                .arg(&self.key)
                .arg(&self.value)
                .query_async(&mut *conn)
                .await;
            if let Err(err) = released {
                warn!("the writer lease couldn't be released: {err}");
            }
        }
    }
}

impl Drop for WriterLease {
    /// Stop renewing a lease that is dropped without being released; it expires on its own
    fn drop(&mut self) {
        // the heartbeat must not outlive the holder
        self.heartbeat.abort();
    }
}
