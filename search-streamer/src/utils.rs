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
    // build url to the configured database using authentication if its configured
    let url = match (&redis.username, &redis.password) {
        // redis with username/password auth setup
        (Some(user), Some(password)) => format!(
            "redis://{}:{}@{}:{}/{}",
            user, password, redis.host, redis.port, redis.db
        ),
        (None, Some(password)) => format!(
            "redis://default:{}@{}:{}/{}",
            password, redis.host, redis.port, redis.db
        ),
        (None, None) => format!("redis://{}:{}/{}", redis.host, redis.port, redis.db),
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use thorium::Conf;

    use super::get_redis_client;

    /// The Redis client selects the configured database, whatever authentication is configured,
    /// and a username without a password is refused
    #[test]
    fn redis_client_selects_the_configured_database() {
        // the API's config template as a base config
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../api/thorium-template.yml");
        let raw = std::fs::read_to_string(&path).expect("the config template reads");
        let mut conf: Conf = serde_norway::from_str(&raw).expect("the template parses");
        // a database other than the default one on a host that parses as a url
        conf.redis.host = "localhost".to_owned();
        conf.redis.db = 3;
        // no authentication, a password alone, and a username with a password
        for (username, password) in [(None, None), (None, Some("pw")), (Some("user"), Some("pw"))] {
            conf.redis.username = username.map(str::to_owned);
            conf.redis.password = password.map(str::to_owned);
            // the client's connection info names the configured database
            let client = get_redis_client(&conf).expect("a client builds");
            assert_eq!(client.get_connection_info().redis.db, 3);
        }
        // a username without a password can't authenticate
        conf.redis.username = Some("user".to_owned());
        conf.redis.password = None;
        assert!(get_redis_client(&conf).is_err());
    }
}
