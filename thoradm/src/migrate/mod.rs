//! Lists, plans, and runs the data migrations behind Thorium's upgrade revisions
//!
//! The revisions and their steps come from the catalog shared with the operator
//! (`thorium::models::upgrades`). Steps that change stored data name a thoradm migration,
//! which the operator will eventually run in a pod it spawns. No migration is implemented
//! yet: each one is registered in [`registry`] and reports the manual procedure instead.

use thorium::models::upgrades::{self, CATALOG, RevisionId, StepDef, UpgradeState};

use crate::Error;
use crate::args::{Args, MigrateSubCommands, MigrationTarget, PlanMigrations};
use crate::outcome::RunOutcome;

mod check;
mod issues;
mod record;
pub mod registry;
mod reset_state;
mod revert;
mod run;
mod verify;

use registry::{MIGRATIONS, MigrationDef};

/// Find a registered migration by its id
///
/// # Arguments
///
/// * `id` - The id of the migration
fn find(id: &str) -> Result<&'static MigrationDef, Error> {
    // look the migration up among the registered ones
    MIGRATIONS
        .iter()
        .find(|migration| migration.id == id)
        .ok_or_else(|| {
            Error::new(format!(
                "Unknown migration {id}; see `thoradm migrate list`"
            ))
        })
}

/// Find the catalog step a migration performs
///
/// # Arguments
///
/// * `id` - The id of the migration
fn step_for(id: &str) -> Option<&'static StepDef> {
    // search every revision's steps for the one naming this migration
    CATALOG
        .iter()
        .flat_map(|entry| entry.steps.iter())
        .find(|step| step.thoradm_migration == Some(id))
}

/// Parse a revision id from the command line
///
/// # Arguments
///
/// * `raw` - The revision id to parse
fn revision(raw: &str) -> Result<RevisionId, Error> {
    // parse the id, turning a malformed one into a thoradm error
    raw.parse()
        .map_err(|error: upgrades::UpgradeError| Error::new(error.to_string()))
}

/// Describe the migration a step uses, if any
///
/// # Arguments
///
/// * `step` - The step to describe
fn migration_note(step: &StepDef) -> String {
    // describe the migration and whether thoradm can run it
    match step.thoradm_migration {
        Some(id) => match MIGRATIONS.iter().find(|migration| migration.id == id) {
            Some(migration) if migration.implemented => format!(", migration {id}"),
            Some(_) => format!(", migration {id} (not implemented; manual procedure)"),
            None => format!(", migration {id} (not registered in this thoradm)"),
        },
        None => String::new(),
    }
}

/// Build the lines describing every revision in the catalog and its steps
fn list_lines() -> Vec<String> {
    // collect the lines for each revision, oldest first
    let mut lines = Vec::new();
    for entry in CATALOG {
        // describe the revision
        lines.push(format!(
            "{} (released with Thorium {}): {}",
            entry.id, entry.released_with, entry.description
        ));
        // describe each of its steps
        for step in entry.steps {
            // note steps that need an approval with a backup
            let approval = if step.kind.needs_approval() {
                ", needs approval + backup"
            } else {
                ""
            };
            // note the components that must be paused for a step
            let pause = if step.pause.is_empty() {
                String::new()
            } else {
                let names = step
                    .pause
                    .iter()
                    .map(|component| component.deployment_name())
                    .collect::<Vec<_>>();
                format!(", scales down {}", names.join(", "))
            };
            // describe the step with its notes
            lines.push(format!(
                "  - {} [{:?}{approval}{pause}{}]: {}",
                step.id,
                step.kind,
                migration_note(step),
                step.description
            ));
        }
    }
    lines
}

/// Print every revision in the catalog and its steps
fn list() {
    // print each line of the catalog listing
    for line in list_lines() {
        println!("{line}");
    }
}

