//! Handle updating thorctl

use std::io::IsTerminal;

use crate::Args;
use thorium::models::Component;
use thorium::{Error, Thorium};

use crate::utils;

/// Determine if an updated thorctl is available
///
/// # Arguments
///
/// * `thorium` - A client for the Thorium API
pub async fn check(thorium: &Thorium) -> Result<bool, Error> {
    // get the current version of Thorium from the api
    let version = thorium.updates.get_version().await?;
    // get the current version
    let current = env!("CARGO_PKG_VERSION");
    // compare to our version and see if its different
    if version.thorium != semver::Version::parse(current)? {
        // print the version mismatch
        eprintln!("Thorctl is out of date!");
        eprintln!("{current} -> {}", version.thorium);
        return Ok(true);
    }
    Ok(false)
}

/// Check if an update is needed and then ask for permission to update
///
/// The prompt is only offered when stdin and stderr are both terminals; otherwise an
/// out-of-date client is only reported (by [`check`]) so scripted/CI runs are never
/// blocked on a prompt they can't answer.
///
/// # Arguments
///
/// * `thorium` - A client for the Thorium API
pub async fn ask_update(thorium: &Thorium) -> Result<(), Error> {
    // check if thorctl needs to be updated; this prints the out of date notice
    let outdated = check(thorium).await?;
    // dialoguer draws its prompt on stderr and reads stdin, so both must be terminals
    let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    // only prompt when an update is available and we can ask the user about it
    if outdated && interactive {
        // ask the user for permission to update Thorctl
        let response = dialoguer::Confirm::new()
            .with_prompt("Update now?:")
            .interact()?;
        if response {
            // update Thorctl
            thorium.updates.update(Component::Thorctl).await?;
            // tell the user Thorctl has updated and to rerun their command
            println!("🚀 Thorctl has been updated! Please rerun your command.");
            // exit Thorctl
            std::process::exit(0);
        }
    }
    Ok(())
}

/// Update thorctl if there exists a newer version
///
/// # Arguments
///
/// * `args` - The args passed to update
pub async fn update(args: &Args) -> Result<(), Error> {
    // load our config and instance our client
    let (conf, thorium) = utils::get_client(args).await?;
    // warn about insecure connections if not set to skip
    if !conf.skip_insecure_warning.unwrap_or_default() {
        utils::warn_insecure_conf(&conf)?;
    }
    // update thorium
    thorium.updates.update(Component::Thorctl).await?;
    // tell the user thorctl has updated and to rerun their command
    println!("🚀 Thorctl has been updated! Please rerun your command.");
    // exit thorctl
    std::process::exit(0);
}
