//! Handles toolbox commands

use thorium::Error;

mod build;
mod build_images;
mod collisions;
mod diff;
mod export;
mod import;
pub(crate) mod init;
mod manifest;
pub(crate) mod policies;
mod prompt;
mod remove;
mod shared;

use crate::args::Args;
use crate::args::toolbox::Toolbox;
use crate::utils;

/// Dispatches a toolbox subcommand to its handler
///
/// # Arguments
///
/// * `args` - The top-level thorctl args
/// * `toolbox` - The toolbox subcommand to execute
pub async fn handle(args: &Args, toolbox: &Toolbox) -> Result<(), Error> {
    // handle commands that don't need API access
    if let Toolbox::Build(cmd) = toolbox {
        // build walks the tree with synchronous std::fs, so run it off the async
        // runtime (mirrors the spawn_blocking wrap in export and diff)
        let cmd = cmd.clone();
        return tokio::task::spawn_blocking(move || build::build(&cmd))
            .await
            .map_err(|err| Error::new(format!("Toolbox build task panicked: {err}")))?;
    }
    if let Toolbox::Init(cmd) = toolbox {
        return init::handle(cmd, args).await;
    }
    if let Toolbox::BuildImages(cmd) = toolbox {
        // resolve the container runtime (docker/podman); an offline build loads no
        // config, so only the flag and PATH auto-detection apply
        super::container::init_runtime(args.container_runtime, None);
        return build_images::build_images(cmd).await;
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
        Toolbox::Import(cmd) => {
            // resolve the container runtime (docker/podman) before pushing bundled images
            super::container::init_runtime(args.container_runtime, conf.container_runtime);
            import::import(thorium, conf, cmd, args.workers).await
        }
        Toolbox::Export(cmd) => {
            // resolve the container runtime (docker/podman) before bundling images
            super::container::init_runtime(args.container_runtime, conf.container_runtime);
            export::export(thorium, cmd, args, &conf).await
        }
        Toolbox::Remove(cmd) => remove::remove(thorium, conf, cmd).await,
        Toolbox::Diff(cmd) => {
            // exit non-zero on drift (git diff --exit-code) at the dispatch
            // boundary so diff() returns normally and its resources drop first
            if diff::diff(thorium, cmd).await? {
                std::process::exit(1);
            }
            Ok(())
        }
        Toolbox::Build(_) | Toolbox::Init(_) | Toolbox::BuildImages(_) => unreachable!(),
    }
}
