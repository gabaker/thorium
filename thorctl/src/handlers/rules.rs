//! Handles YARA/Sigma detection rule commands

use chrono::SecondsFormat;
use owo_colors::OwoColorize;
use std::collections::{BTreeSet, HashSet};
use std::io::{IsTerminal, Write};
use std::path::Path;
use tabled::settings::Style;
use tabled::{Table, Tabled};
use thorium::models::{
    Cursor, Entity, EntityKinds, EntityListOpts, EntityMetadata, EntityMetadataRequest,
    EntityMetadataUpdate, EntityRequest, EntityUpdate, SigmaRule, SigmaRuleAppliesTo,
    TagDeleteRequest, YaraRule, YaraRuleAppliesTo,
};
use thorium::{Error, Thorium};

use super::update::ask_update;
use crate::args::Args;
use crate::args::rules::{
    CreateRule, DeleteRules, ExemptRule, GetRule, ListRules, RuleAppliesTo, RuleEngine, Rules,
    UnquarantineRule, UpdateRule, parse_tag,
};
use crate::err_not_admin;
use crate::utils;
use crate::utils::render;

mod edit;
mod import;
mod notifications;

/// Read a rule source file into a string
///
/// # Arguments
///
/// * `path` - The path to the rule source file to read
async fn read_rule_file(path: &Path) -> Result<String, Error> {
    // read this rule source file into a string
    tokio::fs::read_to_string(path).await.map_err(|error| {
        Error::new(format!(
            "Failed to read rule file at '{}': {}",
            path.to_string_lossy(),
            error
        ))
    })
}

/// Get the engine name to print for an entity kind
///
/// # Arguments
///
/// * `kind` - The entity kind to get an engine name for
pub(crate) fn engine_name(kind: &EntityKinds) -> &str {
    // map rule kinds to their engine names
    match kind {
        EntityKinds::YaraRule => "yara",
        EntityKinds::SigmaRule => "sigma",
        // non rule kinds should never appear here but print their kind if they do
        other => other.as_str(),
    }
}

/// Build the engine-specific rule metadata for some rule source
///
/// A user-supplied score is passed through when given; otherwise YARA rules
/// keep the model's default score of 0 and Sigma rules keep the score the
/// model derives from their YAML `level`. The applies-to list defaults to
/// samples for YARA rules and windows processes for Sigma rules.
///
/// # Arguments
///
/// * `engine` - The engine this rule targets
/// * `rule_src` - The rule source to build metadata for
/// * `applies_to` - The kinds of data this rule applies to
/// * `score` - The user-supplied score if one was given
fn build_metadata(
    engine: RuleEngine,
    rule_src: String,
    applies_to: &[RuleAppliesTo],
    score: Option<i64>,
) -> Result<EntityMetadataRequest, Error> {
    // build the right metadata for this engine
    match engine {
        RuleEngine::Yara => {
            // map our applies to args to yara applies to values
            let applies = applies_to
                .iter()
                .map(|applies| applies.to_yara())
                .collect::<Result<Vec<YaraRuleAppliesTo>, Error>>()?;
            // build this yara rule which checks the source is non-empty and under the size cap
            let mut rule = YaraRule::new(rule_src, YaraRuleAppliesTo::Samples)
                .map_err(|error| Error::new(format!("Invalid YARA rule: {error}")))?;
            // replace the default applies to list if the user set one
            if !applies.is_empty() {
                rule.applies_to = applies;
            }
            // pass a user-supplied score through if one was given
            if let Some(score) = score {
                rule = rule.score(score);
            }
            Ok(EntityMetadataRequest::YaraRule(rule))
        }
        RuleEngine::Sigma => {
            // map our applies to args to sigma applies to values
            let applies = applies_to
                .iter()
                .map(|applies| applies.to_sigma())
                .collect::<Result<Vec<SigmaRuleAppliesTo>, Error>>()?;
            // build this sigma rule which validates the YAML and derives a score from its level
            let mut rule = SigmaRule::new(rule_src, SigmaRuleAppliesTo::WindowsProcesses)
                .map_err(|error| Error::new(format!("Invalid Sigma rule: {error}")))?;
            // replace the default applies to list if the user set one
            if !applies.is_empty() {
                rule.applies_to = applies;
            }
            // override the level-derived score if the user supplied one
            if let Some(score) = score {
                rule = rule.score(score);
            }
            Ok(EntityMetadataRequest::SigmaRule(rule))
        }
    }
}

