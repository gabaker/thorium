//! Handle the different ai commands for Thorctl

use thorium::{CtlConf, Error};

mod batch;
mod chat;
mod check;
mod defang;
mod headless;
mod pricing;
mod session;
mod setup;
mod summary;
mod telemetry;
mod tui;
mod usage;

use crate::args::Args;
use crate::args::ai::Ai;

/// handle all ai commands for Thorctl
///
/// # Arguments
///
/// * `args` - The base Thorctl arguments
/// * `cmd` - The ai command to run
pub async fn handle(args: &Args, cmd: &Ai) -> Result<(), Error> {
    // handle the correct command; each arm loads only the config it needs so
    // config-free commands like `batch figures` run on boxes with no thorctl
    // config at all
    match cmd {
        Ai::Chat(chat) => {
            // get our thorctl config
            let conf = CtlConf::from_path(&args.config)?;
            // reject the flag combinations clap cannot express before any work
            if let Err(error) = chat.validate() {
                return Err(Error::new(error));
            }
            // an opening prompt routes the chat to the headless runner instead
            // of the TUI
            if chat.is_headless() {
                headless::handle(&conf, chat, &args.config).await
            } else {
                chat::tui(&conf, chat, &args.config).await
            }
        }
        Ai::Summary(summary) => {
            // get our thorctl config
            let conf = CtlConf::from_path(&args.config)?;
            // summarize the target binary with ThorPal
            summary::handle(&conf, summary, &args.config).await
        }
        Ai::Check => {
            // get our thorctl config
            let conf = CtlConf::from_path(&args.config)?;
            // round trip a trivial prompt through the configured provider
            check::handle(&conf).await
        }
        // explore global usage/cost stats; the handler loads config
        // best-effort itself so the viewer runs config-free
        Ai::Usage(cmd) => usage::handle(args, cmd).await,
        // walk through ThorPal setup; the handler loads config best-effort
        // itself because the wizard may be creating the very first config
        Ai::Setup(cmd) => setup::handle(args, cmd).await,
        Ai::Batch(cmd) => batch::handle(args, cmd).await,
        // export privacy-safe telemetry bundles; the handler loads config
        // best-effort itself so boxes with no thorctl config can still export
        Ai::Telemetry(cmd) => telemetry::handle(args, cmd).await,
    }
}
