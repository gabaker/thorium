//! `thorctl toolbox import` and `thorctl toolbox update`
//!
//! An import loads a toolbox (a legacy one is converted in memory first), reads what exists in the
//! groups it names, plans what to create, publish, and update, asks about anything that differs
//! when told to, and applies the plan in dependency order with a journal that a rollback can
//! replay. An update is the same import that creates nothing.

use std::io::IsTerminal;
use thorium::Error;

mod apply;
mod classify;
mod journal;
mod plan;
mod preflight;
mod prepare;
mod report;
mod resolve;
mod rollback;
mod terminal;
mod world;

#[cfg(test)]
mod tests;

use apply::Applier;
use journal::Journal;
use plan::{ImportPlan, Options, build_plan};
use report::{Action, ImportReport, ItemResult, Outcome};
use world::{ImportApi, World};

use crate::args::Args;
use crate::args::toolbox::{ImportOpts, ImportToolbox, UpdateToolbox};
use crate::handlers::progress::{Bar, BarKind};
use crate::handlers::toolbox::convert::{ConvertOptions, Input, convert_document, read_input};
use crate::handlers::toolbox::v2::load::{ConvertReport, SourceFormat, resolve_sources};
use crate::handlers::toolbox::v2::plan_types::{
    Conflict, ConflictPolicy, ItemState, Plan, Resolution,
};
use crate::handlers::toolbox::v2::types::ToolboxManifestV2;
use crate::handlers::toolbox::v2::validate::{Severity, validate_manifest};
use crate::utils::output::{self, OutputFormat};

/// What the engine is told to do, apart from the manifest
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct Settings {
    /// Whether items that don't exist are created (an update creates nothing)
    pub create: bool,
    /// Whether groups the manifest names that don't exist are created
    pub create_groups: bool,
    /// How an existing item that differs is decided when no one is asked
    pub policy: ConflictPolicy,
    /// Whether existing network policies are brought in line with the manifest
    pub update_network_policy: bool,
    /// Whether to plan only and write nothing
    pub dry_run: bool,
    /// Whether to roll back what was applied when the import stops early, without asking
    pub rollback_on_failure: bool,
    /// Whether a person can be asked questions
    pub can_prompt: bool,
    /// Whether to ask before writing
    pub confirm: bool,
    /// How many requests to run at once
    pub workers: usize,
}

/// Whoever decides the questions an import raises
pub trait Decider {
    /// Decide what to do with an existing item that differs from the toolbox
    ///
    /// # Arguments
    ///
    /// * `conflict` - How the item differs
    /// * `diff` - The difference, when there is one to show
    fn resolve(&mut self, conflict: &Conflict, diff: Option<&str>) -> Resolution;

    /// Decide whether to go ahead with writing a plan
    ///
    /// # Arguments
    ///
    /// * `plan` - The plan
    fn confirm(&mut self, plan: &Plan) -> bool;

    /// Decide whether to undo what was applied before the import stopped
    ///
    /// # Arguments
    ///
    /// * `applied` - What was applied, one line per change
    fn rollback(&mut self, applied: &[String]) -> bool;
}

