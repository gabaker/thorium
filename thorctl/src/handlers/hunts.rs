//! Handle hunt related Thorctl commands

use chrono::SecondsFormat;
use owo_colors::OwoColorize;
use std::collections::HashSet;
use std::io::Write;
use std::path::PathBuf;
use tabled::settings::Style;
use tabled::{Table, Tabled};
use thorium::models::{
    Cursor, Entity, EntityKinds, EntityListOpts, EntityMetadata, Hunt, HuntEngine, HuntLane,
    HuntRuleLine, HuntRuleListOpts, HuntStatus,
};
use thorium::{Error, Thorium};
use tokio::time::{Duration, sleep};
use uuid::Uuid;

use crate::args::Args;
use crate::args::hunts::{
    CancelHunt, CreateHunt, DeleteHunts, HuntHits, HuntRules, Hunts, ListHunts, StatusHunt,
};
use crate::handlers::rules::engine_name;
use crate::handlers::update;
use crate::utils;

/// Resolve a hunt by its id or exact name and split out its id, name, and metadata
///
/// # Arguments
///
/// * `thorium` - A Thorium client
/// * `target` - The id or name of the hunt to get
async fn get_hunt(thorium: &Thorium, target: &str) -> Result<(Uuid, String, Hunt), Error> {
    // resolve this target to a hunt entity
    let entity =
        utils::entities::resolve_entity(thorium, target, &[EntityKinds::Hunt], "hunt").await?;
    // split this entity into its id, name, and hunt metadata
    split_hunt(entity)
}

/// Split a hunt entity into its id, name, and hunt metadata
///
/// # Arguments
///
/// * `entity` - The hunt entity to split
fn split_hunt(entity: Entity) -> Result<(Uuid, String, Hunt), Error> {
    // take the id and name before splitting the metadata out of this entity
    let id = entity.id;
    let name = entity.name;
    // make sure this entity actually carries hunt metadata
    match entity.metadata {
        EntityMetadata::Hunt(hunt) => Ok((id, name, hunt)),
        // callers already kind-checked so this only guards a malformed response
        _ => Err(Error::new(format!(
            "Entity {id} is a {} entity, not a Hunt",
            entity.kind
        ))),
    }
}

/// Launch a hunt in Thorium
///
/// # Arguments
///
/// * `thorium` - A Thorium client
/// * `cmd` - The create hunt command to execute
async fn create(thorium: &Thorium, cmd: &CreateHunt) -> Result<(), Error> {
    // rule targets must be rules this hunt's engine can scan with
    let rule_kind = HuntEngine::from(cmd.engine).rule_kind();
    // resolve every rule name in the selector to its entity id; ids pass
    // through without a fetch since the create route validates them anyway
    let mut rule_ids = Vec::with_capacity(cmd.selector.rules.len());
    for target in &cmd.selector.rules {
        // resolve this rule target against the engine's rule kind
        rule_ids
            .push(utils::entities::resolve_entity_id(thorium, target, &[rule_kind], "rule").await?);
    }
    // resolve the rule set target if one was set
    let ruleset_id = match &cmd.selector.ruleset {
        Some(target) => Some(
            utils::entities::resolve_entity_id(
                thorium,
                target,
                &[EntityKinds::RuleSet],
                "rule set",
            )
            .await?,
        ),
        None => None,
    };
    // build our hunt create request from the resolved selector targets
    let req = cmd.to_req(rule_ids, ruleset_id);
    // create this hunt in Thorium
    let entity = match thorium.entities.create_hunt(&req).await {
        Ok(entity) => entity,
        Err(error) => {
            // surface the server's 400 reason (bad selector, quotas, etc) verbatim
            if error.status().map(|status| status.as_u16()) == Some(400)
                && let Some(msg) = utils::api_error_text(&error)
            {
                return Err(Error::new(format!("Failed to create hunt: {msg}")));
            }
            return Err(error);
        }
    };
    // keep this hunt's id and kind for printing
    let id = entity.id;
    let kind = entity.kind;
    // split the hunt metadata out of the created entity
    let hunt = match entity.metadata {
        EntityMetadata::Hunt(hunt) => hunt,
        _ => {
            return Err(Error::new(format!(
                "Hunt create returned a {kind} entity for {id}"
            )));
        }
    };
    // print this hunt's id and starting status
    println!("{} hunt {id} ({})", "Created".bright_green(), hunt.status);
    // print the size of the frozen snapshot and its exclusion counts
    println!(
        "{} rules frozen ({} quarantined excluded, {} foreign excluded)",
        hunt.rules.len(),
        hunt.excluded_quarantined,
        hunt.excluded_foreign
    );
    // print how many lanes the corpus was split into
    println!("{} lanes", hunt.lanes.len());
    // print a hint for watching this hunt's progress
    println!("Watch progress with: thorctl hunts status {id} --watch");
    Ok(())
}

