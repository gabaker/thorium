//! Performs admin tasks for Thorium

use clap::Parser;
use std::process::ExitCode;
use thoradm::outcome::RunOutcome;
use thoradm::{args, backup, census, migrate, provision, schema, settings, users};
use tracing::error;

/// Runs one thoradm command and exits with its outcome's code
#[tokio::main]
async fn main() -> ExitCode {
    // load command line args, mapping usage errors to an abort so exit 2 only means failures
    let args = match args::Args::try_parse() {
        Ok(args) => args,
        Err(error) => {
            // help and version requests print to stdout and succeed
            let code = if error.use_stderr() {
                RunOutcome::Aborted.code()
            } else {
                0
            };
            let _ = error.print();
            return ExitCode::from(code);
        }
    };
    // send every error and summary to stderr
    thoradm::shared::progress::init_logging();
    // execute the right handler; backup, census, migrate, and schema commands return their own
    // outcome so they can exit with a nonzero code
    let outcome = match &args.cmd {
        args::SubCommands::Backup(backup_cmd) => backup::handle(backup_cmd, &args).await,
        args::SubCommands::Settings(settings_cmd) => settings::handle(settings_cmd, &args)
            .await
            .map(|()| RunOutcome::Complete),
        args::SubCommands::Provision(provision_args) => provision::handle(provision_args)
            .await
            .map(|()| RunOutcome::Complete),
        args::SubCommands::Census(census_cmd) => census::handle(census_cmd, &args).await,
        args::SubCommands::Users(users_cmd) => users::handle(users_cmd, &args)
            .await
            .map(|()| RunOutcome::Complete),
        args::SubCommands::Migrate(migrate_cmd) => migrate::handle(migrate_cmd, &args).await,
        args::SubCommands::Schema(schema_cmd) => schema::handle(schema_cmd, &args).await,
    };
    // an error that ended the command early is an abort
    match outcome {
        Ok(outcome) => outcome.into(),
        Err(err) => {
            error!("{err}");
            // exit with the code for this kind of error
            ExitCode::from(err.exit_code())
        }
    }
}
