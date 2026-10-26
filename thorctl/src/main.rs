//! A CLI tool for working with Thorium

#![feature(path_file_prefix)]
#![feature(os_str_slice)]
#![feature(path_is_empty)]

use clap::Parser;
use thorium::{CtlConf, Error};

mod args;
mod errors;
mod handlers;
mod utils;

use args::{Args, SubCommands};

#[tokio::main]
async fn main() {
    // restore the terminal and remove temporary files if we are interrupted
    utils::interrupt::install();
    // get the command line args that were passed in
    let args = Args::parse();
    // fall into the right handler and execute this users command
    let thorctl_result = match &args.cmd {
        SubCommands::Login(login) => handlers::clusters::login(&args, login).await,
        SubCommands::Clusters(clusters) => handlers::clusters::handle(&args, clusters).await,
        SubCommands::Groups(groups) => handlers::groups::handle(&args, groups).await,
        SubCommands::Files(files) => handlers::files::handle(&args, files).await,
        SubCommands::Images(images) => handlers::images::handle(&args, images).await,
        SubCommands::Pipelines(pipelines) => handlers::pipelines::handle(&args, pipelines).await,
        SubCommands::Reactions(reactions) => handlers::reactions::handle(&args, reactions).await,
        SubCommands::Results(results) => handlers::results::handle(&args, results).await,
        SubCommands::Tokens(tokens) => handlers::tokens::handle(&args, tokens).await,
        SubCommands::Activate(activate) => handlers::tokens::activate(&args, activate).await,
        SubCommands::Deactivate => handlers::tokens::deactivate(&args),
        SubCommands::Tags(tags) => handlers::tags::handle(&args, tags).await,
        SubCommands::Repos(repos) => handlers::repos::handle(&args, repos).await,
        SubCommands::Trees(trees) => handlers::trees::handle(&args, trees).await,
        SubCommands::Associations(associations) => {
            handlers::associations::handle(&args, associations).await
        }
        SubCommands::NetworkPolicies(network_policies) => {
            handlers::network_policies::handle(&args, network_policies).await
        }
        SubCommands::Tools(tools) => handlers::tools::handle(&args, tools).await,
        SubCommands::Models(models) => handlers::models::handle(&args, models).await,
        SubCommands::Agents(agents) => handlers::agents::handle(&args, agents).await,
        SubCommands::ToolSets(tool_sets) => handlers::toolsets::handle(&args, tool_sets).await,
        SubCommands::Workflows(workflows) => handlers::workflows::handle(&args, workflows).await,
        SubCommands::Instances(instances) => handlers::instances::handle(&args, instances).await,
        SubCommands::Runs(runs) => handlers::runs::handle(&args, runs).await,
        SubCommands::Cart(cart) => handlers::cart::handle(&args, cart).await,
        SubCommands::Uncart(uncart) => handlers::uncart::handle(&args, uncart).await,
        SubCommands::Run(run) => handlers::run::handle(&args, run).await,
        SubCommands::Update => handlers::update::update(&args).await,
        SubCommands::Config(config) => handlers::config::config(&args, config),
        SubCommands::Toolbox(toolbox) => handlers::toolbox::handle(&args, toolbox).await,
    };
    // a prompt that failed or ended on a key press may have left the cursor hidden
    utils::interrupt::restore_terminal();
    // error if thorctl failed
    if let Err(error) = thorctl_result {
        // a prompt that read Ctrl-C as a key press ends like an interrupt, without an error dump
        if utils::interrupt::was_interrupted(&error) {
            utils::interrupt::exit_interrupted();
        }
        // print our error to stderr nicely if possible
        match error {
            Error::Generic(msg) => eprintln!("{msg}"),
            _ => eprintln!("{error:#?}"),
        }
        // exit with the timeout code if a --wait-for timed out, or 1 otherwise
        std::process::exit(utils::watch::exit_code());
    }
}