/// Build the colored display form of a hunt status
///
/// # Arguments
///
/// * `status` - The hunt status to color
fn color_status(status: HuntStatus) -> String {
    // color each status by how much attention it needs
    match status {
        HuntStatus::Created => format!("{}", "Created".bright_blue()),
        HuntStatus::Running => format!("{}", "Running".yellow()),
        HuntStatus::Completed => format!("{}", "Completed".bright_green()),
        HuntStatus::Cancelled => format!("{}", "Cancelled".bright_red()),
        HuntStatus::Failed => format!("{}", "Failed".bright_red()),
    }
}

/// How many consecutive failed polls a watch tolerates before giving up
const MAX_WATCH_FAILURES: u32 = 10;

/// Compute how far through its time range a lane has walked
///
/// # Arguments
///
/// * `lane` - The lane to compute a walked fraction for
fn lane_fraction(lane: &HuntLane) -> f64 {
    // an exhausted lane is done no matter where its checkpoint stopped (its
    // newest listed upload can be newer than lane.end), matching the UI's
    // laneFraction so both surfaces agree a finished lane sits at 100%
    if lane.exhausted {
        return 1.0;
    }
    // get this lane's total time span in milliseconds
    let total = (lane.start - lane.end).num_milliseconds();
    // a lane with no span to walk is already done
    if total <= 0 {
        return 1.0;
    }
    // get how much of that span this lane has walked
    let walked = (lane.start - lane.current).num_milliseconds();
    // clamp the walked fraction into 0..=1
    (walked as f64 / total as f64).clamp(0.0, 1.0)
}

/// Print one lane's progress line
///
/// # Arguments
///
/// * `index` - The index of this lane
/// * `lane` - The lane to print
fn print_lane(index: usize, lane: &HuntLane) {
    // compute this lane's walked fraction
    let fraction = lane_fraction(lane);
    // size the filled part of a 10 segment bar
    let filled = (fraction * 10.0).round() as usize;
    // build the bar from filled and empty segments
    let bar = format!("{}{}", "#".repeat(filled), "·".repeat(10 - filled));
    // compute a whole percentage for this lane
    let percent = (fraction * 100.0).round() as u32;
    // mark exhausted lanes with a check
    let check = if lane.exhausted { "  ✓" } else { "" };
    println!("Lane {index} [{bar}] {percent:>3}%{check}");
}

/// Render the full status block for a hunt
///
/// # Arguments
///
/// * `id` - The id of the hunt being rendered
/// * `name` - The name of the hunt being rendered
/// * `hunt` - The hunt metadata to render
fn render_status(id: Uuid, name: &str, hunt: &Hunt) {
    // print this hunt's name, id, and colored status
    println!("Hunt {name} ({id}): {}", color_status(hunt.status));
    // print the failure reason if the system stopped this hunt
    if let Some(failure) = &hunt.failure {
        println!("Failure: {}", failure.bright_red());
    }
    // print one progress line per lane
    for (index, lane) in hunt.lanes.iter().enumerate() {
        print_lane(index, lane);
    }
    // print the scan counters
    let counters = &hunt.counters;
    println!(
        "Counters: listed={} scanned={} matched={} written={} suppressed={} skipped={} missing={} errors={}",
        counters.listed,
        counters.scanned,
        counters.matched,
        counters.written,
        counters.suppressed,
        counters.skipped,
        counters.missing,
        counters.errors,
    );
    // remind the user that counters can drift under chunk replay
    println!("counters are telemetry — hits are the exact record");
}

