use thorium::{Error, Thorium};

/// Initialize Thorium system settings
///
/// Note: initialization of settings does not override existing settings for clusters
/// that have already been provisioned.
///
/// # Arguments
///
/// * `thorium` - The Thorium client being used for API interactions
pub async fn init_settings(thorium: &Thorium) -> Result<(), Error> {
    println!("Initializing Thorium system settings");
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
