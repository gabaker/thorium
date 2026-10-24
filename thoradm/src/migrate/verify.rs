//! `thoradm migrate verify`: loads every migrated object through the API's own deserialization

use serde_json::json;
use thorium::models::schema::{RunLimits, RunMode, RunOptions, verify_pass};

use super::context::{self, migrate_error};
use super::registry::Support;
use crate::Error;
use crate::args::{Args, MigrationTarget};
use crate::outcome::RunOutcome;

/// Verify a data migration: every object must load, so none may be left in an old layout or
/// unreadable
///
/// # Arguments
///
/// * `cmd` - The migration to verify
/// * `args` - The arguments thoradm was started with
pub(super) async fn handle(cmd: &MigrationTarget, args: &Args) -> Result<RunOutcome, Error> {
    // a migration thoradm can't run reports how to check it by hand
    let def = super::find(cmd.id())?;
    // a migration that can only be checked is refused before anything is read
    if def.support != Support::Yes {
        return super::run_or_verify(cmd, "verify");
    }
    // load every object of the family
    let env = context::connect(args).await?;
    let ctx = context::context(
        &env,
        RunMode::Check,
        context::cancel_on_signal(),
        RunLimits::default(),
    );
    let opts = RunOptions {
        slices: 4,
        ..RunOptions::default()
    };
    let run = verify_pass(def.runner, &opts, &ctx)
        .await
        .map_err(migrate_error)?;
    println!(
        "{}",
        json!({"migration": def.id, "checked": run.checked, "failures": run.failures})
    );
    // any object that doesn't load fails the verification
    if run.failures.is_empty() {
        Ok(RunOutcome::Complete)
    } else {
        Err(Error::DataIssues(format!(
            "{} of {} objects of {} don't load",
            run.failures.values().sum::<u64>(),
            run.checked,
            def.id
        )))
    }
}