/// Show the status of a hunt, optionally watching until it is terminal
///
/// # Arguments
///
/// * `thorium` - A Thorium client
/// * `cmd` - The status hunt command to execute
async fn status(thorium: &Thorium, cmd: &StatusHunt) -> Result<(), Error> {
    // resolve this hunt and render its status block once
    let (id, name, hunt) = get_hunt(thorium, &cmd.hunt).await?;
    render_status(id, &name, &hunt);
    // stop here unless we are watching this hunt
    if !cmd.watch {
        return Ok(());
    }
    // build the interval to wait between polls, never busy looping
    let refresh = Duration::from_secs(cmd.refresh.max(1));
    // track the last status we saw so we can act on the terminal one
    let mut last = hunt.status;
    // count consecutive failed polls so a persistent outage ends the watch
    let mut failed_polls: u32 = 0;
    // keep polling until this hunt reaches a terminal status
    while last.is_active() {
        // wait for the refresh interval before polling again
        sleep(refresh).await;
        // re-poll by the resolved id directly (not the resolver) so a rename
        // mid-watch cannot change targets and a 404 keeps its status instead
        // of being remapped to a friendly statusless error
        let (id, name, hunt) = match thorium.entities.get(id).await {
            Ok(polled) => split_hunt(polled)?,
            // a deleted hunt ends the watch with a clear terminal error
            Err(error) if error.status().map(|status| status.as_u16()) == Some(404) => {
                return Err(Error::new(format!("Hunt {id} was deleted mid-watch")));
            }
            // a long watch should survive transient network/server blips, but
            // only genuinely transient ones: statusless decode failures
            // (client/server schema skew) are permanent and would retry forever
            Err(error)
                if matches!(&error, Error::Reqwest(req)
                        if req.is_connect() || req.is_timeout() || req.is_request())
                    || error
                        .status()
                        .is_some_and(|status| status.is_server_error()) =>
            {
                // give up once an outage stops looking transient
                failed_polls += 1;
                if failed_polls >= MAX_WATCH_FAILURES {
                    return Err(Error::new(format!(
                        "Watch gave up after {MAX_WATCH_FAILURES} consecutive failed polls: {error}"
                    )));
                }
                // warn and keep watching instead of failing the whole watch
                eprintln!(
                    "{}: poll failed, retrying: {error}",
                    "Warning".bright_yellow()
                );
                continue;
            }
            // anything else (401, decode skew, ...) is a real error worth stopping for
            Err(error) => return Err(error),
        };
        // a successful poll resets the consecutive failure count
        failed_polls = 0;
        // separate this tick's block from the last one
        println!();
        // render this tick's status block
        render_status(id, &name, &hunt);
        // save this status for the terminal check
        last = hunt.status;
    }
    // fail the watch so scripts can see this hunt ended Failed
    if last == HuntStatus::Failed {
        return Err(Error::new(format!("Hunt {id} failed")));
    }
    Ok(())
}

/// Drain a cursor into a single list
///
/// # Arguments
///
/// * `cursor` - The cursor to drain
async fn drain<T>(mut cursor: Cursor<T>) -> Result<Vec<T>, Error>
where
    for<'de> T: serde::Deserialize<'de>,
    T: serde::Serialize,
{
    // collect every page of this cursor
    let mut items = Vec::default();
    loop {
        // take this page's items
        items.append(&mut cursor.data);
        // stop when the cursor is exhausted
        if cursor.exhausted() {
            break;
        }
        // get the next page of items
        cursor.refill().await?;
    }
    Ok(items)
}