/// Get a rule entity by its id or exact name and make sure it is actually a rule
///
/// # Arguments
///
/// * `thorium` - A Thorium client
/// * `target` - The id or name of the rule to get
async fn get_rule(thorium: &Thorium, target: &str) -> Result<Entity, Error> {
    // resolve this target to a detection rule of either engine
    utils::entities::resolve_entity(
        thorium,
        target,
        &[EntityKinds::YaraRule, EntityKinds::SigmaRule],
        "rule",
    )
    .await
}

impl CreateRule {
    /// Build an [`EntityRequest`] from this create rule command
    ///
    /// # Arguments
    ///
    /// * `rule_src` - The rule source read from this command's rule file
    fn to_req(&self, rule_src: String) -> Result<EntityRequest, Error> {
        // build the engine-specific rule metadata
        let metadata = build_metadata(self.engine, rule_src, &self.applies_to, self.score)?;
        // build the base entity request
        let mut req = EntityRequest::new(self.name.clone(), metadata, self.groups.clone());
        // add any tags to this request
        for raw in &self.tags {
            // split this tag into its key and value
            let (key, value) = parse_tag(raw)?;
            // add this tag to our request
            req = req.tag(key, value);
        }
        // set our description if one was given
        req.description = self.description.clone();
        Ok(req)
    }
}

/// Create a detection rule in Thorium
///
/// # Arguments
///
/// * `thorium` - A Thorium client
/// * `cmd` - The create rule command to execute
async fn create(thorium: &Thorium, cmd: &CreateRule) -> Result<(), Error> {
    // read this rule's source from disk
    let rule_src = read_rule_file(&cmd.rule_file).await?;
    // build the entity request for this rule
    let req = cmd.to_req(rule_src)?;
    // try to create this rule in Thorium
    match thorium.entities.create(req).await {
        Ok(resp) => {
            // show the created rule's name and id
            println!(
                "{} {} rule '{}' ({})",
                "Created".bright_green(),
                cmd.engine.as_str(),
                resp.name,
                resp.id
            );
            Ok(())
        }
        Err(error) => match error.status().map(|status| status.as_u16()) {
            // surface server-side validation errors (e.g. yara-x compile output) verbatim
            Some(400) => Err(Error::new(
                utils::api_error_text(&error).unwrap_or_else(|| "Invalid rule".to_owned()),
            )),
            _ => Err(error),
        },
    }
}

