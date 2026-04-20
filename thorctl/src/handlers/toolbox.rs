//! Handles toolbox commands

use thorium::Error;

mod build;
mod categorize;
mod create;
pub(crate) mod editor;
mod export;
mod import;
pub(crate) mod init;
mod manifest;
pub(crate) mod merge;
mod prompt;
mod shared;
pub(crate) mod update;

use crate::args::Args;
use crate::args::toolbox::Toolbox;
use crate::utils;

pub async fn handle(args: &Args, toolbox: &Toolbox) -> Result<(), Error> {
    // handle commands that don't need API access
    if let Toolbox::Build(cmd) = toolbox {
        return build::build(cmd);
    }
    if let Toolbox::Init(cmd) = toolbox {
        return init::handle(cmd).await;
    }
    // load our config and instance our client
    let (conf, thorium) = utils::get_client(args).await?;
    // warn about insecure connections if not set to skip
    if !conf.skip_insecure_warning.unwrap_or_default() {
        utils::warn_insecure_conf(&conf)?;
    }
    // check if we need to update
    if !args.skip_update && !conf.skip_update.unwrap_or_default() {
        crate::handlers::update::ask_update(&thorium).await?;
    }
    match toolbox {
        Toolbox::Import(cmd) => import::import(thorium, conf, cmd).await,
        Toolbox::Export(cmd) => export::export(thorium, cmd).await,
        Toolbox::Build(_) | Toolbox::Init(_) => unreachable!(),
    }
}