/// Print listed lines as a table, JSON, or JSON written to a file
///
/// # Arguments
///
/// * `output` - The path to write JSON to if one was given
/// * `condensed` - Whether to print condensed JSON
/// * `json` - Whether to print pretty JSON to stdout
/// * `lines` - The lines to print
fn print_lines<R, T>(
    output: &Option<PathBuf>,
    condensed: bool,
    json: bool,
    lines: &[T],
) -> Result<(), Error>
where
    T: serde::Serialize,
    R: Tabled,
    for<'a> R: From<&'a T>,
{
    // print our lines in the requested format
    match (output, condensed, json) {
        (None, false, false) => {
            // print our lines as a content sized table
            println!(
                "{}",
                Table::new(lines.iter().map(R::from)).with(Style::psql())
            );
        }
        (None, true, _) => {
            serde_json::to_writer(std::io::stdout(), lines)?;
            println!();
        }
        (None, false, true) => {
            serde_json::to_writer_pretty(std::io::stdout(), lines)?;
            println!();
        }
        (Some(output_path), true, _) => {
            let mut output_file = std::fs::File::create(output_path)?;
            serde_json::to_writer(&mut output_file, lines)?;
            writeln!(&mut output_file)?;
        }
        (Some(output_path), false, _) => {
            let mut output_file = std::fs::File::create(output_path)?;
            serde_json::to_writer_pretty(&mut output_file, lines)?;
            writeln!(&mut output_file)?;
        }
    }
    Ok(())
}

/// A row in the hunt list table
#[derive(Tabled)]
struct HuntRow {
    /// The name of this hunt
    #[tabled(rename = "NAME")]
    name: String,
    /// The status of this hunt
    #[tabled(rename = "STATUS")]
    status: String,
    /// The scan engine this hunt drives
    #[tabled(rename = "ENGINE")]
    engine: String,
    /// This hunt's unique id
    #[tabled(rename = "ID")]
    id: String,
    /// When this hunt was created
    #[tabled(rename = "CREATED")]
    created: String,
}

impl From<&Entity> for HuntRow {
    /// Build a table row from a hunt entity
    ///
    /// # Arguments
    ///
    /// * `entity` - The hunt entity to build a table row from
    fn from(entity: &Entity) -> Self {
        // pull the status and engine out of this entity's hunt metadata
        let (status, engine) = match &entity.metadata {
            EntityMetadata::Hunt(hunt) => (hunt.status.to_string(), hunt.engine.to_string()),
            // fall back to placeholders if this entity is somehow not a hunt
            _ => ("-".to_owned(), "-".to_owned()),
        };
        HuntRow {
            name: entity.name.clone(),
            status,
            engine,
            id: entity.id.to_string(),
            // seconds precision keeps the timestamp column narrow
            created: entity.created.to_rfc3339_opts(SecondsFormat::Secs, true),
        }
    }
}

/// List hunts in Thorium
///
/// # Arguments
///
/// * `thorium` - A Thorium client
/// * `cmd` - The list hunts command to execute
async fn list(thorium: &Thorium, cmd: &ListHunts) -> Result<(), Error> {
    // build our list options restricted to hunt entities
    let mut opts = EntityListOpts::default()
        .kind(EntityKinds::Hunt)
        .groups(cmd.groups.clone())
        .page_size(cmd.page_size);
    // add a limit unless the user disabled it
    if !cmd.no_limit {
        opts = opts.limit(cmd.limit);
    }
    // resume from a cursor if one was given
    if let Some(cursor) = cmd.cursor {
        opts = opts.cursor(cursor);
    }
    // only list active hunts if requested
    if cmd.active {
        opts = opts.tag(Hunt::ACTIVE_TAG, "true");
    }
    // drain the cursor into a list of hunt entities
    let hunts = drain(thorium.entities.list_details(&opts).await?).await?;
    // print our hunts as JSON or a table
    print_lines::<HuntRow, _>(&cmd.output, cmd.condensed, cmd.json, &hunts)
}

