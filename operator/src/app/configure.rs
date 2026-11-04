use thorium::{Error, Thorium};
use tracing::{Level, event, instrument};

/// Initialize Thorium system settings
///
/// Note: initialization of settings does not override existing settings for clusters
/// that have already been provisioned.
///
/// # Arguments
///
/// * `thorium` - The Thorium client being used for API interactions
#[instrument(name = "configure::init_settings", skip_all)]
pub async fn init_settings(thorium: &Thorium) -> Result<(), Error> {
    // log that the default settings are being created
    event!(Level::INFO, msg = "Initializing Thorium system settings");
    // ask the API to create any default settings that don't exist yet
    let result = thorium.system.init().await?;
    // the API answers a successful init with no content
    if result.status() == 204 {
        Ok(())
    } else {
        Err(Error::new(format!(
            "Failed to init system settings: {}",
            result.status()
        )))
    }
}