/// Run an import against Thorium
///
/// Nothing is written until the plan is built, checked, and (when asked) confirmed.
///
/// # Arguments
///
/// * `api` - The API to read and write through
/// * `manifest` - The manifest to import, with its group override and selection applied
/// * `settings` - What the import is told to do
/// * `decider` - Whoever answers the questions the import raises
/// * `bar` - The progress bar
pub async fn execute<A: ImportApi, D: Decider>(
    api: &A,
    manifest: &ToolboxManifestV2,
    settings: &Settings,
    decider: &mut D,
    bar: &Bar,
) -> Result<ImportReport, Error> {
    // read what exists in the groups the manifest names
    bar.refresh("Reading what exists", BarKind::Timer);
    let world = World::load(api, &prepare::wanted(manifest), settings.workers).await?;
    // plan the import and check what would stop it
    let opts = Options {
        create: settings.create,
        create_groups: settings.create_groups,
        policy: settings.policy,
        update_network_policy: settings.update_network_policy,
    };
    let mut import_plan = build_plan(manifest, &world, &opts);
    let problems = preflight::check(manifest, &world, &opts, &import_plan);
    import_plan.errors.extend(problems);
    if !import_plan.errors.is_empty() {
        let mut report = ImportReport::new(import_plan.plan.clone(), Outcome::Refused);
        report.errors.clone_from(&import_plan.errors);
        return Ok(report);
    }
    // a dry run stops at the plan
    if settings.dry_run {
        return Ok(ImportReport::new(import_plan.plan.clone(), Outcome::DryRun));
    }
    // existing items that differ are decided by the person asked, or the import is refused
    if settings.policy == ConflictPolicy::Ask
        && let Some(report) = decide_conflicts(&mut import_plan, settings, decider)
    {
        return Ok(report);
    }
    // an import that changes nothing says so
    if !import_plan.has_changes() {
        let mut report = ImportReport::new(import_plan.plan.clone(), Outcome::NothingToDo);
        report.results = unchanged_results(&import_plan);
        return Ok(report);
    }
    // a person is asked before anything is written
    if settings.confirm && !decider.confirm(&import_plan.plan) {
        return Ok(ImportReport::new(
            import_plan.plan.clone(),
            Outcome::Cancelled,
        ));
    }
    // apply the plan
    let journal = Journal::default();
    let applier = Applier {
        api,
        manifest,
        world: &world,
        plan: &import_plan,
        opts: &opts,
        workers: settings.workers,
        bar,
        journal: &journal,
    };
    let output = applier.run().await;
    let mut report = ImportReport::new(import_plan.plan.clone(), Outcome::Applied);
    report.groups_created = output.groups_created;
    report.policies_created = output.policies_created;
    report.policies_updated = output.policies_updated;
    report.results = output.results;
    // a failure is kept or rolled back
    if let Some(failure) = output.failure {
        report.errors.push(failure);
        report.outcome = Outcome::Failed;
        let changes = journal.describe();
        let undo = settings.rollback_on_failure
            || (settings.can_prompt && !changes.is_empty() && decider.rollback(&changes));
        if undo {
            bar.refresh("Rolling back", BarKind::Timer);
            let outcome = rollback::rollback(api, journal.take()).await;
            report.rolled_back = outcome.undone;
            report.rollback_errors = outcome.failed;
            report.outcome = Outcome::RolledBack;
        }
    }
    Ok(report)
}

/// Report every item of a plan that needs no write as unchanged, skipped, or not imported
///
/// # Arguments
///
/// * `plan` - The plan
fn unchanged_results(plan: &ImportPlan) -> Vec<ItemResult> {
    // each item that needs no write has a passive result
    plan.plan
        .items
        .iter()
        .filter_map(|item| {
            let action = match item.state {
                ItemState::Unchanged => Action::Unchanged,
                ItemState::Skipped => Action::Skipped,
                ItemState::NotImported => Action::NotImported,
                ItemState::Create | ItemState::Update => return None,
            };
            Some(ItemResult {
                item: item.item.clone(),
                group: item.group.clone(),
                action,
                detail: item
                    .conflict
                    .as_ref()
                    .map_or_else(String::new, |conflict| conflict.detail.clone()),
            })
        })
        .collect()
}

/// Decide the existing items that differ, by asking, or refuse the import when no one can be asked
///
/// Returns the report that ends the import when it can't go on.
///
/// # Arguments
///
/// * `plan` - The plan whose conflicts are decided
/// * `settings` - What the import is told to do
/// * `decider` - Whoever answers
fn decide_conflicts<D: Decider>(
    plan: &mut ImportPlan,
    settings: &Settings,
    decider: &mut D,
) -> Option<ImportReport> {
    // nothing to decide
    if plan.plan.conflicts().is_empty() {
        return None;
    }
    // without a person the import can't choose, so it lists what differs and stops
    if !settings.can_prompt {
        let mut report = ImportReport::new(plan.plan.clone(), Outcome::Refused);
        report.errors.push(format!(
            "{} existing item(s) differ from the toolbox; use --overwrite to apply the \
             differences or --skip-conflicts to leave them: {}",
            plan.plan.conflicts().len(),
            plan.plan
                .conflicts()
                .iter()
                .map(|conflict| format!("{} ({})", conflict.item, conflict.detail))
                .collect::<Vec<String>>()
                .join("; ")
        ));
        return Some(report);
    }
    // ask about each item that differs
    let cancelled = ImportReport::new(plan.plan.clone(), Outcome::Cancelled);
    for item in &mut plan.plan.items {
        let Some(conflict) = item.conflict.clone() else {
            continue;
        };
        if item.state != ItemState::Update {
            continue;
        }
        match decider.resolve(&conflict, item.diff.as_deref()) {
            Resolution::Apply => {}
            Resolution::Skip => item.state = ItemState::Skipped,
            Resolution::Quit => return Some(cancelled),
        }
    }
    None
}