/// A row in the hunt hit table
#[derive(Tabled)]
struct HitRow {
    /// The name of the rule that hit
    #[tabled(rename = "RULE")]
    rule: String,
    /// The sha256 of the sample this rule hit on
    #[tabled(rename = "SAMPLE")]
    sample: String,
    /// When this sample was scanned
    #[tabled(rename = "SCANNED AT")]
    scanned_at: String,
}

impl From<&Entity> for HitRow {
    /// Build a table row from a hit entity
    ///
    /// # Arguments
    ///
    /// * `entity` - The hit entity to build a table row from
    fn from(entity: &Entity) -> Self {
        // pull the rule, sample, and scan time out of this entity's hit metadata
        match &entity.metadata {
            EntityMetadata::YaraRuleHit(hit) => HitRow {
                rule: hit.rule_name.clone(),
                sample: hit.sha256.clone(),
                // seconds precision keeps the timestamp column narrow
                scanned_at: hit.scanned_at.to_rfc3339_opts(SecondsFormat::Secs, true),
            },
            // fall back to the entity name if this entity is somehow not a hit
            _ => HitRow {
                rule: entity.name.clone(),
                sample: "-".to_owned(),
                scanned_at: "-".to_owned(),
            },
        }
    }
}

/// List the hits a hunt has written
///
/// # Arguments
///
/// * `thorium` - A Thorium client
/// * `cmd` - The hunt hits command to execute
async fn hits(thorium: &Thorium, cmd: &HuntHits) -> Result<(), Error> {
    // uuid targets are used directly so a deleted hunt's hits stay listable by id
    let id = match utils::entities::parse_id_target(&cmd.hunt) {
        Some(id) => id,
        // name targets (including hash style 32-hex names) resolve to the
        // hunt carrying that name
        None => {
            utils::entities::resolve_entity(thorium, &cmd.hunt, &[EntityKinds::Hunt], "hunt")
                .await?
                .id
        }
    };
    // build our list options for this hunt's yara rule hits
    let mut opts = EntityListOpts::default()
        .kind(EntityKinds::YaraRuleHit)
        .tag(Hunt::ID_TAG, id.to_string())
        .groups(cmd.groups.clone())
        .page_size(cmd.page_size);
    // add a limit unless the user disabled it
    if !cmd.no_limit {
        opts = opts.limit(cmd.limit);
    }
    // resume from a cursor if one was given
    if let Some(cursor) = cmd.cursor {
        opts = opts.cursor(cursor);
    }
    // drain the cursor into a list of hit entities
    let hit_entities = drain(thorium.entities.list_details(&opts).await?).await?;
    // print our hits as JSON or a table
    print_lines::<HitRow, _>(&cmd.output, cmd.condensed, cmd.json, &hit_entities)
}

/// A row in the hunt frozen rule table
#[derive(Tabled)]
struct FrozenRuleRow {
    /// The name of this frozen rule
    #[tabled(rename = "NAME")]
    name: String,
    /// The engine this rule targets
    #[tabled(rename = "ENGINE")]
    engine: String,
    /// The rule's current quarantine status and exemption
    #[tabled(rename = "STATUS")]
    status: String,
    /// The frozen rule entity's id
    #[tabled(rename = "ID")]
    id: String,
    /// When this rule was created
    #[tabled(rename = "CREATED")]
    created: String,
}