/// Render a rule entity as a human readable summary
///
/// # Arguments
///
/// * `entity` - The rule entity to render
/// * `out` - The writer to render this rule to
/// * `ansi` - Whether to style the output with ANSI colors
fn render_rule(entity: &Entity, out: &mut dyn Write, ansi: bool) -> Result<(), Error> {
    // pull the engine specific details out of this rule's metadata
    let (applies_to, score, source, exempt) = match &entity.metadata {
        EntityMetadata::YaraRule(rule) => (
            rule.applies_to
                .iter()
                .map(|applies| applies.as_str())
                .collect::<Vec<&str>>()
                .join(", "),
            rule.score,
            rule.rule.as_str(),
            rule.quarantine_exempt,
        ),
        EntityMetadata::SigmaRule(rule) => (
            rule.applies_to
                .iter()
                .map(|applies| applies.as_str())
                .collect::<Vec<&str>>()
                .join(", "),
            rule.score,
            rule.rule.as_str(),
            false,
        ),
        // get_rule only returns rule kinds so other kinds cannot appear
        _ => ("-".to_owned(), 0, "", false),
    };
    // print this rule's name and groups as a title line
    writeln!(
        out,
        "{}",
        render::header(&entity.name, &entity.groups.join(", "), ansi)
    )?;
    // print this rule's summary fields
    writeln!(
        out,
        "{}",
        render::field("Engine", engine_name(&entity.kind), ansi)
    )?;
    writeln!(out, "{}", render::field("ID", &entity.id.to_string(), ansi))?;
    writeln!(out, "{}", render::field("Score", &score.to_string(), ansi))?;
    writeln!(out, "{}", render::field("Applies To", &applies_to, ansi))?;
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
    // print this rule's quarantine status when it carries one
    if let Some(values) = entity.tags.get(YaraRule::STATUS_TAG) {
        // flatten the status values into a sorted list
        let mut statuses = values.keys().map(String::as_str).collect::<Vec<&str>>();
        statuses.sort_unstable();
        writeln!(
            out,
            "{}",
            render::field("Quarantine", &statuses.join(", "), ansi)
        )?;
    }
    // print the exemption only when it is set so untouched rules render as before
    if exempt {
        writeln!(out, "{}", render::field("Quarantine Exempt", "true", ansi))?;
    }
    // print this rule's description if one was set
    if let Some(description) = &entity.description {
        writeln!(out, "{}", render::field("Description", description, ansi))?;
    }
    // print this rule's tags one per line so long intrinsic tags (e.g. the
    // 64 char rule hash) never wrap mid value in narrow terminals
    if !entity.tags.is_empty() {
        // flatten this rule's tag map into sorted key=value pairs
        let mut tags = entity
            .tags
            .iter()
            .flat_map(|(key, values)| values.keys().map(move |value| format!("{key}={value}")))
            .collect::<Vec<String>>();
        tags.sort_unstable();
        // print the tags under a label like the rule source
        writeln!(out, "{}", render::label("Tags", ansi))?;
        for tag in &tags {
            writeln!(out, "  {tag}")?;
        }
    }
    // print this rule's source verbatim under a label
    writeln!(out, "{}", render::label("Rule", ansi))?;
    write!(out, "{source}")?;
    // make sure the source ends with a newline
    if !source.ends_with('\n') {
        writeln!(out)?;
    }
    Ok(())
}

/// Get a detection rule's full details
///
/// # Arguments
///
/// * `thorium` - A Thorium client
/// * `cmd` - The get rule command to execute
async fn get(thorium: &Thorium, cmd: &GetRule) -> Result<(), Error> {
    // resolve this rule and make sure it is one
    let entity = get_rule(thorium, &cmd.rule).await?;
    // print this rule as a human readable summary or JSON (--condensed implies --json)
    match (&cmd.output, cmd.condensed, cmd.json) {
        (None, false, false) => {
            // style the summary only when stdout is a terminal
            let ansi = std::io::stdout().is_terminal();
            // render the human readable summary to stdout
            render_rule(&entity, &mut std::io::stdout(), ansi)?;
        }
        (None, true, _) => {
            serde_json::to_writer(std::io::stdout(), &entity)?;
            println!();
        }
        (None, false, true) => {
            serde_json::to_writer_pretty(std::io::stdout(), &entity)?;
            println!();
        }
        (Some(output_path), false, false) => {
            // write the human readable summary to the output file without ANSI styling
            let mut output_file = std::fs::File::create(output_path)?;
            render_rule(&entity, &mut output_file, false)?;
        }
        (Some(output_path), true, _) => {
            let mut output_file = std::fs::File::create(output_path)?;
            serde_json::to_writer(&mut output_file, &entity)?;
            writeln!(&mut output_file)?;
        }
        (Some(output_path), false, true) => {
            let mut output_file = std::fs::File::create(output_path)?;
            serde_json::to_writer_pretty(&mut output_file, &entity)?;
            writeln!(&mut output_file)?;
        }
    }
    Ok(())
}

