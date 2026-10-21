//! Utility functions for search-streamer

use scylla::client::session::Session;
use thorium::models::schema::connect;
use thorium::{Conf, Error};

/// Build a scylla client for a specific cluster
///
/// # Arguments
///
/// * `config` - The config for this Thorium cluster
pub async fn get_scylla_client(conf: &Conf) -> Result<Session, Error> {
    // connect with the shared session settings
    let scylla = connect(conf)
        .await
        .map_err(|err| Error::new(format!("Error connecting to Scylla: {err}")))?;
    Ok(scylla)
}

/// Setup a connection pool to the redis backend
///
/// # Arguments
///
/// * `config` - The config for the Thorium API
pub fn get_redis_client(conf: &Conf) -> Result<redis::Client, Error> {
    // get redis config
    let redis = &conf.redis;
    // build url to server using authentication if its configured
    let url = match (&redis.username, &redis.password) {
        // redis with username/password auth setup
        (Some(user), Some(password)) => format!(
            "redis://{}:{}@{}:{}/",
            user, password, redis.host, redis.port
        ),
        (None, Some(password)) => format!(
            "redis://default:{}@{}:{}/",
            password, redis.host, redis.port
        ),
        (None, None) => format!("redis://{}:{}/", redis.host, redis.port),
        _ => {
            return Err(Error::new(
                "Redis Setup Error: Password must be set if username is set",
            ));
        }
    };
    // build Redis client
    let client = redis::Client::open(url)
        .map_err(|err| Error::new(format!("Error connecting to Redis: {err}")))?;
    Ok(client)
}
