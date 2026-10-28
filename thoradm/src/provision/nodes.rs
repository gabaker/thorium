use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use thorium::{Error, Thorium, models::Component};
use tokio::fs;

const THORIUM_PATH: &str = "/opt/thorium";
const AGENT_PATH: &str = "/opt/thorium/thorium-agent";
const TRACING_MOUNT_PATH: &str = "/tmp/tracing.yml";
const TRACING_PATH: &str = "/opt/thorium/tracing.yml";

/// configure a thorium agent directory
#[allow(
    clippy::ptr_arg,
    reason = "public function of the thoradm library; changing the parameter type is an API change"
)]
pub async fn conf_thorium_dir(keys: &String) -> Result<(), Error> {
    println!("Creating Thorium directory: {}", THORIUM_PATH);
    // create thorium worker node base directory
    if !Path::new(THORIUM_PATH).exists() {
        fs::create_dir(THORIUM_PATH).await?
    }
    println!("Setting permissions on Thorium directory");
    // set permissions for thorium directory
    fs::set_permissions(THORIUM_PATH, Permissions::from_mode(0o755)).await?;
    // pull the agent binary form the api
    println!("Downloading latest thorium-agent from API");
    // build Thorium client from keys secret
    let thorium = Thorium::from_key_file(keys).await?;
    // copy down agent from API
    thorium
        .updates
        .update_other(Component::Agent, AGENT_PATH)
        .await?;
    println!("Setting permissions on thorium-agent after download complete");
    fs::set_permissions(AGENT_PATH, Permissions::from_mode(0o755)).await?;
    // the agent needs a tracing config file to run
    println!("Writing tracing.yml to {}", TRACING_PATH);
    // write tracing config to path
    if !Path::new(TRACING_PATH).exists() {
        // file doesn't exist, copy config from mount
        println!("Copying tracing file");
        fs::copy(TRACING_MOUNT_PATH, TRACING_PATH).await?;
    } else {
        // file does exist, backup old version before copying from mount
        println!("Backing up old tracing file");
        fs::rename(TRACING_PATH, format!("{}.bck", TRACING_PATH)).await?;
        // copy tracing file from mount point
        println!("Copying tracing file");
        fs::copy(TRACING_MOUNT_PATH, TRACING_PATH).await?;
    }
    // set permissions
    fs::set_permissions(TRACING_PATH, Permissions::from_mode(0o644)).await?;
    Ok(())
}
