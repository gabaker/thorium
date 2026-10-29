//! A CLI tool for working with Thorium

#![feature(os_str_slice)]

use clap::Parser;
use thorium::{CtlConf, Error};

mod args;
mod errors;
mod handlers;
mod utils;

use args::{Args, SubCommands};

/// Render an error that reached `main` as the text printed to stderr
///
/// Generic errors print as their message, errors that carry an HTTP status print on one line
/// as `Code: <status> Error: <msg>`, and any other error prints in its debug form.
///
/// # Arguments
///
/// * `error` - The error to render
fn render_error(error: &Error) -> String {
    // print generic errors as plain text
    if let Error::Generic(msg) = error {
        return msg.clone();
    }
    // print errors with an HTTP status in the client's one line display form
    if error.status().is_some() {
        return error.to_string();
    }
    // fall back to the debug form for every other kind of error
    format!("{error:#?}")
}

/// Installs the interrupt handler, parses the command line, runs the matching Thorctl command,
/// and exits with a non-zero code after printing the error if the command fails
#[tokio::main]
async fn main() {
    // restore the terminal and remove temporary files if we are interrupted
    utils::interrupt::install();
    // get the command line args that were passed in
    let args = Args::parse();
    // print a completion script before any config or client is loaded, like `--help` does
    if let SubCommands::Completions(completions) = &args.cmd {
        if let Err(error) = handlers::completions::handle(completions) {
            eprintln!("{error:#?}");
            std::process::exit(1);
        }
        return;
    }
    // fall into the right handler and execute this users command
    let thorctl_result = match &args.cmd {
        SubCommands::Login(login) => handlers::clusters::login(&args, login).await,
        SubCommands::Clusters(clusters) => handlers::clusters::handle(&args, clusters).await,
        SubCommands::Groups(groups) => handlers::groups::handle(&args, groups).await,
        SubCommands::Files(files) => handlers::files::handle(&args, files).await,
        SubCommands::Grep(grep) => handlers::grep::handle(&args, grep).await,
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
        // completions were already printed above, before the config was needed
        SubCommands::Completions(_) => Ok(()),
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
        eprintln!("{}", render_error(&error));
        // exit with the timeout code if a --wait-for timed out, or 1 otherwise
        std::process::exit(utils::watch::exit_code());
    }
}

#[cfg(test)]
mod tests {
    use http::StatusCode;
    use thorium::Error;

    use super::render_error;

    /// An API error renders on one line with its status code and message
    #[test]
    fn render_api_error_on_one_line() {
        // build an error like the one the client returns for an unauthorized request
        let error = Error::Thorium {
            code: StatusCode::UNAUTHORIZED,
            msg: Some("{\"trace\":\"x\"}".to_string()),
        };
        // an API error should print as the client's display text rather than a debug struct
        assert_eq!(
            render_error(&error),
            "Code: 401 Unauthorized Error: {\"trace\":\"x\"}"
        );
    }

    /// A generic error renders as its plain message
    #[test]
    fn render_generic_error_as_text() {
        // a generic error should print without any prefix or quoting
        assert_eq!(
            render_error(&Error::Generic("no such file".to_string())),
            "no such file"
        );
    }

    /// An error without an HTTP status keeps its debug form
    #[test]
    fn render_other_error_as_debug() {
        // build an IO error that has no HTTP status
        let error = Error::IO(std::io::Error::other("disk gone"));
        // an error with no status should print in its debug form
        assert_eq!(render_error(&error), format!("{error:#?}"));
        assert!(render_error(&error).starts_with("IO("));
    }
}
