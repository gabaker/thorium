//! Some utilities to help thoradm do what it needs to do

use scylla::client::session::Session;
use thorium::Conf;

use crate::Error;

/// Build a scylla client for a specific cluster
///
/// # Arguments
///
/// * `config` - The config for this Thorium cluster
pub async fn get_scylla_client(config: &Conf) -> Result<Session, Error> {
    // connect the way every Thorium component connects from its config
    let scylla = thorium::models::schema::connect(config).await?;
    Ok(scylla)
}
