//! Handle rule set related Thorctl commands

use chrono::SecondsFormat;
use owo_colors::OwoColorize;
use std::collections::HashSet;
use std::io::{IsTerminal, Write};
use tabled::settings::Style;
use tabled::{Table, Tabled};
use thorium::models::{
    Entity, EntityKinds, EntityListOpts, EntityMetadata, EntityMetadataRequest,
    EntityMetadataUpdate, EntityRequest, EntityUpdate, RuleSetRequest,
};
use thorium::{Error, Thorium};
use uuid::Uuid;

use crate::args::Args;
use crate::args::rulesets::{
    CreateRuleSet, DeleteRuleSets, GetRuleSet, ListRuleSets, Rulesets, UpdateRuleSet,
};
use crate::handlers::rules::RuleRow;
use crate::handlers::update;
use crate::utils;
use crate::utils::render;

/// Get an entity by its id or exact name and make sure it is a rule set
///
/// # Arguments
///
/// * `thorium` - A Thorium client
/// * `target` - The id or name of the rule set to get
async fn get_ruleset(thorium: &Thorium, target: &str) -> Result<Entity, Error> {
    // resolve this target to a rule set entity
    utils::entities::resolve_entity(thorium, target, &[EntityKinds::RuleSet], "rule set").await
}

/// Resolve a member rule target to its entity id
///
/// Ids pass through without a fetch (the rule set routes validate members
/// anyway, and a member whose rule was deleted after being added must stay
/// removable); only names resolve server side.
///
/// # Arguments
///
/// * `thorium` - A Thorium client
/// * `target` - The id or name of the member rule to resolve
async fn resolve_member(thorium: &Thorium, target: &str) -> Result<Uuid, Error> {
    // members may be rules of any engine
    utils::entities::resolve_entity_id(
        thorium,
        target,
        &[EntityKinds::YaraRule, EntityKinds::SigmaRule],
        "rule",
    )
    .await
}

/// Surface a server 400 response body verbatim in a printable error
///
/// Validation failures (e.g. members that are not rules or the rule set
/// member cap) come back as 400s whose body explains the problem.
///
/// # Arguments
///
/// * `action` - The action that failed
/// * `error` - The error the client returned
fn surface_bad_request(action: &str, error: Error) -> Error {
    // get this error's status code and readable server message
    match (
        error.status().map(|status| status.as_u16()),
        utils::api_error_text(&error),
    ) {
        // surface the server's 400 reason verbatim so validation errors are readable
        (Some(400), Some(msg)) => Error::new(format!("Failed to {action}: {msg}")),
        // all other errors pass through unchanged
        _ => error,
    }
}

impl CreateRuleSet {
    /// Generate an [`EntityRequest`] from the options set in a `CreateRuleSet` command
    ///
    /// Member targets may be given as names, so the handler resolves them to
    /// ids before this runs and passes the results in.
    ///
    /// # Arguments
    ///
    /// * `rule_ids` - The resolved ids of this command's member rule targets
    fn to_req(&self, rule_ids: Vec<Uuid>) -> EntityRequest {
        // build the rule set metadata from our resolved member rule ids
        let metadata = EntityMetadataRequest::RuleSet(RuleSetRequest::new(rule_ids));
        // build the base entity request
        let mut req = EntityRequest::new(self.name.as_str(), metadata, self.groups.clone());
        // add each of our tags
        for (key, value) in &self.tags {
            req = req.tag(key.as_str(), value.as_str());
        }
        // set our description if one was given
        req.description = self.description.clone();
        req
    }
}

/// Create a rule set in Thorium
///
/// # Arguments
///
/// * `thorium` - A Thorium client
/// * `cmd` - The create rule set command to execute
async fn create(thorium: &Thorium, cmd: &CreateRuleSet) -> Result<(), Error> {
    // resolve every member name so bad names fail before anything is created
    let mut rule_ids = Vec::with_capacity(cmd.rules.len());
    for target in &cmd.rules {
        // resolve this member rule target to its entity id
        rule_ids.push(resolve_member(thorium, target).await?);
    }
    // get the number of member rules we are adding
    let cnt = rule_ids.len();
    // build the entity request for this rule set
    let req = cmd.to_req(rule_ids);
    // create this rule set, surfacing any validation errors
    let resp = thorium
        .entities
        .create(req)
        .await
        .map_err(|error| surface_bad_request("create rule set", error))?;
    // print the created rule set's name and id
    println!(
        "{} rule set {} ({}) with {cnt} member rule(s)",
        "Created".bright_green(),
        resp.name,
        resp.id
    );
    Ok(())
}

