//! Cheap checks that Thorium's own credentials work before its config is written
//!
//! These run on every reconcile so a bad credential is reported in the `ThoriumCluster`'s
//! status instead of crash looping the components. A backend that isn't accepting
//! connections yet is reported as [`CheckError::Waiting`] so the cluster stays provisioning.

use redis::{
    AsyncConnectionConfig, ConnectionAddr, ConnectionInfo, ErrorKind, RedisConnectionInfo,
    RedisError,
};
use std::time::Duration;
use thorium::Error;

use super::helpers::{CheckError, error_chain};
use crate::k8s::clusters::ClusterMeta;

/// How long to wait when connecting to or querying a backend
const CHECK_TIMEOUT: Duration = Duration::from_secs(10);

/// Make sure Thorium can log in to Scylla with its own role
///
/// Connecting and logging in are bounded by an overall timeout. A Scylla that isn't
/// accepting connections yet is waited on while a rejected login is a failure.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
pub async fn scylla_login(meta: &ClusterMeta) -> Result<(), CheckError> {
    // get the nodes to connect to
    let nodes = &meta.conf.scylla.nodes;
    // log in as Thorium's role when one is configured
    if let Some(auth) = &meta.conf.scylla.auth {
        super::bootstrap::scylla_connect(nodes, &auth.username, &auth.password).await?;
        return Ok(());
    }
    // without a role Thorium connects anonymously, so make sure that works
    super::bootstrap::scylla_connect_anonymous(nodes).await?;
    Ok(())
}

/// Build the Redis connection info for Thorium's config
///
/// A password without a username authenticates as the `default` user, matching the API.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
fn redis_info(meta: &ClusterMeta) -> Result<ConnectionInfo, Error> {
    // get the redis config
    let conf = &meta.conf.redis;
    // the API refuses a username without a password, so do the same
    if conf.username.is_some() && conf.password.is_none() {
        return Err(Error::new(
            "redis.password must be set when redis.username is set",
        ));
    }
    // a password alone authenticates as the default user
    let username = conf.password.as_ref().map(|_| {
        conf.username
            .clone()
            .unwrap_or_else(|| "default".to_owned())
    });
    // build the connection info without a url so passwords need no escaping
    Ok(ConnectionInfo {
        addr: ConnectionAddr::Tcp(conf.host.clone(), conf.port),
        redis: RedisConnectionInfo {
            username,
            password: conf.password.clone(),
            ..RedisConnectionInfo::default()
        },
    })
}

/// Check whether a Redis error means Redis isn't up yet rather than rejecting us
///
/// A refused, dropped, or timed out connection and a Redis still loading its dataset are
/// waited on, while anything Redis answered with (such as a rejected `AUTH`) is not.
///
/// # Arguments
///
/// * `error` - The error Redis failed with
fn redis_unreachable(error: &RedisError) -> bool {
    // connection level failures and a Redis loading its dataset will resolve on their own
    error.is_io_error() || error.kind() == ErrorKind::BusyLoadingError
}

/// Classify a failed Redis request as waiting on Redis or as a failure
///
/// # Arguments
///
/// * `addr` - Where Redis lives, for the description
/// * `failure` - A description of the failure used when Redis answered
/// * `error` - The error Redis failed with
fn redis_check_error(addr: &str, failure: &str, error: &RedisError) -> CheckError {
    // describe the error with every cause
    let chain = error_chain(error);
    // only a Redis that isn't up yet is waited on
    if redis_unreachable(error) {
        CheckError::Waiting(format!(
            "Waiting for Redis at {addr} to accept connections: {chain}"
        ))
    } else {
        CheckError::Failed(Error::new(format!("{failure}: {chain}")))
    }
}

/// Make sure Redis accepts Thorium's credentials
///
/// Connecting sends `AUTH` with Thorium's credentials, and a `PING` confirms the
/// connection can run commands.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
pub async fn redis_ping(meta: &ClusterMeta) -> Result<(), CheckError> {
    // get where redis lives for errors
    let addr = format!("{}:{}", meta.conf.redis.host, meta.conf.redis.port);
    // build a client for Thorium's redis
    let client = redis::Client::open(redis_info(meta)?).map_err(|error| {
        Error::new(format!(
            "Invalid Redis config for {addr}: {}",
            error_chain(&error)
        ))
    })?;
    // bound connecting and each command
    let config = AsyncConnectionConfig::new()
        .set_connection_timeout(CHECK_TIMEOUT)
        .set_response_timeout(CHECK_TIMEOUT);
    // connect and authenticate
    let mut connection = client
        .get_multiplexed_async_connection_with_config(&config)
        .await
        .map_err(|error| {
            redis_check_error(
                &addr,
                &format!("Failed to connect to Redis at {addr}"),
                &error,
            )
        })?;
    // make sure this connection can run commands
    redis::cmd("PING")
        .query_async::<String>(&mut connection)
        .await
        .map_err(|error| {
            redis_check_error(&addr, &format!("Redis at {addr} rejected PING"), &error)
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Redis connection failures and loading are waited on while rejections are failures
    #[test]
    fn redis_errors_classified() {
        // a refused, dropped, or timed out connection is waited on
        for kind in [
            std::io::ErrorKind::ConnectionRefused,
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::TimedOut,
        ] {
            let error = RedisError::from(std::io::Error::new(kind, "io"));
            let classified = redis_check_error("redis:6379", "Failed", &error);
            let CheckError::Waiting(details) = classified else {
                panic!("{kind:?} should be waited on");
            };
            assert!(details.starts_with("Waiting for Redis at redis:6379 to accept connections: "));
        }
        // a Redis still loading its dataset is waited on
        let loading = RedisError::from((ErrorKind::BusyLoadingError, "loading"));
        assert!(redis_unreachable(&loading));
        // rejected credentials need attention
        let auth = RedisError::from((ErrorKind::AuthenticationFailed, "WRONGPASS"));
        let CheckError::Failed(error) = redis_check_error("redis:6379", "Failed to connect", &auth)
        else {
            panic!("rejected credentials should be a failure");
        };
        assert!(error.to_string().contains("Failed to connect: "));
        // any other answer from Redis needs attention too
        let response = RedisError::from((ErrorKind::ResponseError, "NOPERM"));
        assert!(!redis_unreachable(&response));
    }
}
