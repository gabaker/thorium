//! The shared scylla utilties for Thoradm

use scylla::client::session::Session;
use scylla::client::session_builder::SessionBuilder;
#[cfg(feature = "test-support")]
use scylla::policies::address_translator::AddressTranslator;
#[cfg(feature = "test-support")]
use std::sync::Arc;
use std::time::Duration;
use thorium::Conf;

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
    // start building our scylla client
    let mut session = SessionBuilder::new();
    // if we have auth info for scylla then add that
    if let Some(creds) = &config.scylla.auth {
        // inject our creds
        session = session.user(&creds.username, &creds.password);
    }
    // set our request timeout
    let session =
        session.connection_timeout(Duration::from_secs(u64::from(config.scylla.setup_time)));
    // route through a test's proxy when one was registered for these nodes
    #[cfg(feature = "test-support")]
    let session = translate(session, config);
    // add every configured node as a known node
    config
        .scylla
        .nodes
        .iter()
        .fold(session, |builder, node| builder.known_node(node))
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
