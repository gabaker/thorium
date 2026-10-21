//! The settings related features for Thoradm

use thorium::Thorium;

use crate::{
    Error,
    args::{Args, ResetSettings, SettingsSubCommands, UpdateSettings},
};

/// Get Thorium `SystemSettings` and print them to stdout
///
/// # Arguments
///
/// * `thorium` - The Thorium client
async fn get(thorium: Thorium) -> Result<(), Error> {
    // get the system settings
    let settings = thorium.system.get_settings().await?;
    // print them to stdout in the given format
    serde_json::to_writer_pretty(std::io::stdout(), &settings)?;
    // print the ending newline
    println!();
    Ok(())
}

/// Update Thorium `SystemSettings`
///
/// # Arguments
///
/// * `cmd` - The update command to apply
/// * `thorium` - The Thorium client
async fn update(cmd: &UpdateSettings, thorium: Thorium) -> Result<(), Error> {
    // update Thorium system settings
    thorium
        .system
        .update_settings(&cmd.to_settings_update(), &cmd.to_params())
        .await?;
    Ok(())
}

/// Reset Thorium `SystemSettings` to default
///
/// # Arguments
///
/// * `cmd` - The reset command to apply
/// * `thorium` - The Thorium client
async fn reset(cmd: &ResetSettings, thorium: Thorium) -> Result<(), Error> {
    // reset Thorium system settings
    thorium.system.reset_settings(&cmd.to_params()).await?;
    Ok(())
}

/// Perform a manual consistency scan based on the current Thorium `SystemSettings`
///
/// # Arguments
///
/// * `thorium` - The Thorium client
async fn scan(thorium: Thorium) -> Result<(), Error> {
    // run a consistency scan
    thorium.system.consistency_scan().await?;
    Ok(())
}

/// Handle the settings command
///
/// # Arguments
///
/// * `sub` - The settings subcommand
/// * `args` - The Thoradm args
pub async fn handle(sub: &SettingsSubCommands, args: &Args) -> Result<(), Error> {
    let thorium = Thorium::from_ctl_conf_file(&args.ctl_conf).await?;
    match sub {
        SettingsSubCommands::Get => get(thorium).await,
        SettingsSubCommands::Update(cmd) => update(cmd, thorium).await,
        SettingsSubCommands::Reset(cmd) => reset(cmd, thorium).await,
        SettingsSubCommands::Scan => scan(thorium).await,
    }
}
