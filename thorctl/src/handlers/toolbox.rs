//! Handles toolbox commands

use thorium::{CtlConf, Error, Thorium};

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

/// Builds an API client for the toolbox subcommands that talk to Thorium
///
/// This also runs the insecure-connection warning and the client/server version
/// update check, both of which can be disabled through the ctl config (and the
/// update check through `--skip-update`). The update check only prompts when a
/// terminal is attached (see [`crate::handlers::update::ask_update`]).
///
/// # Arguments
///
/// * `args` - The top-level thorctl args
async fn connect(args: &Args) -> Result<(CtlConf, Thorium), Error> {
    // load the full ctl config and build a client from it
    let (conf, thorium) = utils::get_client(args).await?;
    // warn about an insecure (non-TLS / untrusted) connection unless the config
    // explicitly opts out of the warning
    if !conf.skip_insecure_warning.unwrap_or_default() {
        utils::warn_insecure_conf(&conf)?;
    }
    // run the version check unless either the CLI flag or the config disables it
    if !args.skip_update && !conf.skip_update.unwrap_or_default() {
        crate::handlers::update::ask_update(&thorium).await?;
    }
    Ok((conf, thorium))
}

/// Dispatches a toolbox subcommand to its handler
///
/// # Arguments
///
/// * `args` - The top-level thorctl args
/// * `toolbox` - The toolbox subcommand to execute
pub async fn handle(args: &Args, toolbox: &Toolbox) -> Result<(), Error> {
    // only the subcommands that talk to the API build a client (and run the
    // insecure/update checks); build, init and build-images are purely local
    match toolbox {
        Toolbox::Build(cmd) => {
            // build walks the tree with synchronous std::fs, so run it on a blocking
            // thread; clone the command because the spawned task must own its input
            let cmd = cmd.clone();
            // a JoinError here means the blocking task itself panicked, which is
            // surfaced as an error rather than propagating the panic out of the runtime
            tokio::task::spawn_blocking(move || build::build(&cmd))
                .await
                .map_err(|err| Error::new(format!("Toolbox build task panicked: {err}")))?
        }
        Toolbox::Init(cmd) => init::handle(cmd, args).await,
        Toolbox::BuildImages(cmd) => {
            // build-images needs a docker/podman runtime but no API client, so load the
            // ctl config best-effort purely to read `container_runtime`; if the config is
            // missing or unreadable, fall back to the flag then PATH auto-detection
            let configured_runtime = CtlConf::from_path(&args.config)
                .ok()
                .and_then(|conf| conf.container_runtime);
            // resolve and cache the runtime (flag -> config -> PATH) before any build runs
            super::container::init_runtime(args.container_runtime, configured_runtime);
            build_images::build_images(cmd).await
        }
        Toolbox::Import(cmd) => {
            // build a client and run the connection checks
            let (conf, thorium) = connect(args).await?;
            // resolve the runtime up front so pushing any bundled image tarballs has a
            // CLI ready; non-bundled imports simply never invoke it
            super::container::init_runtime(args.container_runtime, conf.container_runtime);
            // the import future is large, so box it rather than keeping it on the stack
            Box::pin(import::import(thorium, conf, cmd, args.workers)).await
        }
        Toolbox::Export(cmd) => {
            // build a client and run the connection checks
            let (conf, thorium) = connect(args).await?;
            // resolve the runtime up front so a `--with-images` export can pull/save
            // tarballs; a plain export simply never invokes it
            super::container::init_runtime(args.container_runtime, conf.container_runtime);
            export::export(thorium, cmd, args, &conf).await
        }
        Toolbox::Remove(cmd) => {
            // build a client and run the connection checks
            let (conf, thorium) = connect(args).await?;
            remove::remove(thorium, conf, cmd).await
        }
        Toolbox::Diff(cmd) => {
            // build a client and run the connection checks
            let (conf, thorium) = connect(args).await?;
            // translate "drift detected" into a `git diff --exit-code`-style non-zero
            // exit here at the dispatch boundary; diff() returns normally so its own
            // resources (progress bar, buffers) are dropped before the process exits
            if diff::diff(thorium, &conf, cmd).await? {
                std::process::exit(1);
            }
            Ok(())
        }
    }
}
