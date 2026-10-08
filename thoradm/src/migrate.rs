//! Lists, plans, and runs the data migrations behind Thorium's upgrade revisions
//!
//! The revisions and their steps come from the catalog shared with the operator
//! (`thorium::models::upgrades`). Steps that change stored data name a thoradm migration,
//! which the operator will eventually run in a pod it spawns. No migration is implemented
//! yet: each one is registered here and reports the manual procedure instead.

use thorium::models::upgrades::{self, CATALOG, RevisionId, StepDef, UpgradeState};

use crate::Error;
use crate::args::{MigrateSubCommands, MigrationTarget, PlanMigrations};

/// The backend a data migration changes
#[derive(Debug, Clone, Copy)]
enum Backend {
    /// Elasticsearch
    Elastic,
}

/// A data migration thoradm knows about
#[derive(Debug, Clone, Copy)]
struct MigrationDef {
    /// The id of this migration, referenced by catalog steps
    id: &'static str,
    /// The backend this migration changes
    backend: Backend,
    /// Whether thoradm can run this migration yet
    implemented: bool,
}

/// Every data migration thoradm knows about
const MIGRATIONS: &[MigrationDef] = &[MigrationDef {
    id: "elastic-keyword-mappings",
    backend: Backend::Elastic,
    implemented: false,
}];

/// Find a registered migration by its id
///
/// # Arguments
///
/// * `id` - The id of the migration
fn find(id: &str) -> Result<&'static MigrationDef, Error> {
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
    raw.parse()
        .map_err(|error: upgrades::UpgradeError| Error::new(error.to_string()))
}

/// Describe the migration a step uses, if any
///
/// # Arguments
///
/// * `step` - The step to describe
fn migration_note(step: &StepDef) -> String {
    match step.thoradm_migration {
        Some(id) => match MIGRATIONS.iter().find(|migration| migration.id == id) {
            Some(migration) if migration.implemented => format!(", migration {id}"),
            Some(_) => format!(", migration {id} (not implemented; manual procedure)"),
            None => format!(", migration {id} (not registered in this thoradm)"),
        },
        None => String::new(),
    }
}

/// Print every revision in the catalog and its steps
fn list() {
    // print each revision with its steps, oldest first
    for entry in CATALOG {
        // print the revision
        println!(
            "{} (released with Thorium {}): {}",
            entry.id, entry.released_with, entry.description
        );
        // print each of its steps
        for step in entry.steps {
            // note steps that need an approval with a backup
            let approval = if step.kind.needs_approval() {
                ", needs approval + backup"
            } else {
                ""
            };
            // note the components a step scales down
            let quiesce = if step.quiesce.is_empty() {
                String::new()
            } else {
                let names = step
                    .quiesce
                    .iter()
                    .map(|component| component.deployment_name())
                    .collect::<Vec<_>>();
                format!(", scales down {}", names.join(", "))
            };
            // print the step with its notes
            println!(
                "  - {} [{:?}{approval}{quiesce}{}]: {}",
                step.id,
                step.kind,
                migration_note(step),
                step.description
            );
        }
    }
}

/// Print the steps that bring a cluster from one revision to another
///
/// # Arguments
///
/// * `cmd` - The revisions to plan between
fn plan(cmd: &PlanMigrations) -> Result<(), Error> {
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
        println!("Nothing to do between {from} and {to}");
        return Ok(());
    }
    // print each planned step
    println!("Upgrading from {from} to {to} runs:");
    for planned in &steps {
        println!(
            "  {} {} [{:?}]: {}",
            planned.revision, planned.step, planned.kind, planned.description
        );
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
fn run_or_verify(cmd: &MigrationTarget, action: &str) -> Result<(), Error> {
    // find the migration, every one of which is still a stub
    let migration = find(&cmd.id)?;
    Err(not_implemented(migration, action))
}

/// Handle a migrate subcommand
///
/// # Arguments
///
/// * `cmd` - The migrate subcommand to handle
pub fn handle(cmd: &MigrateSubCommands) -> Result<(), Error> {
    match cmd {
        MigrateSubCommands::List => {
            list();
            Ok(())
        }
        MigrateSubCommands::Plan(plan_cmd) => plan(plan_cmd),
        MigrateSubCommands::Run(target) => run_or_verify(target, "run"),
        MigrateSubCommands::Verify(target) => run_or_verify(target, "verify"),
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
            id: "elastic-keyword-mappings".to_owned(),
        };
        let error = run_or_verify(&target, "run").expect_err("stub should fail");
        assert_eq!(error.exit_code(), 3);
        assert!(error.to_string().contains("search-streamer"));
        // unknown migrations are ordinary errors
        let unknown = MigrationTarget {
            id: "nope".to_owned(),
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
}