/// Render a rule set entity as a human readable summary
///
/// # Arguments
///
/// * `entity` - The rule set entity to render
/// * `out` - The writer to render this rule set to
/// * `ansi` - Whether to style the output with ANSI colors
fn render_ruleset(entity: &Entity, out: &mut dyn Write, ansi: bool) -> Result<(), Error> {
    // pull the hydrated member rules out of this rule set's metadata
    let members = match &entity.metadata {
        EntityMetadata::RuleSet(ruleset) => ruleset.rules.as_slice(),
        // get_ruleset only returns rule sets so other kinds cannot appear
        _ => &[],
    };
    // print this rule set's name and groups as a title line
    writeln!(
        out,
        "{}",
        render::header(&entity.name, &entity.groups.join(", "), ansi)
    )?;
    // print this rule set's summary fields
    writeln!(out, "{}", render::field("ID", &entity.id.to_string(), ansi))?;
    writeln!(
        out,
        "{}",
        render::field(
            "Created",
            &entity.created.to_rfc3339_opts(SecondsFormat::Secs, true),
            ansi
        )
    )?;
    writeln!(
        out,
        "{}",
        render::field("Submitter", &entity.submitter, ansi)
    )?;
    // print this rule set's description if one was set
    if let Some(description) = &entity.description {
        writeln!(out, "{}", render::field("Description", description, ansi))?;
    }
    // print the member rules under a label like a rule's source
    writeln!(out, "{}", render::label("Rules", ansi))?;
    if members.is_empty() {
        // make an empty rule set read clearly
        writeln!(out, "  (none)")?;
    } else {
        // print the members as a content sized table
        writeln!(
            out,
            "{}",
            Table::new(members.iter().map(RuleRow::from)).with(Style::psql())
        )?;
    }
    Ok(())
}

/// Get a rule set's full details
///
/// # Arguments
///
/// * `thorium` - A Thorium client
/// * `cmd` - The get rule set command to execute
async fn get(thorium: &Thorium, cmd: &GetRuleSet) -> Result<(), Error> {
    // get this rule set with its hydrated member rules
    let entity = get_ruleset(thorium, &cmd.ruleset).await?;
    // print this rule set as a human readable summary or JSON (--condensed implies --json)
    match (&cmd.output, cmd.condensed, cmd.json) {
        (None, false, false) => {
            // style the summary only when stdout is a terminal
            let ansi = std::io::stdout().is_terminal();
            // render the human readable summary to stdout
            render_ruleset(&entity, &mut std::io::stdout(), ansi)?;
        }
        (None, true, _) => {
            // print condensed JSON to stdout
            serde_json::to_writer(std::io::stdout(), &entity)?;
            println!();
        }
        (None, false, true) => {
            // print pretty JSON to stdout
            serde_json::to_writer_pretty(std::io::stdout(), &entity)?;
            println!();
        }
        (Some(output_path), false, false) => {
            // write the human readable summary to the output file without ANSI styling
            let mut output_file = std::fs::File::create(output_path)?;
            render_ruleset(&entity, &mut output_file, false)?;
        }
        (Some(output_path), true, _) => {
            // write condensed JSON to the output file
            let mut output_file = std::fs::File::create(output_path)?;
            serde_json::to_writer(&mut output_file, &entity)?;
            writeln!(&mut output_file)?;
        }
        (Some(output_path), false, true) => {
            // write pretty JSON to the output file
            let mut output_file = std::fs::File::create(output_path)?;
            serde_json::to_writer_pretty(&mut output_file, &entity)?;
            writeln!(&mut output_file)?;
        }
    }
    Ok(())
}