impl From<&HuntRuleLine> for FrozenRuleRow {
    /// Build a table row from a hunt frozen rule line
    ///
    /// # Arguments
    ///
    /// * `line` - The frozen rule line to build a table row from
    fn from(line: &HuntRuleLine) -> Self {
        // id only lines are rules deleted after the freeze or hidden from us
        let name = line
            .name
            .clone()
            .unwrap_or_else(|| "(deleted or hidden)".to_owned());
        // map the rule's kind to an engine name when we have one
        let engine = line
            .kind
            .as_ref()
            .map_or_else(|| "-".to_owned(), |kind| engine_name(kind).to_owned());
        // build the status cell from the quarantine values and exemption
        let mut parts = line.status.clone();
        if line.quarantine_exempt {
            parts.push("Exempt".to_owned());
        }
        // untouched rules render a placeholder instead of an empty cell
        let status = if parts.is_empty() {
            "-".to_owned()
        } else {
            parts.join(", ")
        };
        FrozenRuleRow {
            name,
            engine,
            status,
            id: line.id.to_string(),
            // seconds precision keeps the timestamp column narrow
            created: line.created.map_or_else(
                || "-".to_owned(),
                |created| created.to_rfc3339_opts(SecondsFormat::Secs, true),
            ),
        }
    }
}

/// List the frozen rules of a hunt
///
/// # Arguments
///
/// * `thorium` - A Thorium client
/// * `cmd` - The hunt rules command to execute
async fn rules(thorium: &Thorium, cmd: &HuntRules) -> Result<(), Error> {
    // make sure this entity exists and is actually a hunt
    let (id, _, _) = get_hunt(thorium, &cmd.hunt).await?;
    // build our frozen rule list options
    let mut opts = HuntRuleListOpts::default().page_size(cmd.page_size);
    // add a limit unless the user disabled it
    if !cmd.no_limit {
        opts = opts.limit(cmd.limit);
    }
    // resume from a cursor if one was given
    if let Some(cursor) = cmd.cursor {
        opts = opts.cursor(cursor);
    }
    // drain the cursor into a list of frozen rule lines using the resolved id
    let lines = drain(thorium.entities.list_hunt_rules(id, &opts).await?).await?;
    // print our frozen rules as JSON or a table
    print_lines::<FrozenRuleRow, _>(&cmd.output, cmd.condensed, cmd.json, &lines)
}

/// Cancel a hunt in Thorium
///
/// # Arguments
///
/// * `thorium` - A Thorium client
/// * `cmd` - The cancel hunt command to execute
async fn cancel(thorium: &Thorium, cmd: &CancelHunt) -> Result<(), Error> {
    // uuid targets are cancelled directly so cancel by id never grows a GET round trip
    let (id, resolved_name) = match utils::entities::parse_id_target(&cmd.hunt) {
        Some(id) => (id, None),
        // name targets (including hash style 32-hex names) resolve to the
        // hunt carrying that name
        None => {
            // resolve this name and keep it for the confirmation text
            let entity =
                utils::entities::resolve_entity(thorium, &cmd.hunt, &[EntityKinds::Hunt], "hunt")
                    .await?;
            (entity.id, Some(entity.name))
        }
    };
    // confirm the cancel unless the user opted to skip the prompt
    if !cmd.skip_confirm {
        // make sure we can actually prompt the user
        utils::require_confirm_terminal("--skip-confirm (-y)")?;
        // warn that cancelling a hunt is permanent, naming the resolved hunt
        match &resolved_name {
            Some(name) => println!(
                "Cancelling hunt {name} ({}). Running chunks stop within one chunk and the hunt cannot be resumed.",
                id.bright_red()
            ),
            None => println!(
                "Cancelling hunt {}. Running chunks stop within one chunk and the hunt cannot be resumed.",
                id.bright_red()
            ),
        }
        // ask the user to confirm this destructive action
        let confirmed = dialoguer::Confirm::new()
            .with_prompt("Cancel this hunt?")
            .default(false)
            .interact()?;
        // abort if the user declined
        if !confirmed {
            println!("Aborted");
            return Ok(());
        }
    }
    // cancel this hunt by its resolved id
    match thorium.entities.cancel_hunt(id).await {
        Ok(_) => {
            println!("{} hunt {}", "Cancelled".bright_green(), id);
            Ok(())
        }
        Err(error) => match error.status().map(|status| status.as_u16()) {
            // surface the server's reason verbatim (already terminal, not a hunt, ...)
            Some(400) => Err(Error::new(format!(
                "Cannot cancel hunt {}: {}",
                id,
                utils::api_error_text(&error).unwrap_or_default()
            ))),
            // cancelling is limited to the submitter and admins, but an
            // expired or invalid token is also a 401 so name both causes
            Some(401) => Err(Error::new(format!(
                "Cannot cancel hunt {id}: only the submitter or an admin can cancel a hunt \
                (a 401 can also mean your token is invalid or expired)"
            ))),
            _ => Err(error),
        },
    }
}