/// The settings a command line gives the engine
///
/// # Arguments
///
/// * `opts` - The import settings from the command line
/// * `create` - Whether the command creates what is missing
/// * `create_groups` - Whether the command creates missing groups
/// * `workers` - The global worker count
fn settings_of(opts: &ImportOpts, create: bool, create_groups: bool, workers: usize) -> Settings {
    // a dialog needs both a keyboard and a screen (prompts are drawn on stderr)
    let terminal = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    let policy = if opts.overwrite {
        ConflictPolicy::Overwrite
    } else if opts.skip_conflicts {
        ConflictPolicy::SkipConflicts
    } else {
        ConflictPolicy::Ask
    };
    Settings {
        create,
        // the override's target group is created when it's missing
        create_groups: create && (create_groups || opts.group_override.is_some()),
        policy,
        update_network_policy: opts.update_network_policy,
        dry_run: opts.dry_run,
        rollback_on_failure: opts.rollback_on_failure,
        // the flags that settle conflicts are the non-interactive paths
        can_prompt: terminal && policy == ConflictPolicy::Ask,
        confirm: terminal && !opts.skip_confirm && !opts.dry_run,
        workers,
    }
}

/// A toolbox ready to import
struct Loaded {
    /// The manifest, converted, with its group override and selection applied
    manifest: ToolboxManifestV2,
    /// Warnings worth showing before the plan
    warnings: Vec<String>,
}

/// Load, convert, check, and shape a toolbox for import
///
/// # Arguments
///
/// * `opts` - The import settings from the command line
/// * `bar` - The progress bar
async fn load(opts: &ImportOpts, bar: &Bar) -> Result<Loaded, Error> {
    // read the manifest and convert it, whichever format it is in
    let Input { bytes, file } = read_input(&opts.manifest, bar).await?;
    let base_dir = file
        .as_deref()
        .and_then(|file| file.parent())
        .map(std::path::Path::to_path_buf);
    let options = ConvertOptions {
        group_override: opts.group_override.clone(),
        follow_latest: opts.follow_latest,
        fill_image_from_tags: false,
        keep_incomplete_pipelines: false,
    };
    let converted = convert_document(&bytes, base_dir.clone(), &options, bar).await?;
    let mut manifest = converted.manifest;
    let summary = converted.summary;
    let mut warnings = Vec::new();
    // a legacy toolbox is reported the way `toolbox convert` reports it
    if summary.source != SourceFormat::V2 || summary.has_issues() {
        bar.suspend(|| {
            eprintln!(
                "{}",
                output::render(&summary, OutputFormat::Human, false)
                    .unwrap_or_default()
                    .trim_end()
            );
        });
    }
    // a collision can't be imported, and strict refuses any conversion issue
    if !summary.collisions.is_empty() {
        return Err(Error::new(format!(
            "The toolbox defines {} more than once; fix the toolbox or pass --group-override",
            summary.collisions.join(", ")
        )));
    }
    if opts.strict && summary.has_issues() {
        return Err(Error::new(
            "--strict: converting the toolbox raised issues (listed above); nothing was changed"
                .to_owned(),
        ));
    }
    // read the specs and configs the manifest points at
    let sources = resolve_sources(&mut manifest, base_dir.as_deref()).await;
    // the group override applies to every kind of item
    if let Some(group) = &opts.group_override {
        prepare::override_group(&mut manifest, group);
    }
    // images of a bundled toolbox are pointed at the registry they were pushed to
    let prefix = opts
        .image_path_prefix
        .as_deref()
        .filter(|prefix| !prefix.is_empty())
        .or_else(|| {
            manifest
                .image_path_prefix
                .as_deref()
                .filter(|prefix| !prefix.is_empty())
        })
        .map(str::to_owned);
    if manifest.bundled_images {
        if let Some(prefix) = prefix {
            let count = prepare::rewrite_images(&mut manifest, &prefix);
            warnings.push(format!(
                "pointed {count} container image(s) at {prefix}; importing does not push container \
                 images, so push them there first"
            ));
        }
    } else if opts.image_path_prefix.is_some() {
        warnings.push(
            "--image-path-prefix has no effect: the toolbox does not bundle container images"
                .to_owned(),
        );
    }
    // refuse a toolbox with errors, and with warnings under --strict
    let issues = validate_manifest(&manifest, &ConvertReport::default(), &sources);
    let errors: Vec<String> = issues
        .iter()
        .filter(|issue| issue.severity == Severity::Error)
        .map(|issue| format!("[{}] {}: {}", issue.code, issue.item, issue.message))
        .collect();
    if !errors.is_empty() {
        return Err(Error::new(format!(
            "The toolbox has {} error(s); nothing was changed:\n  {}",
            errors.len(),
            errors.join("\n  ")
        )));
    }
    for issue in &issues {
        // a name outside the toolbox may exist in Thorium, which the plan checks
        if issue.severity == Severity::Warning && !issue.code.starts_with("unresolved_ref") {
            let line = format!("[{}] {}: {}", issue.code, issue.item, issue.message);
            if opts.strict {
                return Err(Error::new(format!(
                    "--strict: the toolbox has a warning; nothing was changed:\n  {line}"
                )));
            }
            warnings.push(line);
        }
    }
    // cut the toolbox down to what was asked for
    prepare::select(&mut manifest, opts).map_err(Error::new)?;
    Ok(Loaded { manifest, warnings })
}