/// A row in the rule set list table
#[derive(Tabled)]
struct RuleSetRow {
    /// The name of this rule set
    #[tabled(rename = "NAME")]
    name: String,
    /// How many member rules this rule set has
    #[tabled(rename = "RULES")]
    rules: String,
    /// This rule set's unique id
    #[tabled(rename = "ID")]
    id: String,
    /// When this rule set was created
    #[tabled(rename = "CREATED")]
    created: String,
}

impl From<&Entity> for RuleSetRow {
    /// Build a table row from a rule set entity
    ///
    /// # Arguments
    ///
    /// * `entity` - The rule set entity to build a table row from
    fn from(entity: &Entity) -> Self {
        // get this rule set's member count from its hydrated metadata
        let rules = match &entity.metadata {
            EntityMetadata::RuleSet(ruleset) => ruleset.rules.len(),
            _ => 0,
        };
        RuleSetRow {
            name: entity.name.clone(),
            rules: rules.to_string(),
            id: entity.id.to_string(),
            // seconds precision keeps the timestamp column narrow
            created: entity.created.to_rfc3339_opts(SecondsFormat::Secs, true),
        }
    }
}

impl ListRuleSets {
    /// Generate an [`EntityListOpts`] from the options set in a `ListRuleSets` command
    fn to_opts(&self) -> EntityListOpts {
        // restrict our search to rule set entities
        let mut opts = EntityListOpts::default()
            .kind(EntityKinds::RuleSet)
            .groups(self.groups.clone())
            .page_size(self.page_size);
        // continue from a cursor if one was given
        if let Some(cursor) = self.cursor {
            opts = opts.cursor(cursor);
        }
        // set a limit unless the user asked for everything
        if !self.no_limit {
            opts = opts.limit(self.limit);
        }
        // add each of our tag filters
        for (key, value) in &self.tags {
            opts.tag_ref(key.as_str(), value.as_str());
        }
        opts
    }
}

/// List the rule sets in Thorium
///
/// # Arguments
///
/// * `thorium` - A Thorium client
/// * `cmd` - The list rule sets command to execute
async fn list(thorium: &Thorium, cmd: &ListRuleSets) -> Result<(), Error> {
    // build our list options
    let opts = cmd.to_opts();
    // start listing rule sets with details so we can count their members
    let mut cursor = thorium.entities.list_details(&opts).await?;
    // collect all of our rule sets
    let mut rulesets = Vec::default();
    loop {
        // take this page of rule sets
        rulesets.append(&mut cursor.data);
        // check if this cursor is exhausted
        if cursor.exhausted() {
            break;
        }
        // get the next page of rule sets
        cursor.refill().await?;
    }
    // the tag walk cannot filter by kind server side, so enforce the rule set
    // kind per line or a tag filter would list any tagged entity as a rule set
    rulesets.retain(|ruleset| ruleset.kind == EntityKinds::RuleSet);
    // print our rule sets as JSON or a table
    match (&cmd.output, cmd.condensed, cmd.json) {
        (None, false, false) => {
            // print our rule sets as a content sized table
            println!(
                "{}",
                Table::new(rulesets.iter().map(RuleSetRow::from)).with(Style::psql())
            );
        }
        (None, true, _) => {
            // print condensed JSON to stdout
            serde_json::to_writer(std::io::stdout(), &rulesets)?;
            println!();
        }
        (None, false, true) => {
            // print pretty JSON to stdout
            serde_json::to_writer_pretty(std::io::stdout(), &rulesets)?;
            println!();
        }
        (Some(output_path), true, _) => {
            // write condensed JSON to the output file
            let mut output_file = std::fs::File::create(output_path)?;
            serde_json::to_writer(&mut output_file, &rulesets)?;
            writeln!(&mut output_file)?;
        }
        (Some(output_path), false, _) => {
            // write pretty JSON to the output file
            let mut output_file = std::fs::File::create(output_path)?;
            serde_json::to_writer_pretty(&mut output_file, &rulesets)?;
            writeln!(&mut output_file)?;
        }
    }
    Ok(())
}

