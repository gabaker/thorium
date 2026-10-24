//! The shared scylla utilties for Thoradm

use scylla::client::session::Session;
use scylla::client::session_builder::SessionBuilder;
use scylla::errors::{
    ConnectionError, ConnectionPoolError, ConnectionSetupRequestErrorKind, DbError, MetadataError,
    NewSessionError,
};
#[cfg(feature = "test-support")]
use scylla::policies::address_translator::AddressTranslator;
#[cfg(feature = "test-support")]
use std::sync::Arc;
use std::time::{Duration, Instant};
use thorium::Conf;
use thorium::models::schema::session_builder;
use tracing::warn;

mod controller;
mod ranges;
mod reader;

pub use controller::{ScyllaCrawlController, ScyllaCrawlSupport};
pub use ranges::build_token_ranges;
pub use reader::{Page, RangeReadError, RangeReader};

use crate::Error;

/// The address translators tests register for the proxies they put in front of Scylla, by the
/// proxy's address
#[cfg(feature = "test-support")]
static TRANSLATORS: std::sync::Mutex<Vec<(String, Arc<dyn AddressTranslator>)>> =
    std::sync::Mutex::new(Vec::new());

/// Routes every session built for a config that lists `node` through `translator`
///
/// The driver learns each node's address from `system.local` and `system.peers` and connects to
/// it directly, which would bypass a proxy sitting in front of the node. A registered translator
/// maps those addresses, and the shard-aware port is turned off, so every connection goes to the
/// translated address. Only thoradm's own tests compile this, through the `test-support`
/// feature.
///
/// # Arguments
///
/// * `node` - The node address a test's config lists, usually the proxy's
/// * `translator` - Maps each address a node advertises to the address to connect to
///
/// # Panics
///
/// Panics if another test panicked while registering a translator.
#[cfg(feature = "test-support")]
#[doc(hidden)]
pub fn route_through(node: &str, translator: Arc<dyn AddressTranslator>) {
    // a newer registration for the same address replaces the older one
    let mut translators = TRANSLATORS.lock().unwrap();
    translators.retain(|(registered, _)| registered != node);
    translators.push((node.to_owned(), translator));
}

/// Applies the translator a test registered for one of a config's nodes, if any
///
/// # Arguments
///
/// * `session` - The session being built
/// * `config` - The config whose nodes may have a registered translator
#[cfg(feature = "test-support")]
fn translate(session: SessionBuilder, config: &Conf) -> SessionBuilder {
    // find a translator registered for any of the config's nodes
    let translators = TRANSLATORS.lock().unwrap();
    let found = translators
        .iter()
        .find(|(node, _)| config.scylla.nodes.contains(node))
        .map(|(_, translator)| translator.clone());
    drop(translators);
    // route every connection through it and the node's regular port
    match found {
        Some(translator) => session
            .address_translator(translator)
            .disallow_shard_aware_port(true),
        None => session,
    }
}

/// Start building a scylla client for a specific cluster
///
/// # Arguments
///
/// * `config` - The config for this Thorium cluster
fn builder(config: &Conf) -> SessionBuilder {
    // start from the shared session settings for these nodes
    let session = session_builder(config);
    // route through a test's proxy when one was registered for these nodes
    #[cfg(feature = "test-support")]
    let session = translate(session, config);
    session
}

/// Build a scylla client for a specific cluster
///
/// # Arguments
///
/// * `config` - The config for this Thorium cluster
pub async fn get_client(config: &Conf) -> Result<Session, Error> {
    // build a scylla session
    let scylla = builder(config).build().await?;
    Ok(scylla)
}

/// How long to wait between attempts to connect while waiting for Scylla
const CONNECT_RETRY: Duration = Duration::from_secs(3);

/// Build a scylla client, retrying until the wait runs out
///
/// # Arguments
///
/// * `config` - The config for this Thorium cluster
/// * `wait` - How many seconds to keep retrying, if any
pub async fn connect_waiting(config: &Conf, wait: Option<u64>) -> Result<Session, Error> {
    // stop retrying once the wait has passed
    let deadline = Instant::now() + Duration::from_secs(wait.unwrap_or_default());
    loop {
        // try once, and again after a pause while the wait lasts
        match get_client(config).await {
            Ok(session) => return Ok(session),
            Err(err) if Instant::now() < deadline => {
                warn!("Scylla isn't accepting connections yet: {err}");
                tokio::time::sleep(CONNECT_RETRY).await;
            }
            Err(Error::ScyllaNewSession(err)) => {
                return Err(Error::Transient(format!("Scylla can't be reached: {err}")));
            }
            Err(err) => return Err(err),
        }
    }
}

/// Check whether a failed session means Scylla rejected Thorium's login rather than being
/// unreachable
///
/// # Arguments
///
/// * `error` - The error the session failed with
#[must_use]
pub fn is_auth_error(error: &NewSessionError) -> bool {
    // get the last error of the control connection behind a failed metadata fetch
    let NewSessionError::MetadataError(MetadataError::ConnectionPoolError(
        ConnectionPoolError::Broken {
            last_connection_error: ConnectionError::ConnectionSetupRequestError(setup),
        },
    )) = error
    else {
        return false;
    };
    // check why the control connection's login failed
    is_auth_setup_error(&setup.error)
}

/// Check whether a failed connection setup request means Scylla rejected the credentials
///
/// # Arguments
///
/// * `kind` - Why the connection setup request failed
fn is_auth_setup_error(kind: &ConnectionSetupRequestErrorKind) -> bool {
    // Scylla answered the login with an authentication error or demanded credentials
    matches!(
        kind,
        ConnectionSetupRequestErrorKind::DbError(DbError::AuthenticationError, _)
            | ConnectionSetupRequestErrorKind::MissingAuthentication
    )
}

#[cfg(test)]
mod tests {
    use scylla::errors::{ConnectionSetupRequestErrorKind, DbError, NewSessionError};

    use super::{is_auth_error, is_auth_setup_error};

    /// A login Scylla rejects, or one it demands credentials for, is an authentication failure
    /// and nothing else a connection can fail with is
    ///
    /// The driver's `ConnectionSetupRequestError` can't be built outside the driver, so the
    /// classification of its kind is tested apart from the error that wraps it.
    #[test]
    fn only_a_rejected_login_is_an_authentication_failure() {
        // a rejected login and a missing one
        assert!(is_auth_setup_error(
            &ConnectionSetupRequestErrorKind::DbError(
                DbError::AuthenticationError,
                "bad credentials".to_owned()
            )
        ));
        assert!(is_auth_setup_error(
            &ConnectionSetupRequestErrorKind::MissingAuthentication
        ));
        // an overloaded node and a role without a privilege are other failures
        assert!(!is_auth_setup_error(
            &ConnectionSetupRequestErrorKind::DbError(DbError::Overloaded, "busy".to_owned())
        ));
        assert!(!is_auth_setup_error(
            &ConnectionSetupRequestErrorKind::DbError(DbError::Unauthorized, "denied".to_owned())
        ));
        // an error that isn't a failed login at all
        assert!(!is_auth_error(&NewSessionError::EmptyKnownNodesList));
    }
}
