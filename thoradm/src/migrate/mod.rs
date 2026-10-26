//! Lists, plans, and runs the data migrations behind Thorium's upgrade revisions
//!
//! The revisions and their steps come from the catalog shared with the operator
//! (`thorium::models::upgrades`). Steps that change stored data name a thoradm migration,
//! which the operator runs in a Job it creates and an admin can run by hand with
//! `thoradm migrate run`. Each migration is registered in [`registry`] with whether thoradm
//! implements it; one it doesn't implement reports its manual procedure instead of running.

use thorium::models::upgrades::{self, Catalog, CatalogEntry, PlannedStep, RevisionId, StepDef};

use crate::Error;
use crate::args::{Args, MigrateSubCommands, MigrationTarget, PlanMigrations};
use crate::outcome::RunOutcome;

mod check;
pub mod context;
mod drain;
mod elastic_rebuild;
#[cfg(feature = "test-support")]
pub mod fixture;
mod images_to_tools;
mod issues;
mod loss;
pub mod pipelines_to_tool_stages;
mod provenance_to_legacy_tools;
mod record;
pub mod registry;
mod report;
mod reset_state;
mod revert;
mod run;
mod verify;

use registry::{MIGRATIONS, MigrationDef, Support};

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

/// Get the catalog thoradm plans, checks, and reverts over: the one this build ships
///
/// A build with the `test-support` feature plans over the fixture catalog when the environment
/// variable [`fixture::CATALOG_ENV`] is set, so tests can run steps the shipped catalog doesn't
/// have.
fn catalog() -> Catalog {
    // the fixture catalog stands in for the shipped one when a test asks for it
    #[cfg(feature = "test-support")]
    if std::env::var_os(fixture::CATALOG_ENV).is_some() {
        return fixture::CATALOG;
    }
    Catalog::PRODUCTION
}

/// Find the catalog step a migration performs
///
/// # Arguments
///
/// * `id` - The id of the migration
fn step_for(id: &str) -> Option<&'static StepDef> {
    // search every revision's steps for the one naming this migration
    catalog()
        .edges
        .iter()
        .flat_map(|entry| entry.steps.iter())
        .find(|step| step.thoradm_migration == Some(id))
}

/// The revision whose edge holds the step a migration performs
///
/// # Arguments
///
/// * `id` - The id of the migration
fn revision_of(id: &str) -> Option<String> {
    // search every revision's steps for the one naming this migration
    catalog()
        .edges
        .iter()
        .find(|entry| {
            entry
                .steps
                .iter()
                .any(|step| step.thoradm_migration == Some(id))
        })
        .map(|entry| entry.id.to_string())
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
            Some(migration) if migration.support == Support::Yes => format!(", migration {id}"),
            Some(migration) if migration.support == Support::CheckOnly => {
                format!(", migration {id} (check only; manual procedure)")
            }
            Some(_) => format!(", migration {id} (not implemented; manual procedure)"),
            None => format!(", migration {id} (not registered in this thoradm)"),
        },
        None => String::new(),
    }
}

/// Describe where an edge starts and what kind of edge it is
///
/// A release edge with no reverse edge is one-way: deployments can't downgrade across it.
///
/// # Arguments
///
/// * `catalog` - The catalog holding the edge
/// * `entry` - The edge to describe
fn edge_notes(catalog: &Catalog, entry: &CatalogEntry) -> String {
    // name the revision the edge starts from, unless it is a baseline
    let mut notes = entry
        .from
        .map_or_else(String::new, |from| format!("; from {from}"));
    // label feature branch edges, edges back down, and sibling paths
    if entry.prerelease {
        notes.push_str("; pre-release");
    }
    if entry.is_downward() {
        notes.push_str("; revert");
    }
    if entry.sibling {
        notes.push_str("; sibling");
    }
    // label release edges no reverse edge undoes
    let release = entry.from.is_some() && !entry.prerelease && !entry.is_downward();
    if release && catalog.revert_edge(entry).is_none() {
        notes.push_str("; one-way");
    }
    notes
}