impl UpdateRuleSet {
    /// Generate an [`EntityUpdate`] from the options set in an `UpdateRuleSet` command
    ///
    /// Member targets may be given as names, so the handler resolves them to
    /// ids before this runs and passes the results in.
    ///
    /// # Arguments
    ///
    /// * `add_rules` - The resolved ids of the member rule targets to add
    /// * `remove_rules` - The resolved ids of the member rule targets to remove
    fn to_update(&self, add_rules: Vec<Uuid>, remove_rules: Vec<Uuid>) -> EntityUpdate {
        // build the general entity updates
        let mut entity_update = EntityUpdate {
            name: self.opts.name.clone(),
            add_groups: self.opts.add_groups.clone(),
            remove_groups: self.opts.remove_groups.clone(),
            description: self.opts.description.clone(),
            clear_description: self.opts.clear_description,
            metadata: None,
        };
        // only set a metadata update if we have member changes
        if !add_rules.is_empty() || !remove_rules.is_empty() {
            entity_update.metadata = Some(EntityMetadataUpdate::RuleSet {
                add_rules,
                remove_rules,
            });
        }
        entity_update
    }
}

/// Update a rule set in Thorium
///
/// # Arguments
///
/// * `thorium` - A Thorium client
/// * `cmd` - The update rule set command to execute
async fn update(thorium: &Thorium, cmd: &UpdateRuleSet) -> Result<(), Error> {
    // make sure the target exists and is a rule set before updating it
    let entity = get_ruleset(thorium, &cmd.ruleset).await?;
    // resolve every member name to add so bad names fail before the update
    let mut add_rules = Vec::with_capacity(cmd.opts.add_rules.len());
    for target in &cmd.opts.add_rules {
        // resolve this member rule target to its entity id
        add_rules.push(resolve_member(thorium, target).await?);
    }
    // resolve every member target to remove
    let mut remove_rules = Vec::with_capacity(cmd.opts.remove_rules.len());
    for target in &cmd.opts.remove_rules {
        // resolve this member rule target to its entity id
        remove_rules.push(resolve_member(thorium, target).await?);
    }
    // build the update to apply from the resolved member ids
    let entity_update = cmd.to_update(add_rules, remove_rules);
    // apply this update to the resolved id, surfacing any validation errors
    thorium
        .entities
        .update(entity.id, entity_update)
        .await
        .map_err(|error| surface_bad_request("update rule set", error))?;
    // print the updated rule set's name and id, preferring a just-applied rename
    let shown = cmd.opts.name.as_deref().unwrap_or(&entity.name);
    println!(
        "{} rule set {} ({})",
        "Updated".bright_green(),
        shown,
        entity.id
    );
    Ok(())
}

