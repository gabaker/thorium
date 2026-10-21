//! The one way Thorium's components connect to Scylla from their config

use scylla::client::session::Session;
use scylla::client::session_builder::{GenericSessionBuilder, SessionBuilder};
use scylla::errors::NewSessionError;
use std::time::Duration;

use crate::Conf;

/// Build a Scylla session builder from the config's nodes, credentials, and setup time
///
/// # Arguments
///
/// * `conf` - The Thorium config
#[must_use]
pub fn session_builder(conf: &Conf) -> SessionBuilder {
    // start from the driver's defaults
    let mut builder = SessionBuilder::new();
    // authenticate when the config holds credentials
    if let Some(creds) = &conf.scylla.auth {
        builder = builder.user(&creds.username, &creds.password);
    }
    // bound each connection attempt by the configured setup time
    let builder =
        builder.connection_timeout(Duration::from_secs(u64::from(conf.scylla.setup_time)));
    // connect to every configured node
    conf.scylla
        .nodes
        .iter()
        .fold(builder, GenericSessionBuilder::known_node)
}

/// Open a Scylla session from the config
///
/// # Arguments
///
/// * `conf` - The Thorium config
// the driver's own error is returned as is, so callers convert it the way they already do
#[allow(clippy::result_large_err)]
pub async fn connect(conf: &Conf) -> Result<Session, NewSessionError> {
    session_builder(conf).build().await
}