/// Build the lines describing every edge in a catalog and its steps
///
/// # Arguments
///
/// * `catalog` - The catalog to describe
fn list_lines(catalog: &Catalog) -> Vec<String> {
    // collect the lines for each edge, in catalog order
    let mut lines = Vec::new();
    for entry in catalog.edges {
        // describe the revision the edge reaches and the kind of edge it is
        lines.push(format!(
            "{} (released with Thorium {}{}): {}",
            entry.id,
            entry.released_with,
            edge_notes(catalog, entry),
            entry.description
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
    // describe every time the minimum supported revision was raised, oldest first
    lines.extend(catalog.floors.iter().map(|floor| {
        format!(
            "Minimum supported revision {} since Thorium {}: older deployments install Thorium {} \
             first",
            floor.minimum, floor.since_version, floor.intermediate
        )
    }));
    lines
}

/// Print every revision in the catalog and its steps
fn list() {
    // print each line of the catalog listing
    for line in list_lines(&Catalog::PRODUCTION) {
        println!("{line}");
    }
}

/// Describe the notes of a planned step: its kind, and whether it is a pre-release or revert step
///
/// # Arguments
///
/// * `planned` - The planned step to describe
fn planned_notes(planned: &PlannedStep) -> String {
    // start with the step's kind and label feature branch and revert steps
    let mut notes = format!("{:?}", planned.kind);
    if planned.prerelease {
        notes.push_str(", pre-release");
    }
    if planned.reverse {
        notes.push_str(", revert");
    }
    notes
}

/// Build the lines describing the steps that bring a cluster from one revision to another
///
/// # Arguments
///
/// * `catalog` - The catalog to plan over
/// * `cmd` - The revisions to plan between
fn plan_lines(catalog: &Catalog, cmd: &PlanMigrations) -> Result<Vec<String>, Error> {
    // parse both revisions, defaulting the target to the latest one
    let from = revision(&cmd.from)?;
    let to = match &cmd.to {
        Some(raw) => revision(raw)?,
        None => catalog.latest(),
    };
    // plan from the state of a deployment that reached the starting revision
    let state = catalog.reached(&from, env!("CARGO_PKG_VERSION"));
    let steps = catalog
        .plan(&state, &to)
        .map_err(|error| Error::new(error.to_string()))?;
    // there may be nothing between the two revisions
    if steps.is_empty() {
        return Ok(vec![format!("Nothing to do between {from} and {to}")]);
    }
    // a plan with a step leading down is a downgrade
    let moving = if steps.iter().any(|planned| planned.reverse) {
        "Downgrading"
    } else {
        "Upgrading"
    };
    let mut lines = vec![format!("{moving} from {from} to {to} runs:")];
    for planned in &steps {
        lines.push(format!(
            "  {} {} [{}]: {}",
            planned.revision,
            planned.step,
            planned_notes(planned),
            planned.description
        ));
        // a lossy reverse names the fields it loses and what is lost
        if let Some(loss) = &planned.loss {
            let fields = loss
                .fields
                .iter()
                .map(|field| field.field.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            lines.push(format!("    loses {fields}: {}", loss.description));
        }
    }
    Ok(lines)
}

/// Print the steps that bring a cluster from one revision to another
///
/// # Arguments
///
/// * `cmd` - The revisions to plan between
fn plan(cmd: &PlanMigrations) -> Result<(), Error> {
    // build the plan, then print each of its lines
    for line in plan_lines(&Catalog::PRODUCTION, cmd)? {
        println!("{line}");
    }
    Ok(())
}

/// Report that thoradm can't perform a migration along with how to perform it by hand
///
/// # Arguments
///
/// * `migration` - The migration that isn't implemented
/// * `action` - What was asked of it ("run", "verify", or "check")
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

/// Refuse to run, verify, or check a data migration thoradm can't perform
///
/// The run, verify, and check handlers call this for a migration whose registry entry doesn't
/// support the action; an unknown migration is an ordinary error.
///
/// # Arguments
///
/// * `cmd` - The migration asked for
/// * `action` - What was asked of it ("run", "verify", or "check")
fn run_or_verify(cmd: &MigrationTarget, action: &str) -> Result<RunOutcome, Error> {
    // find the migration
    let migration = find(cmd.id())?;
    // report how to perform it by hand, since thoradm can't perform it
    Err(not_implemented(migration, action))
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
        MigrateSubCommands::Report(report_cmd) => report::handle(report_cmd, args).await,
        MigrateSubCommands::Revert(revert_cmd) => revert::handle(revert_cmd, args).await,
        MigrateSubCommands::ResetState(reset_cmd) => reset_state::handle(reset_cmd, args).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use thorium::models::upgrades::CATALOG;

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
        for migration in MIGRATIONS.iter().filter(|migration| {
            // a fixture or rebucket migration has no catalog step, and a check-only migration may be
            // registered ahead of the step that runs it
            !migration.id.starts_with("fixture-")
                && !migration.id.starts_with("rebucket-")
                && migration.support != Support::CheckOnly
        }) {
            assert!(
                step_for(migration.id).is_some(),
                "migration {} is not used by any catalog step",
                migration.id
            );
        }
    }

    /// Every registered migration names the runner its passes go through, and only the migrations
    /// still performed by hand are marked unimplemented, so none can bypass the runner
    #[test]
    fn every_registered_migration_runs_through_a_runner() {
        // the migrations thoradm can't run yet
        let by_hand: [&str; 0] = [];
        for migration in MIGRATIONS {
            // each has a pass to record under and a chain to resume by
            assert!(
                !migration.pass.is_empty(),
                "{} has no pass key",
                migration.id
            );
            assert!(!migration.chain.is_empty(), "{} has no chain", migration.id);
            // a migration performed by hand is unsupported; every other one is run, or only checked
            // when it has no catalog run step to belong to
            assert_eq!(
                migration.support == Support::No,
                by_hand.contains(&migration.id),
                "{}",
                migration.id
            );
            // a check-only migration has no catalog step to run it, except the drain, whose check
            // is its whole work and whose step only waits for it
            if migration.support == Support::CheckOnly {
                assert_eq!(
                    step_for(migration.id).is_some(),
                    migration.id == "drain-reactions",
                    "{}",
                    migration.id
                );
            }
        }
    }

    /// The image conversion is registered as runnable, runs in its own pass, and covers the images
    /// and the entities its tools are written to
    #[test]
    fn the_image_conversion_is_registered_and_runnable() {
        // look it up as a run would
        let migration = find("images-to-tools").expect("the migration is registered");
        assert_eq!(migration.support, Support::Yes);
        assert_eq!(migration.pass, "images-to-tools");
        assert_eq!(migration.families, ["images", "entities"]);
    }

    /// The pipeline conversion is registered as runnable, runs in the `pipelines` pass, and covers
    /// the pipelines family
    #[test]
    fn the_pipeline_conversion_is_registered() {
        // look it up as a run would
        let migration = find("pipelines-to-tool-stages").expect("the migration is registered");
        assert_eq!(migration.support, Support::Yes);
        assert_eq!(migration.pass, "pipelines");
        assert_eq!(migration.families, ["pipelines"]);
    }

    /// The provenance conversion is registered as runnable and covers the entities its rows are in
    #[test]
    fn the_provenance_conversion_is_registered() {
        // look it up as a run would
        let migration = find("provenance-to-legacy-tools").expect("the migration is registered");
        assert_eq!(migration.support, Support::Yes);
        assert_eq!(migration.families, ["entities"]);
    }

    /// The drain is registered as a check that waits, and its step has no pause list, no
    /// approval work, and a procedure that names the command that clears reactions
    #[test]
    fn the_drain_is_a_check_with_a_waiting_step() {
        // the registration is a check only
        let migration = find("drain-reactions").expect("the migration is registered");
        assert_eq!(migration.support, Support::CheckOnly);
        // its step pauses nothing, reads nothing after another step, and says how to clear reactions
        let step = step_for("drain-reactions").expect("a catalog step");
        assert!(step.pause.is_empty() && step.reads_after.is_empty());
        assert!(
            step.manual_procedure
                .is_some_and(|text| text.contains("thorctl reactions delete"))
        );
    }

    /// A migration thoradm can't perform fails with the not implemented exit code and the manual
    /// procedure
    #[test]
    fn stubs_report_the_procedure() {
        // run a migration that can only be checked
        let target = MigrationTarget {
            id: Some("fixture-checkonly".to_owned()),
            positional_id: None,
        };
        let error = run_or_verify(&target, "run").expect_err("stub should fail");
        assert_eq!(error.exit_code(), 3);
        assert!(error.to_string().contains("No manual procedure"));
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
        // the reindex step's migration is the Elastic rebuild, which thoradm runs
        let (_, reindex) = upgrades::step("elastic-reindex-keyword-mappings").expect("reindex");
        assert_eq!(migration_note(reindex), ", migration elastic-rebuild");
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
            step_for("elastic-rebuild").map(|step| step.id),
            Some(reindex.id)
        );
    }

    /// A step whose migration can only be checked says so, and one whose run works says nothing
    #[cfg(feature = "test-support")]
    #[test]
    fn check_only_migrations_are_noted() {
        // borrow a step and point it at the fixture migrations
        let (_, finalize) = upgrades::step("finalize").expect("finalize");
        let note = |id| {
            migration_note(&StepDef {
                thoradm_migration: Some(id),
                ..*finalize
            })
        };
        assert_eq!(
            note("fixture-checkonly"),
            ", migration fixture-checkonly (check only; manual procedure)"
        );
        assert_eq!(note("fixture-theme"), ", migration fixture-theme");
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
        let lines = plan_lines(
            &Catalog::PRODUCTION,
            &PlanMigrations {
                from: "2026-10-v01".to_owned(),
                to: None,
            },
        )
        .expect("plan");
        // the header names both revisions
        assert_eq!(
            lines[0],
            format!("Upgrading from 2026-10-v01 to {} runs:", upgrades::latest())
        );
        // every step of every later forward edge follows in catalog order, the edges back aside,
        // except that the drain, which needs the API running, comes before the users step that
        // pauses the API
        let mut expected: Vec<&str> = CATALOG
            .iter()
            .filter(|entry| entry.id > "2026-10-v01" && !entry.is_downward())
            .flat_map(|entry| entry.steps.iter().map(|step| step.id))
            .collect();
        let drain = expected
            .iter()
            .position(|id| *id == "drain-reactions")
            .expect("the drain is planned");
        let users = expected
            .iter()
            .position(|id| *id == "users-settings-theme")
            .expect("the users step is planned");
        let drain = expected.remove(drain);
        expected.insert(users, drain);
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
        let lines = plan_lines(
            &Catalog::PRODUCTION,
            &PlanMigrations {
                from: latest.clone(),
                to: Some(latest.clone()),
            },
        )
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
        let lines = list_lines(&Catalog::PRODUCTION);
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
             elastic-rebuild]"
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

    /// A branch catalog with a pre-release, its revert edge, and a sibling, for the listing and
    /// plan tests
    const BRANCH: Catalog = Catalog {
        edges: &[
            CatalogEntry {
                from: None,
                id: "2026-10-v02",
                released_with: "0.0.0",
                description: "A release",
                schema: None,
                prerelease: false,
                sibling: false,
                steps: &[],
            },
            CatalogEntry {
                from: Some("2026-10-v02"),
                id: "2026-10-v02-pre.theme",
                released_with: "0.0.0",
                description: "A branch's change",
                schema: None,
                prerelease: true,
                sibling: false,
                steps: &[StepDef {
                    id: "theme",
                    description: "Add the theme",
                    ..StepDef::BASE
                }],
            },
            CatalogEntry {
                from: Some("2026-10-v02-pre.theme"),
                id: "2026-10-v02",
                released_with: "0.0.0",
                description: "Revert the branch's change",
                schema: None,
                prerelease: true,
                sibling: false,
                steps: &[StepDef {
                    id: "theme-revert",
                    description: "Remove the theme",
                    reverses: Some("theme"),
                    ..StepDef::BASE
                }],
            },
            CatalogEntry {
                from: Some("2026-10-v02"),
                id: "2026-10-v02-pre.theme-b",
                released_with: "0.0.0",
                description: "A second path",
                schema: None,
                prerelease: true,
                sibling: true,
                steps: &[],
            },
        ],
        floors: &[],
    };

    /// The listing labels pre-release, revert, and sibling edges with where they start
    #[test]
    fn list_labels_prerelease_edges() {
        // build the listing of the branch catalog
        let lines = list_lines(&BRANCH);
        // a baseline names no start, and a forward pre-release edge names its start
        assert!(lines.contains(&"2026-10-v02 (released with Thorium 0.0.0): A release".to_owned()));
        assert!(
            lines.contains(
                &"2026-10-v02-pre.theme (released with Thorium 0.0.0; from 2026-10-v02; \
              pre-release): A branch's change"
                    .to_owned()
            )
        );
        // the revert edge and the sibling are labelled as such
        assert!(
            lines.contains(
                &"2026-10-v02 (released with Thorium 0.0.0; from 2026-10-v02-pre.theme; \
              pre-release; revert): Revert the branch's change"
                    .to_owned()
            )
        );
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("2026-10-v02-pre.theme-b ")
                    && line.contains("; pre-release; sibling)"))
        );
    }

    /// The listing ends with each floor and the release to install below it
    #[test]
    fn list_names_the_floors() {
        // a catalog whose floor was raised once
        let floored = Catalog {
            floors: &[thorium::models::upgrades::SupportFloor {
                minimum: "2026-10-v02",
                intermediate: "1.9.0",
                since_version: "2.0.0",
            }],
            ..BRANCH
        };
        // the floor is the listing's last line
        assert_eq!(
            list_lines(&floored).last().map(String::as_str),
            Some(
                "Minimum supported revision 2026-10-v02 since Thorium 2.0.0: older deployments \
                 install Thorium 1.9.0 first"
            )
        );
        // a catalog without floors lists none
        assert!(
            list_lines(&BRANCH)
                .iter()
                .all(|line| !line.starts_with("Minimum supported revision"))
        );
    }

    /// Plans take pre-release ids and label pre-release and revert steps
    #[test]
    fn plan_labels_prerelease_steps() {
        // plan up to the pre-release and back down to its base
        let plan_between = |from: &str, to: &str| {
            plan_lines(
                &BRANCH,
                &PlanMigrations {
                    from: from.to_owned(),
                    to: Some(to.to_owned()),
                },
            )
            .expect("plan")
        };
        assert_eq!(
            plan_between("2026-10-v02", "2026-10-v02-pre.theme")[1],
            "  2026-10-v02-pre.theme theme [Config, pre-release]: Add the theme"
        );
        assert_eq!(
            plan_between("2026-10-v02-pre.theme", "2026-10-v02")[1],
            "  2026-10-v02 theme-revert [Config, pre-release, revert]: Remove the theme"
        );
        // the production catalog refuses a pre-release it doesn't know, naming the revert
        let error = plan_lines(
            &Catalog::PRODUCTION,
            &PlanMigrations {
                from: "2026-10-v02-pre.theme".to_owned(),
                to: None,
            },
        )
        .expect_err("unknown pre-release");
        assert!(
            error
                .to_string()
                .contains("thoradm migrate revert --to 2026-10-v02")
        );
    }

    /// The listing marks release edges without a reverse edge as one-way
    #[test]
    fn list_marks_one_way_edges() {
        use thorium::models::upgrades::downgrade_fixtures::ONE_WAY;
        // the release edge with a reverse edge isn't one-way, and the one without is
        let lines = list_lines(&ONE_WAY);
        let header = |id: &str, from: &str| {
            lines
                .iter()
                .find(|line| {
                    line.starts_with(&format!("{id} (")) && line.contains(&format!("; from {from}"))
                })
                .cloned()
                .expect("edge line")
        };
        assert!(!header("2026-10-v02", "2026-10-v01").contains("one-way"));
        assert!(header("2026-10-v03", "2026-10-v02").contains("; one-way)"));
        // the reverse edge and the baseline aren't marked
        assert!(!header("2026-10-v01", "2026-10-v02").contains("one-way"));
        // the production catalog ships no reverse edge yet, so its release edge is one-way
        assert!(
            list_lines(&Catalog::PRODUCTION)
                .iter()
                .any(|line| line.starts_with("2026-10-v02 (") && line.contains("; one-way)"))
        );
    }

    /// A downgrade plan says it downgrades and names what each lossy reverse loses, and a
    /// downgrade across an irreversible step is refused
    #[test]
    fn plan_describes_downgrades_and_losses() {
        use thorium::models::upgrades::downgrade_fixtures::{IRREVERSIBLE, LOSSY};
        // the lossy reverse is a downgrade naming the lost field and the loss
        let lines = plan_lines(
            &LOSSY,
            &PlanMigrations {
                from: "2026-10-v02".to_owned(),
                to: Some("2026-10-v01".to_owned()),
            },
        )
        .expect("plan");
        assert_eq!(
            lines,
            [
                "Downgrading from 2026-10-v02 to 2026-10-v01 runs:",
                "  2026-10-v01 contacts-v2-reverse [Data, revert]: A fixture step",
                "    loses contact: Users keep only their first contact",
            ]
        );
        // a downgrade across a finished irreversible step is refused, naming the restore
        let error = plan_lines(
            &IRREVERSIBLE,
            &PlanMigrations {
                from: "2026-10-v03".to_owned(),
                to: Some("2026-10-v01".to_owned()),
            },
        )
        .expect_err("irreversible");
        assert!(error.to_string().contains("files-v2"));
        assert!(error.to_string().contains("restore from backup"));
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
        let args =
            crate::args::Args::try_parse_from(["thoradm", "migrate", "run", "elastic-rebuild"])
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
        for form in [&["--id", "elastic-rebuild"][..], &["elastic-rebuild"][..]] {
            let run = parse(&[&["run"], form].concat()).expect("run should parse");
            let crate::args::SubCommands::Migrate(MigrateSubCommands::Run(run)) = run.cmd else {
                panic!("expected migrate run");
            };
            assert_eq!(run.target.id(), "elastic-rebuild");
            let verify = parse(&[&["verify"], form].concat()).expect("verify should parse");
            let crate::args::SubCommands::Migrate(MigrateSubCommands::Verify(verify)) = verify.cmd
            else {
                panic!("expected migrate verify");
            };
            assert_eq!(verify.id(), "elastic-rebuild");
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
