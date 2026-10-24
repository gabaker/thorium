//! `thoradm migrate issues`: lists the issue groups a migration recorded

use thorium::models::schema::issue_groups;

use super::context::{self, migrate_error};
use crate::Error;
use crate::args::{Args, ListIssues};
use crate::outcome::RunOutcome;

/// List the issue groups a migration recorded, or one group's sample keys
///
/// # Arguments
///
/// * `cmd` - The migration and, optionally, the group to show
/// * `args` - The arguments thoradm was started with
pub(super) async fn handle(cmd: &ListIssues, args: &Args) -> Result<RunOutcome, Error> {
    // read the current attempt's groups, summed across workers
    let def = super::find(&cmd.id)?;
    let env = context::connect(args).await?;
    let groups = issue_groups(&env.store, def.pass)
        .await
        .map_err(migrate_error)?;
    match &cmd.reason {
        // one group: its code, class, counts, acceptance, and sample keys, sorted
        Some(reason) => {
            let mut found = false;
            for group in groups.iter().filter(|group| &group.reason == reason) {
                found = true;
                println!(
                    "{} class={} count={} resolved={} accepted={}",
                    group.reason,
                    group.class.as_str(),
                    group.count,
                    group.resolved,
                    group.accepted
                );
                for key in &group.samples {
                    println!("  {key}");
                }
            }
            if !found {
                return Err(Error::new(format!(
                    "{} has no issue group {reason}",
                    cmd.id
                )));
            }
        }
        // every group, one line each
        None => {
            for group in &groups {
                println!(
                    "{} class={} count={} resolved={} accepted={}",
                    group.reason,
                    group.class.as_str(),
                    group.count,
                    group.resolved,
                    group.accepted
                );
            }
        }
    }
    Ok(RunOutcome::Complete)
}