/// A row in the rules list table
#[derive(Tabled)]
pub(crate) struct RuleRow {
    /// The name of this rule entity
    #[tabled(rename = "NAME")]
    name: String,
    /// The engine this rule targets
    #[tabled(rename = "ENGINE")]
    engine: String,
    /// This rule entity's unique id
    #[tabled(rename = "ID")]
    id: String,
    /// When this rule was created
    #[tabled(rename = "CREATED")]
    created: String,
}

impl From<&Entity> for RuleRow {
    /// Build a table row from a rule entity
    ///
    /// # Arguments
    ///
    /// * `rule` - The rule entity to build a table row from
    fn from(rule: &Entity) -> Self {
        RuleRow {
            name: rule.name.clone(),
            engine: engine_name(&rule.kind).to_owned(),
            id: rule.id.to_string(),
            // seconds precision keeps the timestamp column narrow
            created: rule.created.to_rfc3339_opts(SecondsFormat::Secs, true),
        }
    }
}

/// List detection rules in Thorium
///
/// # Arguments
///
/// * `thorium` - A Thorium client
/// * `cmd` - The list rules command to execute
async fn list(thorium: &Thorium, cmd: &ListRules) -> Result<(), Error> {
    // build our list options
    let mut opts = EntityListOpts::default()
        .groups(cmd.groups.clone())
        .page_size(cmd.page_size);
    // apply our limit if one was set
    if !cmd.no_limit {
        opts = opts.limit(cmd.limit);
    }
    // continue from a cursor if one was given
    if let Some(cursor) = cmd.cursor {
        opts = opts.cursor(cursor);
    }
    // restrict our list to the requested engine or all rule kinds
    opts = match cmd.engine {
        Some(engine) => opts.kind(engine.kind()),
        None => opts.kinds([EntityKinds::YaraRule, EntityKinds::SigmaRule]),
    };
    // add any tag filters
    for raw in &cmd.tags {
        // split this tag into its key and value
        let (key, value) = parse_tag(raw)?;
        // add this tag filter to our options
        opts = opts.tag(key, value);
    }
    // start listing rules with details
    let mut cursor: Cursor<Entity> = thorium.entities.list_details(&opts).await?;
    // collect all of our rules
    let mut rules = Vec::default();
    loop {
        // take this page of rules
        rules.append(&mut cursor.data);
        // check if this cursor is exhausted
        if cursor.exhausted() {
            break;
        }
        // get the next page of rules
        cursor.refill().await?;
    }
    // the tag walk cannot filter by kind server side, so enforce the rule
    // kinds per line or a tag filter would list any tagged entity as a rule
    let wanted: Vec<EntityKinds> = match cmd.engine {
        Some(engine) => vec![engine.kind()],
        None => vec![EntityKinds::YaraRule, EntityKinds::SigmaRule],
    };
    rules.retain(|rule| wanted.contains(&rule.kind));
    // print our rules as JSON or a table
    match (&cmd.output, cmd.condensed, cmd.json) {
        (None, false, false) => {
            // print our rules as a content sized table
            println!(
                "{}",
                Table::new(rules.iter().map(RuleRow::from)).with(Style::psql())
            );
        }
        (None, true, _) => {
            serde_json::to_writer(std::io::stdout(), &rules)?;
            println!();
        }
        (None, false, true) => {
            serde_json::to_writer_pretty(std::io::stdout(), &rules)?;
            println!();
        }
        (Some(output_path), true, _) => {
            let mut output_file = std::fs::File::create(output_path)?;
            serde_json::to_writer(&mut output_file, &rules)?;
            writeln!(&mut output_file)?;
        }
        (Some(output_path), false, _) => {
            let mut output_file = std::fs::File::create(output_path)?;
            serde_json::to_writer_pretty(&mut output_file, &rules)?;
            writeln!(&mut output_file)?;
        }
    }
    Ok(())
}