/// Delete rule sets from Thorium
///
/// Deleting a rule set does not affect existing hunts since hunts freeze a
/// snapshot of their rules at creation.
///
/// # Arguments
///
/// * `thorium` - A Thorium client
/// * `cmd` - The delete rule sets command to execute
async fn delete(thorium: &Thorium, cmd: &DeleteRuleSets) -> Result<(), Error> {
    // resolve every target first so a bad target fails before anything is deleted
    let mut targets = Vec::with_capacity(cmd.rulesets.len());
    // dedupe resolved ids so the same rule set given by name and by id is only
    // deleted (and listed in the prompt) once instead of 404ing on the repeat
    let mut seen = HashSet::with_capacity(cmd.rulesets.len());
    for target in &cmd.rulesets {
        // make sure this entity exists and is a rule set
        let entity = get_ruleset(thorium, target).await?;
        // skip targets that resolved to an already listed rule set
        if seen.insert(entity.id) {
            targets.push(entity);
        }
    }
    // confirm the deletion unless the user opted to skip the prompt
    if !cmd.skip_confirm {
        // make sure we can actually prompt the user
        utils::require_confirm_terminal("--skip-confirm (-y)")?;
        // show what we are about to delete
        println!("Deleting {} rule set(s):", targets.len());
        for entity in &targets {
            println!(
                "  {} ({})",
                entity.name.bright_red(),
                entity.id.bright_red()
            );
        }
        // remind the user that hunts keep their frozen rule snapshots
        println!(
            "Existing hunts are unaffected: hunts freeze a snapshot of their rules at creation."
        );
        // ask the user to confirm this destructive action
        let confirmed = dialoguer::Confirm::new()
            .with_prompt("Delete these rule sets?")
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
    // delete each rule set in order
    for entity in &targets {
        // delete this rule set, keeping going so one failure can't strand the rest
        match thorium.entities.delete(entity.id).await {
            // print this successful delete
            Ok(_) => println!(
                "{} rule set {} ({})",
                "Deleted".bright_green(),
                entity.name,
                entity.id
            ),
            Err(error) => {
                // count and show this failure but keep deleting the rest
                failures += 1;
                eprintln!(
                    "{} to delete rule set {} ({}): {error}",
                    "Failed".bright_red(),
                    entity.name,
                    entity.id
                );
            }
        }
    }
    // fail the command if any deletes failed
    if failures > 0 {
        return Err(Error::new(format!(
            "Failed to delete {failures} rule set(s)"
        )));
    }
    Ok(())
}

/// Handle all rule set commands
///
/// # Arguments
///
/// * `args` - The arguments passed to Thorctl
/// * `cmd` - The rulesets command to execute
pub async fn handle(args: &Args, cmd: &Rulesets) -> Result<(), Error> {
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
    // call the right rule sets handler
    match cmd {
        Rulesets::Create(cmd) => create(&thorium, cmd).await,
        Rulesets::Get(cmd) => get(&thorium, cmd).await,
        Rulesets::List(cmd) => list(&thorium, cmd).await,
        Rulesets::Update(cmd) => update(&thorium, cmd).await,
        Rulesets::Delete(cmd) => delete(&thorium, cmd).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use std::collections::HashMap;
    use thorium::models::{RuleSet, YaraRule, YaraRuleAppliesTo};

    /// Build a yara rule entity for render tests
    ///
    /// # Arguments
    ///
    /// * `name` - The name for this test entity
    fn member(name: &str) -> Entity {
        // build minimal yara metadata for this member
        let rule = YaraRule::new("rule a { condition: true }", YaraRuleAppliesTo::Samples)
            .expect("failed to build yara rule");
        Entity {
            id: Uuid::nil(),
            name: name.to_owned(),
            kind: EntityKinds::YaraRule,
            metadata: EntityMetadata::YaraRule(rule),
            description: None,
            submitter: "mcarson".to_owned(),
            groups: vec!["demolab".to_owned()],
            tags: HashMap::default(),
            image: None,
            created: DateTime::<Utc>::from_timestamp(1_760_000_000, 0)
                .expect("timestamp was out of range"),
        }
    }

    /// The human readable summary must include fields and a member table
    #[test]
    fn render_ruleset_plain() {
        // build a rule set entity carrying two hydrated members
        let members = vec![member("demo_a"), member("demo_b")];
        let mut entity = member("demo-pack");
        entity.kind = EntityKinds::RuleSet;
        entity.metadata = EntityMetadata::RuleSet(RuleSet { rules: members });
        entity.description = Some("A demo pack".to_owned());
        // render this rule set without ANSI styling
        let mut out = Vec::new();
        render_ruleset(&entity, &mut out, false).expect("failed to render rule set");
        let rendered = String::from_utf8(out).expect("rendered rule set was not utf-8");
        // the summary fields must all be present
        assert!(rendered.contains("demo-pack (demolab)"));
        assert!(rendered.contains("Submitter: mcarson"));
        assert!(rendered.contains("Description: A demo pack"));
        // the members must render as a table under the Rules label
        assert!(rendered.contains("Rules:"));
        assert!(rendered.contains("ENGINE"));
        assert!(rendered.contains("demo_a"));
        assert!(rendered.contains("demo_b"));
        // nothing may be ANSI styled without a terminal
        assert!(!rendered.contains('\u{1b}'));
    }

    /// An empty rule set must render a clear placeholder member list
    #[test]
    fn render_ruleset_empty_members() {
        // build a rule set entity with no members
        let mut entity = member("empty-pack");
        entity.kind = EntityKinds::RuleSet;
        entity.metadata = EntityMetadata::RuleSet(RuleSet {
            rules: Vec::default(),
        });
        // render this rule set without ANSI styling
        let mut out = Vec::new();
        render_ruleset(&entity, &mut out, false).expect("failed to render rule set");
        let rendered = String::from_utf8(out).expect("rendered rule set was not utf-8");
        // the empty membership must render its placeholder
        assert!(rendered.contains("Rules:\n  (none)"));
    }
}