/// Build the lines describing the steps that bring a cluster from one revision to another
///
/// # Arguments
///
/// * `cmd` - The revisions to plan between
fn plan_lines(cmd: &PlanMigrations) -> Result<Vec<String>, Error> {
    // parse both revisions, defaulting the target to the latest one
    let from = revision(&cmd.from)?;
    let to = match &cmd.to {
        Some(raw) => revision(raw)?,
        None => upgrades::latest(),
    };
    // plan from a state at the starting revision
    let state = UpgradeState::new(from.clone(), env!("CARGO_PKG_VERSION"));
    let steps = upgrades::plan(&state, &to).map_err(|error| Error::new(error.to_string()))?;
    // there may be nothing between the two revisions
    if steps.is_empty() {
        return Ok(vec![format!("Nothing to do between {from} and {to}")]);
    }
    // describe each planned step under a header
    let mut lines = vec![format!("Upgrading from {from} to {to} runs:")];
    lines.extend(steps.iter().map(|planned| {
        format!(
            "  {} {} [{:?}]: {}",
            planned.revision, planned.step, planned.kind, planned.description
        )
    }));
    Ok(lines)
}

/// Print the steps that bring a cluster from one revision to another
///
/// # Arguments
///
/// * `cmd` - The revisions to plan between
fn plan(cmd: &PlanMigrations) -> Result<(), Error> {
    // build the plan, then print each of its lines
    for line in plan_lines(cmd)? {
        println!("{line}");
    }
    Ok(())
}

/// Report that a migration can't run yet along with how to perform it by hand
///
/// # Arguments
///
/// * `migration` - The migration that isn't implemented
/// * `action` - What was asked of it ("run" or "verify")
fn not_implemented(migration: &MigrationDef, action: &str) -> Error {
    // include the manual procedure from the catalog step it performs
    let procedure = step_for(migration.id)
        .and_then(|step| step.manual_procedure)
        .unwrap_or("No manual procedure is documented.");
    Error::NotImplemented(format!(
        "thoradm can't {action} the {:?} migration {} yet. Perform it by hand: {procedure}",
        migration.backend, migration.id
    ))
}

/// Run or verify a data migration
///
/// # Arguments
///
/// * `cmd` - The migration to run or verify
/// * `action` - What to do with it ("run" or "verify")
fn run_or_verify(cmd: &MigrationTarget, action: &str) -> Result<RunOutcome, Error> {
    // find the migration, every one of which is still a stub
    let migration = find(cmd.id())?;
    // report how to perform it by hand since thoradm can't run it yet
    Err(not_implemented(migration, action))
}

/// Report that a migrate subcommand isn't built yet
///
/// # Arguments
///
/// * `command` - The subcommand, as typed after `thoradm migrate`
fn not_built(command: &str) -> Error {
    // name the subcommand so scripts can fall back to a manual procedure
    Error::NotImplemented(format!("thoradm migrate {command} is not available yet"))
}