/// Update a detection rule in Thorium
///
/// # Arguments
///
/// * `thorium` - A Thorium client
/// * `cmd` - The update rule command to execute
async fn update(thorium: &Thorium, cmd: &UpdateRule) -> Result<(), Error> {
    // resolve this rule first so we know which engine it belongs to
    let entity = get_rule(thorium, &cmd.rule).await?;
    // read the new rule source if one was given
    let rule_src = match &cmd.opts.rule_file {
        Some(path) => Some(read_rule_file(path).await?),
        None => None,
    };
    // start building our entity update
    let mut update = EntityUpdate::default();
    // set a new name if one was given
    if let Some(name) = &cmd.opts.name {
        update = update.name(name.clone());
    }
    // add any new groups
    for group in &cmd.opts.add_groups {
        update = update.group(group.clone());
    }
    // remove any old groups
    for group in &cmd.opts.remove_groups {
        update = update.remove_group(group.clone());
    }
    // set a new description if one was given
    if let Some(description) = &cmd.opts.description {
        update = update.description(description.clone());
    }
    // clear the description if requested
    if cmd.opts.clear_description {
        update = update.clear_description();
    }
    // check if any engine-specific metadata fields were set
    let has_metadata = rule_src.is_some()
        || cmd.opts.score.is_some()
        || !cmd.opts.add_applies_to.is_empty()
        || !cmd.opts.remove_applies_to.is_empty();
    if has_metadata {
        // build the metadata update matching this rule's engine
        let metadata = match entity.kind {
            EntityKinds::YaraRule => EntityMetadataUpdate::YaraRule {
                yara_rule: rule_src,
                score: cmd.opts.score,
                add_applies_to: cmd
                    .opts
                    .add_applies_to
                    .iter()
                    .map(|applies| applies.to_yara())
                    .collect::<Result<Vec<YaraRuleAppliesTo>, Error>>()?,
                remove_applies_to: cmd
                    .opts
                    .remove_applies_to
                    .iter()
                    .map(|applies| applies.to_yara())
                    .collect::<Result<Vec<YaraRuleAppliesTo>, Error>>()?,
                // action updates are not exposed in thorctl v1
                add_actions: Vec::default(),
                remove_actions: BTreeSet::default(),
            },
            EntityKinds::SigmaRule => EntityMetadataUpdate::SigmaRule {
                sigma_rule: rule_src,
                score: cmd.opts.score,
                add_applies_to: cmd
                    .opts
                    .add_applies_to
                    .iter()
                    .map(|applies| applies.to_sigma())
                    .collect::<Result<Vec<SigmaRuleAppliesTo>, Error>>()?,
                remove_applies_to: cmd
                    .opts
                    .remove_applies_to
                    .iter()
                    .map(|applies| applies.to_sigma())
                    .collect::<Result<Vec<SigmaRuleAppliesTo>, Error>>()?,
                // action updates are not exposed in thorctl v1
                add_actions: Vec::default(),
                remove_actions: BTreeSet::default(),
            },
            // get_rule already rejected non-rule kinds
            _ => unreachable!("non-rule kinds are rejected when resolving the rule"),
        };
        // set the metadata update
        update = update.metadata(metadata);
    }
    // send this update to Thorium using the resolved id
    match thorium.entities.update(entity.id, update).await {
        Ok(_) => {
            // show that this rule was updated
            println!(
                "{} rule '{}' ({})",
                "Updated".bright_green(),
                entity.name,
                entity.id
            );
            Ok(())
        }
        Err(error) => match error.status().map(|status| status.as_u16()) {
            // surface server-side validation errors (e.g. yara-x compile output) verbatim
            Some(400) => Err(Error::new(
                utils::api_error_text(&error).unwrap_or_else(|| "Invalid rule update".to_owned()),
            )),
            _ => Err(error),
        },
    }
}

