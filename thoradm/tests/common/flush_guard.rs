//! The guard of the tests that flush a whole Redis database
//!
//! A flush deletes every key of the database the test configuration points at, so a test that
//! flushes runs only when `THORADM_TEST_FLUSH_REDIS` names that Redis (`host:port`) exactly.
//! The variable is never set by default; a developer sets it by hand for a Redis of their own
//! and CI sets it in the job script for the job's own Redis.

/// The environment variable that must equal the `host:port` of the Redis a test would flush
pub const FLUSH_VARIABLE: &str = "THORADM_TEST_FLUSH_REDIS";

/// Decides whether a flushing test may run against a Redis
///
/// The `CI` variable has no part in the decision: only the setting and the target do.
///
/// # Arguments
///
/// * `setting` - The value of `THORADM_TEST_FLUSH_REDIS`, if it is set
/// * `target` - The `host:port` of the Redis the test configuration would flush
pub fn check_target(setting: Option<&str>, target: &str) -> Result<String, String> {
    // an unset or blank variable never allows a flush
    let Some(setting) = setting.map(str::trim).filter(|setting| !setting.is_empty()) else {
        return Err(format!(
            "this test flushes the whole Redis database at {target}; set {FLUSH_VARIABLE}={target} \
             to allow it on a Redis that holds nothing you need (it is unset or empty)"
        ));
    };
    // a value that is not exactly the target would flush a Redis the setter did not name
    if setting != target {
        return Err(format!(
            "{FLUSH_VARIABLE} is {setting:?} but this test would flush the Redis at {target}; \
             set it to exactly {target} to allow the flush"
        ));
    }
    // the setting names the target, so the flush may go ahead
    Ok(format!(
        "{FLUSH_VARIABLE} allows flushing the whole Redis database at {target}"
    ))
}

/// Checks that a Redis has a second database for the tests' marker
///
/// The tests write a marker into database `other_db` to show that a flush deletes one database
/// and not every database, so the Redis must allow `SELECT` of it.
///
/// # Arguments
///
/// * `conn` - A connection to the Redis the tests would flush
/// * `other_db` - The database number the tests write their marker into
pub async fn check_databases(
    conn: &mut redis::aio::MultiplexedConnection,
    other_db: u8,
) -> Result<(), String> {
    // select the other database and switch back to the default one in the same pipeline
    let selected: Result<(), redis::RedisError> = redis::pipe()
        .cmd("SELECT")
        .arg(other_db)
        .ignore()
        .cmd("SELECT")
        .arg(0)
        .ignore()
        .query_async(conn)
        .await;
    // a refusal means the Redis has too few databases or denies the command
    selected.map_err(|err| {
        format!(
            "these tests write a marker into Redis database {other_db}, so the Redis needs at \
             least {} databases (`databases` setting) and must allow SELECT; it answered: {err}",
            u16::from(other_db) + 1
        )
    })
}