/// Run an import or an update from the command line
///
/// Returns whether it succeeded, so the caller can exit non-zero after the report is printed.
///
/// # Arguments
///
/// * `args` - The top level arguments
/// * `opts` - The import settings from the command line
/// * `create` - Whether the command creates what is missing
/// * `create_groups` - Whether the command creates missing groups
async fn run(
    args: &Args,
    opts: &ImportOpts,
    create: bool,
    create_groups: bool,
) -> Result<bool, Error> {
    let name = if create {
        "toolbox import"
    } else {
        "toolbox update"
    };
    let bar = Bar::new_or_quiet(name, "Reading manifest...", BarKind::UnboundIO, args.quiet);
    // load the toolbox, then run
    let loaded = load(opts, &bar).await?;
    for warning in &loaded.warnings {
        bar.warning(warning);
    }
    // connect only once there is something to import
    bar.refresh("Connecting", BarKind::Timer);
    let (_conf, thorium) = super::connect(args).await?;
    let settings = settings_of(opts, create, create_groups, args.workers);
    let format = opts.format.format.unwrap_or(OutputFormat::Human);
    let mut decider = terminal::TerminalDecider::new(&bar);
    let report = execute(&thorium, &loaded.manifest, &settings, &mut decider, &bar).await?;
    bar.finish_and_clear();
    // print the plan of a dry run or a refusal, and the result of anything else
    match report.outcome {
        Outcome::DryRun | Outcome::Refused => {
            let document = plan::PlanDocument {
                plan: report.plan.clone(),
                errors: report.errors.clone(),
            };
            output::print(&document, format)?;
            // a refusal is an error, so scripts that read stderr see why
            if report.outcome == Outcome::Refused {
                for error in &report.errors {
                    eprintln!("error: {error}");
                }
            }
        }
        _ => output::print(&report, format)?,
    }
    Ok(report.success())
}

/// Handle `thorctl toolbox import`
///
/// Returns whether the import succeeded.
///
/// # Arguments
///
/// * `args` - The top level arguments
/// * `cmd` - The import command
pub async fn import(args: &Args, cmd: &ImportToolbox) -> Result<bool, Error> {
    // an import creates what is missing, and groups when told to
    run(args, &cmd.opts, true, cmd.create_groups).await
}

/// Handle `thorctl toolbox update`
///
/// Returns whether the update succeeded.
///
/// # Arguments
///
/// * `args` - The top level arguments
/// * `cmd` - The update command
pub async fn update(args: &Args, cmd: &UpdateToolbox) -> Result<bool, Error> {
    // an update creates nothing
    run(args, &cmd.opts, false, false).await
}