/// Delete detection rules from Thorium
///
/// # Arguments
///
/// * `thorium` - A Thorium client
/// * `cmd` - The delete rules command to execute
async fn delete(thorium: &Thorium, cmd: &DeleteRules) -> Result<(), Error> {
    // resolve every rule first so bad targets fail before anything is deleted
    let mut rules = Vec::with_capacity(cmd.rules.len());
    // dedupe resolved ids so the same rule given by name and by id is only
    // deleted (and listed in the prompt) once instead of 404ing on the repeat
    let mut seen = HashSet::with_capacity(cmd.rules.len());
    for target in &cmd.rules {
        // resolve this rule and make sure it is one
        let rule = get_rule(thorium, target).await?;
        // skip targets that resolved to an already listed rule
        if seen.insert(rule.id) {
            rules.push(rule);
        }
    }
    // confirm the deletion unless the user opted to skip the prompt
    if !cmd.skip_confirm {
        // make sure we can actually prompt the user
        utils::require_confirm_terminal("--skip-confirm (-y)")?;
        // show what we are about to delete
        println!("Deleting {} rule(s):", rules.len());
        for rule in &rules {
            println!("  {} ({})", rule.name.bright_red(), rule.id);
        }
        // ask the user to confirm this destructive action
        let confirmed = dialoguer::Confirm::new()
            .with_prompt("Delete these rules?")
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
    // delete each rule
    for rule in &rules {
        match thorium.entities.delete(rule.id).await {
            Ok(_) => println!(
                "{} rule '{}' ({})",
                "Deleted".bright_green(),
                rule.name,
                rule.id
            ),
            Err(error) => {
                // count and show this failure but keep deleting the rest
                failures += 1;
                eprintln!(
                    "{} to delete '{}' ({}): {}",
                    "Failed".bright_red(),
                    rule.name,
                    rule.id,
                    error
                );
            }
        }
    }
    // fail the command if any deletes failed
    if failures > 0 {
        return Err(Error::new(format!("Failed to delete {failures} rule(s)")));
    }
    Ok(())
}

/// Re-admit a quarantined rule by deleting its clearable status values
///
/// Only the Noisy/Slow quarantine statuses are clearable (the same set the
/// API's exemption route clears): a `Broken` rule cannot scan, and deleting
/// its tag would just get it re-tagged (and its submitter re-notified) by the
/// next scanner restart, so only fixing the rule source clears Broken.
///
/// # Arguments
///
/// * `thorium` - A Thorium client
/// * `cmd` - The unquarantine rule command to execute
async fn unquarantine(thorium: &Thorium, cmd: &UnquarantineRule) -> Result<(), Error> {
    // resolve this rule and make sure it is one
    let entity = get_rule(thorium, &cmd.rule).await?;
    // collect the status values this rule carries
    let all_values = entity
        .tags
        .get(YaraRule::STATUS_TAG)
        .map_or_else(Vec::new, |values| {
            values.keys().cloned().collect::<Vec<String>>()
        });
    // keep only the clearable noisy/slow quarantine statuses
    let mut values: Vec<String> = all_values
        .iter()
        .filter(|value| *value == YaraRule::STATUS_NOISY || *value == YaraRule::STATUS_SLOW)
        .cloned()
        .collect();
    // remember whether this rule is broken so output can explain it
    let broken = all_values
        .iter()
        .any(|value| value == YaraRule::STATUS_BROKEN);
    // rules with no clearable status are either broken or already in rotation
    if values.is_empty() {
        // a broken rule needs a source fix, not an unquarantine
        if broken {
            println!(
                "Rule '{}' ({}) is {}: fix its source with 'thorctl rules update' instead",
                entity.name,
                entity.id,
                YaraRule::STATUS_BROKEN
            );
        } else {
            println!("Rule '{}' ({}) is not quarantined", entity.name, entity.id);
        }
        return Ok(());
    }
    // sort the values so output and deletes are deterministic
    values.sort_unstable();
    // scope the delete to every group this rule lives in
    let mut req = TagDeleteRequest::<Entity>::default().groups(entity.groups.clone());
    // delete every status value this rule carries
    for value in &values {
        req.add_ref(YaraRule::STATUS_TAG, value);
    }
    // delete this rules status tag to re-admit it to scanning
    thorium.entities.delete_tags(entity.id, &req).await?;
    // show which status values were cleared
    println!(
        "{} rule '{}' ({}) - deleted {}: {}",
        "Re-admitted".bright_green(),
        entity.name,
        entity.id,
        YaraRule::STATUS_TAG,
        values.join(", ")
    );
    // a broken status stays until the rule source is fixed
    if broken {
        println!(
            "The {} status was kept: fix this rule's source with 'thorctl rules update' to clear it",
            YaraRule::STATUS_BROKEN
        );
    }
    Ok(())
}

/// Set or clear a rules quarantine exemption (admins only)
///
/// # Arguments
///
/// * `thorium` - A Thorium client
/// * `cmd` - The exempt rule command to execute
async fn exempt(thorium: &Thorium, cmd: &ExemptRule) -> Result<(), Error> {
    // resolve this rule and make sure it is one
    let entity = get_rule(thorium, &cmd.rule).await?;
    // only yara rules can be exempted since quarantine is yara only
    if entity.kind != EntityKinds::YaraRule {
        return Err(Error::new(format!(
            "Rule '{}' ({}) is a {} rule; only yara rules can be exempted from quarantine",
            entity.name,
            entity.id,
            engine_name(&entity.kind)
        )));
    }
    // set or clear this rules exemption based on the clear flag
    if cmd.clear {
        // clear this rules exemption (admins only)
        err_not_admin!(
            thorium.entities.clear_quarantine_exemption(entity.id).await,
            "manage quarantine exemptions"
        );
        // show that this rules exemption was cleared
        println!(
            "{} quarantine exemption for rule '{}' ({})",
            "Cleared".bright_green(),
            entity.name,
            entity.id
        );
    } else {
        // exempt this rule from quarantine (admins only)
        err_not_admin!(
            thorium.entities.set_quarantine_exemption(entity.id).await,
            "manage quarantine exemptions"
        );
        // show that this rule is now exempt and re-admitted; two lines so the
        // uuid carrying line stays narrow
        println!(
            "{} rule '{}' ({}) from quarantine",
            "Exempted".bright_green(),
            entity.name,
            entity.id
        );
        println!("Any Noisy/Slow status was cleared");
    }
    Ok(())
}

/// Handle all rules commands
///
/// # Arguments
///
/// * `args` - The arguments passed to Thorctl
/// * `cmd` - The rules command to execute
pub async fn handle(args: &Args, cmd: &Rules) -> Result<(), Error> {
    // load our config and instance our client
    let (conf, thorium) = utils::get_client(args).await?;
    // warn about insecure connections if not set to skip
    if !conf.skip_insecure_warning.unwrap_or_default() {
        utils::warn_insecure_conf(&conf)?;
    }
    // check if we need to update
    if !args.skip_update && !conf.skip_update.unwrap_or_default() {
        ask_update(&thorium).await?;
    }
    // call the right rules handler
    match cmd {
        Rules::Create(cmd) => create(&thorium, cmd).await,
        Rules::Get(cmd) => get(&thorium, cmd).await,
        Rules::List(cmd) => list(&thorium, cmd).await,
        Rules::Update(cmd) => update(&thorium, cmd).await,
        Rules::Edit(cmd) => edit::edit(&conf, &thorium, cmd).await,
        Rules::Delete(cmd) => delete(&thorium, cmd).await,
        Rules::Import(cmd) => import::import(&thorium, cmd).await,
        Rules::Notifications(cmd) => notifications::handle(&thorium, cmd).await,
        Rules::Unquarantine(cmd) => unquarantine(&thorium, cmd).await,
        Rules::Exempt(cmd) => exempt(&thorium, cmd).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use std::collections::HashMap;
    use uuid::Uuid;

    /// Build a yara rule entity for render tests
    fn build_entity() -> Entity {
        // build the yara metadata with a multi line source
        let rule = YaraRule::new(
            "rule demo {\n    condition:\n        true\n}\n",
            YaraRuleAppliesTo::Samples,
        )
        .expect("failed to build yara rule");
        Entity {
            id: Uuid::nil(),
            name: "demo_rule".to_owned(),
            kind: EntityKinds::YaraRule,
            metadata: EntityMetadata::YaraRule(rule),
            description: Some("A demo rule".to_owned()),
            submitter: "mcarson".to_owned(),
            groups: vec!["demolab".to_owned()],
            tags: HashMap::default(),
            image: None,
            created: DateTime::<Utc>::from_timestamp(1_760_000_000, 0)
                .expect("timestamp was out of range"),
        }
    }

    /// The human readable summary must include fields and the verbatim source
    #[test]
    fn render_rule_plain() {
        // build a demo rule entity
        let entity = build_entity();
        // render this rule without ANSI styling
        let mut out = Vec::new();
        render_rule(&entity, &mut out, false).expect("failed to render rule");
        let rendered = String::from_utf8(out).expect("rendered rule was not utf-8");
        // the summary fields must all be present
        assert!(rendered.contains("demo_rule (demolab)"));
        assert!(rendered.contains("Engine: yara"));
        assert!(rendered.contains("Score: 0"));
        assert!(rendered.contains("Applies To: Samples"));
        assert!(rendered.contains("Submitter: mcarson"));
        assert!(rendered.contains("Description: A demo rule"));
        // the rule source must be rendered verbatim as real lines
        assert!(rendered.contains("rule demo {\n    condition:\n        true\n}\n"));
        // nothing may be ANSI styled without a terminal
        assert!(!rendered.contains('\u{1b}'));
        // untouched rules must not render any quarantine fields
        assert!(!rendered.contains("Quarantine"));
    }

    /// Quarantine status and exemption must render when present
    #[test]
    fn render_rule_quarantine_fields() {
        // build a demo rule entity that is quarantined and exempt
        let mut entity = build_entity();
        // tag this rule as noisy like the scanner does
        entity.tags.insert(
            YaraRule::STATUS_TAG.to_owned(),
            HashMap::from([(
                "Noisy".to_owned(),
                std::collections::HashSet::from(["demolab".to_owned()]),
            )]),
        );
        // set the exemption flag on this rules metadata
        if let EntityMetadata::YaraRule(rule) = &mut entity.metadata {
            rule.quarantine_exempt = true;
        }
        // render this rule without ANSI styling
        let mut out = Vec::new();
        render_rule(&entity, &mut out, false).expect("failed to render rule");
        let rendered = String::from_utf8(out).expect("rendered rule was not utf-8");
        // the quarantine fields must both be present
        assert!(rendered.contains("Quarantine: Noisy"));
        assert!(rendered.contains("Quarantine Exempt: true"));
        // tags must render one per line under a label so they never wrap
        assert!(rendered.contains("Tags:\n  YaraRuleStatus=Noisy\n"));
    }

    /// Rule rows must render engine names and narrow timestamps
    #[test]
    fn rule_row_from_entity() {
        // build a table row from a demo rule entity
        let row = RuleRow::from(&build_entity());
        // the row must carry the engine name and the entity's id
        assert_eq!(row.name, "demo_rule");
        assert_eq!(row.engine, "yara");
        assert_eq!(row.id, Uuid::nil().to_string());
        // timestamps must render at seconds precision with a Z suffix
        assert_eq!(row.created, "2025-10-09T08:53:20Z");
    }
}