/// Delete hunts in Thorium
///
/// # Arguments
///
/// * `thorium` - A Thorium client
/// * `cmd` - The delete hunts command to execute
async fn delete(thorium: &Thorium, cmd: &DeleteHunts) -> Result<(), Error> {
    // resolve every target up front so a bad target can't abort a partial delete
    let mut targets = Vec::with_capacity(cmd.hunts.len());
    // dedupe resolved ids so the same hunt given by name and by id is only
    // deleted (and listed in the prompt) once instead of 404ing on the repeat
    let mut seen = HashSet::with_capacity(cmd.hunts.len());
    for target in &cmd.hunts {
        // make sure this entity exists and is actually a hunt
        let (id, name, _) = get_hunt(thorium, target).await?;
        // skip targets that resolved to an already listed hunt
        if seen.insert(id) {
            targets.push((name, id));
        }
    }
    // confirm the deletion unless the user opted to skip the prompt
    if !cmd.skip_confirm {
        // make sure we can actually prompt the user
        utils::require_confirm_terminal("--skip-confirm (-y)")?;
        // show what we are about to delete
        println!("Deleting {} hunt(s):", targets.len());
        for (name, id) in &targets {
            println!("  {name} ({})", id.bright_red());
        }
        // remind the user what stays behind
        println!("Hits and their HuntId tags are not deleted with the hunt.");
        // ask the user to confirm this destructive action
        let confirmed = dialoguer::Confirm::new()
            .with_prompt("Delete these hunts?")
            .default(false)
            .interact()?;
        // abort if the user declined
        if !confirmed {
            println!("Aborted");
            return Ok(());
        }
    }
    // track how many deletes failed
    let mut failures = 0;
    // delete each pre-validated hunt in order
    for (name, id) in &targets {
        // delete this hunt, keeping going so one failure can't strand the rest
        match thorium.entities.delete(*id).await {
            Ok(_) => println!("{} hunt {name} ({id})", "Deleted".bright_green()),
            Err(error) => {
                // count and show this failure but keep deleting the rest
                failures += 1;
                eprintln!(
                    "{} to delete hunt {name} ({id}): {error}",
                    "Failed".bright_red()
                );
            }
        }
    }
    // fail the command if any deletes failed
    if failures > 0 {
        return Err(Error::new(format!("Failed to delete {failures} hunt(s)")));
    }
    Ok(())
}

/// Handle all hunt commands
///
/// # Arguments
///
/// * `args` - The arguments passed to Thorctl
/// * `cmd` - The hunts command to execute
pub async fn handle(args: &Args, cmd: &Hunts) -> Result<(), Error> {
    // load our config and instance our client
    let (conf, thorium) = utils::get_client(args).await?;
    // warn about insecure connections if not set to skip
    if !conf.skip_insecure_warning.unwrap_or_default() {
        utils::warn_insecure_conf(&conf)?;
    }
    // check if we need to update
    if !args.skip_update && !conf.skip_update.unwrap_or_default() {
        update::ask_update(&thorium).await?;
    }
    // call the right hunts handler
    match cmd {
        Hunts::Create(cmd) => create(&thorium, cmd).await,
        Hunts::Status(cmd) => status(&thorium, cmd).await,
        Hunts::List(cmd) => list(&thorium, cmd).await,
        Hunts::Hits(cmd) => hits(&thorium, cmd).await,
        Hunts::Rules(cmd) => rules(&thorium, cmd).await,
        Hunts::Cancel(cmd) => cancel(&thorium, cmd).await,
        Hunts::Delete(cmd) => delete(&thorium, cmd).await,
    }
}
