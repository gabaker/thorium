//! The ban commands for Thoradm

use bb8_redis::RedisConnectionManager;
use bb8_redis::bb8::Pool;
use thorium::Conf;
use thorium::models::backends::entities::tool_bans::{self, LegacyImageBans};
use thorium::utils::Shared;

use crate::Error;
use crate::args::{Args, BansSubCommands};
use crate::outcome::RunOutcome;

/// Handle a ban command
///
/// # Arguments
///
/// * `cmd` - The ban subcommand to execute
/// * `args` - The Thoradm args
pub async fn handle(cmd: &BansSubCommands, args: &Args) -> Result<RunOutcome, Error> {
    // dispatch to the right ban subcommand
    match cmd {
        BansSubCommands::Audit => Box::pin(audit_cmd(args)).await,
    }
}

/// List every legacy image with bans, printing one JSON line per image and a summary
///
/// # Arguments
///
/// * `args` - The Thoradm args
async fn audit_cmd(args: &Args) -> Result<RunOutcome, Error> {
    // connect to the cluster the config names
    let conf = crate::migrate::context::load_conf(args)?;
    let redis = crate::shared::redis::get_client(&conf)
        .await
        .map_err(|err| Error::Transient(format!("Redis can't be reached: {err}")))?;
    let audits = Box::pin(audit(conf, redis)).await?;
    // one JSON line per image on stdout, so the list can be piped to jq
    for entry in &audits {
        println!("{}", serde_json::to_string(entry)?);
    }
    // summarize on stderr
    let uncovered = audits.iter().filter(|entry| !entry.covered).count();
    eprintln!(
        "{} legacy image(s) with bans; {uncovered} not covered (an instance of the converted tool lacks a ban it should carry or can't be read)",
        audits.len()
    );
    Ok(RunOutcome::Complete)
}

/// List every legacy image with bans, every instance of the tool it converted into, and which of
/// its bans each instance lacks
///
/// Reads only: nothing is changed.
///
/// # Arguments
///
/// * `conf` - The cluster's config
/// * `redis` - The cluster's Redis
pub async fn audit(
    conf: Conf,
    redis: Pool<RedisConnectionManager>,
) -> Result<Vec<LegacyImageBans>, Error> {
    // read through the same code the API uses, without starting an API
    let shared = Box::pin(Shared::for_offline_writer(conf, redis))
        .await
        .map_err(|err| Error::Transient(format!("the cluster can't be read: {err}")))?;
    tool_bans::audit(&shared)
        .await
        .map_err(|err| Error::Generic(format!("the ban audit failed: {err}")))
}