/// Handle a migrate subcommand
///
/// # Arguments
///
/// * `cmd` - The migrate subcommand to handle
/// * `args` - The arguments thoradm was started with
pub async fn handle(cmd: &MigrateSubCommands, args: &Args) -> Result<RunOutcome, Error> {
    // dispatch to the subcommand's handler
    match cmd {
        MigrateSubCommands::List => {
            list();
            Ok(RunOutcome::Complete)
        }
        MigrateSubCommands::Plan(plan_cmd) => plan(plan_cmd).map(|()| RunOutcome::Complete),
        MigrateSubCommands::Run(run_cmd) => run::handle(run_cmd, args).await,
        MigrateSubCommands::Check(check_cmd) => check::handle(check_cmd, args).await,
        MigrateSubCommands::Record(record_cmd) => record::handle(record_cmd, args).await,
        MigrateSubCommands::Verify(target) => verify::handle(target, args).await,
        MigrateSubCommands::Issues(issues_cmd) => issues::handle(issues_cmd, args).await,
        MigrateSubCommands::Revert(revert_cmd) => revert::handle(revert_cmd, args).await,
        MigrateSubCommands::ResetState(reset_cmd) => reset_state::handle(reset_cmd, args).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// Every migration the catalog references is registered, and every registered migration
    /// is performed by a catalog step
    #[test]
    fn catalog_migrations_are_registered() {
        // check every step that names a migration
        for step in CATALOG.iter().flat_map(|entry| entry.steps.iter()) {
            if let Some(id) = step.thoradm_migration {
                assert!(
                    find(id).is_ok(),
                    "migration {id} of step {} is not registered",
                    step.id
                );
            }
        }
        // a registered migration no catalog step uses could never be run by an upgrade
        for migration in MIGRATIONS {
            assert!(
                step_for(migration.id).is_some(),
                "migration {} is not used by any catalog step",
                migration.id
            );
        }
    }

    /// A stub migration fails with the not implemented exit code and the manual procedure
    #[test]
    fn stubs_report_the_procedure() {
        // run the reindex migration
        let target = MigrationTarget {
            id: Some("elastic-keyword-mappings".to_owned()),
            positional_id: None,
        };
        let error = run_or_verify(&target, "run").expect_err("stub should fail");
        assert_eq!(error.exit_code(), 3);
        assert!(error.to_string().contains("search-streamer"));
        // unknown migrations are ordinary errors
        let unknown = MigrationTarget {
            id: None,
            positional_id: Some("nope".to_owned()),
        };
        assert_eq!(
            run_or_verify(&unknown, "verify")
                .expect_err("unknown")
                .exit_code(),
            1
        );
    }

    /// Steps note the migration they use and whether thoradm can run it yet
    #[test]
    fn migration_notes() {
        // the reindex step's migration is registered but not implemented
        let (_, reindex) = upgrades::step("elastic-reindex-keyword-mappings").expect("reindex");
        assert_eq!(
            migration_note(reindex),
            ", migration elastic-keyword-mappings (not implemented; manual procedure)"
        );
        // steps without a migration have no note
        let (_, finalize) = upgrades::step("finalize").expect("finalize");
        assert_eq!(migration_note(finalize), "");
        // a migration from a newer catalog is called out
        let newer = StepDef {
            thoradm_migration: Some("from-the-future"),
            ..*finalize
        };
        assert!(migration_note(&newer).contains("not registered"));
        // the migration is traced back to the step it performs
        assert_eq!(
            step_for("elastic-keyword-mappings").map(|step| step.id),
            Some(reindex.id)
        );
    }

    /// Planning accepts known revisions and rejects malformed, unknown, and backwards ones
    #[test]
    fn plan_validates_revisions() {
        // build a plan request
        let plan_between = |from: &str, to: Option<&str>| {
            plan(&PlanMigrations {
                from: from.to_owned(),
                to: to.map(str::to_owned),
            })
        };
        // known revisions plan, defaulting the target to the latest revision
        assert!(plan_between("2026-10-v01", None).is_ok());
        assert!(plan_between("2026-10-v02", Some("2026-10-v02")).is_ok());
        // malformed, unknown, and backwards revisions are errors
        assert!(plan_between("latest", None).is_err());
        assert!(plan_between("2026-10-v01", Some("2099-01-v01")).is_err());
        assert!(plan_between("2026-10-v02", Some("2026-10-v01")).is_err());
    }

    /// A plan from the first revision lists every later step in catalog order
    #[test]
    fn plan_lists_steps_in_order() {
        // plan from the pre-Helm baseline to the latest revision
        let lines = plan_lines(&PlanMigrations {
            from: "2026-10-v01".to_owned(),
            to: None,
        })
        .expect("plan");
        // the header names both revisions
        assert_eq!(
            lines[0],
            format!("Upgrading from 2026-10-v01 to {} runs:", upgrades::latest())
        );
        // every step of every later revision follows in catalog order
        let expected: Vec<&str> = CATALOG
            .iter()
            .filter(|entry| entry.id > "2026-10-v01")
            .flat_map(|entry| entry.steps.iter().map(|step| step.id))
            .collect();
        let found: Vec<&str> = lines[1..]
            .iter()
            .map(|line| line.split_whitespace().nth(1).expect("step id"))
            .collect();
        assert_eq!(found, expected);
        // the Helm revision's steps are among them in order
        assert_eq!(
            &found[..4],
            &[
                "scylla-role",
                "elastic-identity",
                "elastic-reindex-keyword-mappings",
                "finalize"
            ]
        );
        // each step line carries its kind and description
        assert!(lines[3].contains("[Data]"));
        assert!(lines[3].starts_with("  2026-10-v02 elastic-reindex-keyword-mappings"));
    }

    /// A plan between the same revision says there is nothing to do
    #[test]
    fn plan_between_same_revision_is_empty() {
        // plan from the latest revision to itself
        let latest = upgrades::latest().to_string();
        let lines = plan_lines(&PlanMigrations {
            from: latest.clone(),
            to: Some(latest.clone()),
        })
        .expect("plan");
        // only the nothing to do line is printed
        assert_eq!(
            lines,
            vec![format!("Nothing to do between {latest} and {latest}")]
        );
    }

    /// The catalog listing notes approvals, scaled down components, and migrations
    #[test]
    fn list_notes_approvals_and_migrations() {
        // build the listing
        let lines = list_lines();
        // every revision has a header line
        for entry in CATALOG {
            assert!(
                lines
                    .iter()
                    .any(|line| line.starts_with(&format!("{} (released with Thorium", entry.id))),
                "missing revision {}",
                entry.id
            );
        }
        // the data step needs an approval, scales down the search-streamer, and names its migration
        let reindex = lines
            .iter()
            .find(|line| line.starts_with("  - elastic-reindex-keyword-mappings "))
            .expect("reindex line");
        assert!(reindex.contains(
            "[Data, needs approval + backup, scales down search-streamer, migration \
             elastic-keyword-mappings (not implemented; manual procedure)]"
        ));
        // infra and config steps need no approval and carry no notes
        let scylla = lines
            .iter()
            .find(|line| line.starts_with("  - scylla-role "))
            .expect("scylla line");
        assert!(scylla.contains("[Infra]"));
        let finalize = lines
            .iter()
            .find(|line| line.starts_with("  - finalize "))
            .expect("finalize line");
        assert!(finalize.contains("[Config]"));
    }

    /// The migrate subcommands parse from the command line
    #[test]
    fn migrate_args_parse() {
        // plan takes a starting and optional target revision
        let args = crate::args::Args::try_parse_from([
            "thoradm",
            "migrate",
            "plan",
            "--from",
            "2026-10-v01",
        ])
        .expect("plan should parse");
        let crate::args::SubCommands::Migrate(MigrateSubCommands::Plan(plan)) = args.cmd else {
            panic!("expected migrate plan");
        };
        assert_eq!(plan.from, "2026-10-v01");
        assert_eq!(plan.to, None);
        // run takes the migration id
        let args = crate::args::Args::try_parse_from([
            "thoradm",
            "migrate",
            "run",
            "elastic-keyword-mappings",
        ])
        .expect("run should parse");
        assert!(matches!(
            args.cmd,
            crate::args::SubCommands::Migrate(MigrateSubCommands::Run(_))
        ));
    }

    /// Run and verify take the migration id as `--id` or positionally, but exactly one of them
    #[test]
    fn run_and_verify_take_the_id_either_way() {
        // parse a migrate command line
        let parse = |rest: &[&str]| {
            crate::args::Args::try_parse_from(["thoradm", "migrate"].iter().chain(rest))
        };
        // both forms name the same migration for run and for verify
        for form in [
            &["--id", "elastic-keyword-mappings"][..],
            &["elastic-keyword-mappings"][..],
        ] {
            let run = parse(&[&["run"], form].concat()).expect("run should parse");
            let crate::args::SubCommands::Migrate(MigrateSubCommands::Run(run)) = run.cmd else {
                panic!("expected migrate run");
            };
            assert_eq!(run.target.id(), "elastic-keyword-mappings");
            let verify = parse(&[&["verify"], form].concat()).expect("verify should parse");
            let crate::args::SubCommands::Migrate(MigrateSubCommands::Verify(verify)) = verify.cmd
            else {
                panic!("expected migrate verify");
            };
            assert_eq!(verify.id(), "elastic-keyword-mappings");
        }
        // run keeps its other flags alongside --id
        let run = parse(&["run", "--id", "a", "--objects", "x,y"]).expect("run should parse");
        let crate::args::SubCommands::Migrate(MigrateSubCommands::Run(run)) = run.cmd else {
            panic!("expected migrate run");
        };
        assert_eq!((run.target.id(), run.objects.len()), ("a", 2));
        // a missing id, or both forms at once, is a usage error
        for command in ["run", "verify"] {
            assert!(parse(&[command]).is_err());
            assert!(parse(&[command, "--id", "a", "b"]).is_err());
        }
    }
}
