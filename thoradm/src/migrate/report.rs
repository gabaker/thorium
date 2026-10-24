//! `thoradm migrate report`: prints what a migration's last pass applied

use super::context::{self, migrate_error};
use crate::Error;
use crate::args::{Args, ReportMigration};
use crate::outcome::RunOutcome;

/// Print the applied report stored in a migration's pass row
///
/// # Arguments
///
/// * `cmd` - The migration whose report to print
/// * `args` - The arguments thoradm was started with
pub(super) async fn handle(cmd: &ReportMigration, args: &Args) -> Result<RunOutcome, Error> {
    // read the pass row's report, which only a pass that ended has
    let def = super::find(&cmd.id)?;
    let env = context::connect(args).await?;
    let row = env.store.pass_row(def.pass).await.map_err(migrate_error)?;
    match row.and_then(|row| row.report) {
        Some(report) => {
            println!("{}", serde_json::to_string(&report)?);
            Ok(RunOutcome::Complete)
        }
        None => Err(Error::new(format!(
            "{} has no applied report; its last pass hasn't ended",
            cmd.id
        ))),
    }
}
