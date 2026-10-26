//! Offline checks of a toolbox manifest
//!
//! Nothing here talks to Thorium: every check reads the manifest (and what loading it reported)
//! and produces [`Issue`]s with stable codes. An error means an import would refuse the manifest;
//! a warning is worth a look (and fails `validate --strict`); a note is only information.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;
use thorium::Error;
use thorium::models::{ToolKind, ToolSpec};
use thorium::utils::protected_names;
use uuid::Uuid;

use super::graph;
use super::load::{ConvertReport, Loaded, SourceFailure, SourceFormat, load_any, resolve_sources};
use super::refs::{Ref, SlotValue, find_slots, resolve_slots};
use super::types::{
    BuildManifest, FORMAT_VERSION, ToolManifest, ToolRef, ToolboxManifestV2, check_label,
    check_name, spec_kind,
};
use crate::args::toolbox::ValidateToolbox;
use crate::handlers::progress::{Bar, BarKind};
use crate::utils::output::{self, OutputFormat, View, table_from};

/// Substrings (compared lowercase) that make an environment variable name look like it holds a
/// secret
const SECRET_NAME_PARTS: [&str; 11] = [
    "password",
    "passwd",
    "secret",
    "token",
    "api_key",
    "apikey",
    "api-key",
    "credential",
    "private_key",
    "private-key",
    "access_key",
];

/// How serious an issue is
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// An import would refuse the manifest
    Error,
    /// Worth a look; fails `validate --strict`
    Warning,
    /// Only information
    Note,
}

impl Severity {
    /// Get the name this severity is shown with
    #[must_use]
    pub fn as_str(self) -> &'static str {
        // each severity has a fixed name
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Note => "note",
        }
    }
}

/// One problem found in a manifest
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issue {
    /// How serious the issue is
    pub severity: Severity,
    /// The stable code of the issue, such as `missing_field:spec`
    pub code: String,
    /// The item the issue is about, such as `tool clamav@latest`
    pub item: String,
    /// What is wrong and how to fix it
    pub message: String,
}

impl Issue {
    /// Build an issue
    ///
    /// # Arguments
    ///
    /// * `severity` - How serious the issue is
    /// * `code` - The issue's code
    /// * `item` - The item the issue is about
    /// * `message` - What is wrong
    pub fn new(
        severity: Severity,
        code: impl Into<String>,
        item: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        // gather the parts
        Issue {
            severity,
            code: code.into(),
            item: item.into(),
            message: message.into(),
        }
    }
}

/// Collects issues while a manifest is checked
struct Checker<'a> {
    /// The manifest being checked
    manifest: &'a ToolboxManifestV2,
    /// The issues found so far
    issues: Vec<Issue>,
}

impl Checker<'_> {
    /// Record an error
    ///
    /// # Arguments
    ///
    /// * `code` - The issue's code
    /// * `item` - The item the issue is about
    /// * `message` - What is wrong
    fn error(&mut self, code: &str, item: &str, message: impl Into<String>) {
        // record the issue as an error
        self.issues
            .push(Issue::new(Severity::Error, code, item, message));
    }

    /// Record a warning
    ///
    /// # Arguments
    ///
    /// * `code` - The issue's code
    /// * `item` - The item the issue is about
    /// * `message` - What is wrong
    fn warning(&mut self, code: &str, item: &str, message: impl Into<String>) {
        // record the issue as a warning
        self.issues
            .push(Issue::new(Severity::Warning, code, item, message));
    }

    /// Record an error when a tool, toolset, workflow, or pipeline has a protected name
    ///
    /// The API refuses to create these names, so the manifest could never be imported.
    ///
    /// # Arguments
    ///
    /// * `item` - The item whose name to check
    /// * `name` - The item's name
    fn check_protected_name(&mut self, item: &str, name: &str) {
        // refuse a name the API would refuse, with the API's own line
        if let Err(refusal) = protected_names::check(name) {
            self.error("invalid_value:name", item, refusal);
        }
    }

    /// Check that a tool reference names something the manifest or the target group can supply
    ///
    /// # Arguments
    ///
    /// * `item` - The item holding the reference
    /// * `name` - The referenced tool's name
    /// * `label` - The version label the reference pins, if any
    /// * `code` - The error code to use when the manifest holds the tool but not that version
    fn check_tool_ref(&mut self, item: &str, name: &str, label: Option<&str>, code: &str) {
        match self.manifest.tools.get(name) {
            // a tool the manifest doesn't hold may already be in the target group
            None => self.warning(
                "unresolved_ref:tool",
                item,
                format!("refers to tool '{name}', which this manifest does not define; it must already exist in the target group"),
            ),
            // a pinned version must be one the manifest defines for that tool
            Some(tool) => {
                if let Some(label) = label
                    && !tool.versions.contains_key(label)
                {
                    self.error(
                        code,
                        item,
                        format!("refers to '{name}@{label}', but tool '{name}' defines no version '{label}'"),
                    );
                }
            }
        }
    }

    /// Check a list of tool references held by an item
    ///
    /// # Arguments
    ///
    /// * `item` - The item holding the references
    /// * `tools` - The references
    fn check_tool_refs(&mut self, item: &str, tools: &[ToolRef]) {
        // check each reference's name and pinned label
        for tool in tools {
            if let Err(err) = check_name(&tool.tool) {
                self.error("invalid_value:name", item, format!("tool reference: {err}"));
            }
            self.check_tool_ref(
                item,
                &tool.tool,
                tool.version.as_deref(),
                "missing_ref:tool",
            );
        }
    }

    /// Check that an item names at least one group, and note groups the `groups` section omits
    ///
    /// # Arguments
    ///
    /// * `item` - The item to check
    /// * `groups` - The groups the item names
    fn check_groups(&mut self, item: &str, groups: &[String]) {
        // an item with no group can't be created anywhere
        if groups.is_empty() {
            self.error("missing_field:groups", item, "names no group");
        }
        // when the manifest declares groups, an undeclared one is worth a note
        if !self.manifest.groups.is_empty() {
            for group in groups {
                if !self.manifest.groups.contains_key(group) {
                    self.issues.push(Issue::new(
                        Severity::Note,
                        "note:undeclared-group",
                        item,
                        format!("group '{group}' is not declared in the groups section"),
                    ));
                }
            }
        }
    }

    /// Check one tool and its versions
    ///
    /// # Arguments
    ///
    /// * `name` - The tool's name
    /// * `tool` - The tool
    fn check_tool(&mut self, name: &str, tool: &ToolManifest) {
        let item = format!("tool {name}");
        // the name must be usable in references
        if let Err(err) = check_name(name) {
            self.error("invalid_value:name", &item, err);
        }
        self.check_protected_name(&item, name);
        self.check_groups(&item, &tool.groups);
        // the latest label must name a version
        if let Some(latest) = &tool.latest
            && !tool.versions.contains_key(latest)
        {
            self.error(
                "invalid_value:latest",
                &item,
                format!("latest '{latest}' is not one of the tool's versions"),
            );
        }
        // a tool with no versions has nothing to publish
        if tool.versions.is_empty() {
            self.error("missing_field:versions", &item, "defines no versions");
        }
        // every version must agree on the kind the tool is
        let kinds: BTreeSet<ToolKind> = tool
            .versions
            .values()
            .filter_map(|version| version.spec.as_ref().and_then(spec_kind))
            .collect();
        if kinds.len() > 1 {
            self.error(
                "invalid_value:kind",
                &item,
                "its versions are of different kinds",
            );
        }
        if let (Some(declared), Some(actual)) = (tool.kind, kinds.iter().next())
            && declared != *actual
        {
            self.error(
                "invalid_value:kind",
                &item,
                format!("declares kind {declared} but its spec is {actual}"),
            );
        }
        // the harness-only fields need a harness
        let kind = tool.effective_kind();
        if kind.is_some_and(|kind| kind != ToolKind::Harness)
            && (!tool.harness_env.is_empty() || !tool.disabled_models.is_empty())
        {
            self.error(
                "invalid_value:harness_env",
                &item,
                "harness_env and disabled_models only apply to Harness tools",
            );
        }
        // check each version
        for label in tool.versions.keys() {
            let item = format!("tool {name}@{label}");
            self.check_version(&item, name, tool, label, kind);
        }
    }

    /// Check one version of a tool
    ///
    /// # Arguments
    ///
    /// * `item` - The version's display name
    /// * `name` - The tool's name
    /// * `tool` - The tool
    /// * `label` - The version's label
    /// * `kind` - The kind of the tool
    fn check_version(
        &mut self,
        item: &str,
        name: &str,
        tool: &ToolManifest,
        label: &str,
        kind: Option<ToolKind>,
    ) {
        let version = &tool.versions[label];
        // the label must be usable in references
        if let Err(err) = check_label(label) {
            self.error("invalid_value:version", item, err);
        }
        // a version needs a spec from exactly one place
        if !version.has_spec_source() {
            self.error("missing_field:spec", item, "has neither spec nor spec_from");
        }
        if version.spec.is_some() && version.spec_from.is_some() {
            self.error(
                "invalid_value:spec_from",
                item,
                "has both spec and spec_from; use exactly one",
            );
        }
        if let Some(spec) = &version.spec {
            self.check_spec(item, spec);
        }
        // a build with nothing to build from is a mistake
        if let Some(build) = &version.build {
            self.check_build(item, build);
        }
        // only harness versions provide native tools
        if !version.provides.is_empty() {
            if kind == Some(ToolKind::Harness) {
                self.check_provides(item, name, tool, &version.provides);
            } else {
                self.error(
                    "invalid_value:provides",
                    item,
                    "only a Harness tool can provide tools",
                );
            }
        }
    }

    /// Check what a harness version provides
    ///
    /// # Arguments
    ///
    /// * `item` - The version's display name
    /// * `name` - The harness tool's name
    /// * `harness` - The harness tool
    /// * `provides` - The tools the version provides
    fn check_provides(
        &mut self,
        item: &str,
        name: &str,
        harness: &ToolManifest,
        provides: &[ToolRef],
    ) {
        for provided in provides {
            // a provided tool must be defined here so its groups can be checked
            let Some(tool) = self.manifest.tools.get(&provided.tool) else {
                self.error(
                    "missing_ref:provides",
                    item,
                    format!(
                        "provides tool '{}', which this manifest does not define",
                        provided.tool
                    ),
                );
                continue;
            };
            // a pinned version must exist
            if let Some(label) = &provided.version
                && !tool.versions.contains_key(label)
            {
                self.error(
                    "missing_ref:provides",
                    item,
                    format!(
                        "provides '{}@{label}', but that version is not defined",
                        provided.tool
                    ),
                );
            }
            // a harness can only provide native agentic tools
            if tool
                .effective_kind()
                .is_some_and(|kind| kind != ToolKind::Agentic)
            {
                self.error(
                    "invalid_value:provides.kind",
                    item,
                    format!("provides '{}', which is not an Agentic tool", provided.tool),
                );
            }
            // the provided tool must be in every group the harness is in
            if let Some(missing) = harness
                .groups
                .iter()
                .find(|group| !tool.groups.contains(group))
            {
                self.error(
                    "invalid_value:provides.groups",
                    item,
                    format!(
                        "provided tool '{}' is not in group '{missing}', which harness '{name}' is in",
                        provided.tool
                    ),
                );
            }
        }
    }

    /// Check a build entry
    ///
    /// # Arguments
    ///
    /// * `item` - The item the build belongs to
    /// * `build` - The build entry
    fn check_build(&mut self, item: &str, build: &BuildManifest) {
        // an image that is built needs a context and tags
        if build.build_image && build.build_path.is_empty() {
            self.error(
                "missing_field:build.build_path",
                item,
                "builds its image but names no build_path",
            );
        }
        if build.build_image && build.image_tags.is_empty() {
            self.warning(
                "missing_field:build.image_tags",
                item,
                "builds its image but lists no image_tags to build it as",
            );
        }
    }

    /// Check an inline tool spec: its references, how it parses, and the env names it sets
    ///
    /// # Arguments
    ///
    /// * `item` - The version's display name
    /// * `spec` - The spec JSON
    fn check_spec(&mut self, item: &str, spec: &Value) {
        // read what every id slot holds
        let (found, problems) = find_slots(spec);
        for problem in problems {
            self.error(
                "invalid_value:spec_ref",
                item,
                format!("{}: {}", problem.path, problem.message),
            );
        }
        for slot in found {
            match slot.value {
                // a reference must land somewhere
                SlotValue::Ref(Ref::Tool { name, label }) => {
                    self.check_tool_ref(item, &name, label.as_deref(), "missing_ref:tool");
                }
                SlotValue::Ref(_) => {}
                // a literal id ties the manifest to one Thorium
                SlotValue::Uuid(_) => self.warning(
                    "warning:uuid-literal",
                    item,
                    format!("{} holds a literal id, which only exists in one Thorium; use a ref:tool/NAME reference", slot.path),
                ),
            }
        }
        // parse the spec the way the API does, with every reference swapped for an id
        let mut parsable = spec.clone();
        resolve_slots(&mut parsable, |_, _| Ok(Uuid::nil()));
        match serde_json::to_string(&parsable) {
            Ok(text) => {
                if let Err(err) = ToolSpec::parse_request(&text) {
                    self.error("invalid_value:spec", item, err);
                }
            }
            Err(err) => self.error("invalid_value:spec", item, err.to_string()),
        }
        // warn about environment variable names that look like they hold secrets
        let mut names = Vec::new();
        secret_env_names(spec, &mut names);
        for env in names {
            self.warning(
                "warning:secret-env",
                item,
                format!("environment variable {env} looks like a secret; spec env values are readable by group members"),
            );
        }
    }

    /// Check every tool and build-only context
    fn check_tools_and_builds(&mut self) {
        // check each tool
        for (name, tool) in &self.manifest.tools {
            self.check_tool(name, tool);
        }
        // check each build-only context
        for (name, builds) in &self.manifest.builds {
            if let Err(err) = check_name(name) {
                self.error("invalid_value:name", &format!("build {name}"), err);
            }
            for (label, build) in builds {
                let item = format!("build {name}@{label}");
                if let Err(err) = check_label(label) {
                    self.error("invalid_value:version", &item, err);
                }
                self.check_build(&item, build);
            }
        }
    }

    /// Check every toolset, agent, and workflow
    fn check_entities(&mut self) {
        // toolsets
        for (name, toolset) in &self.manifest.toolsets {
            let item = format!("toolset {name}");
            if let Err(err) = check_name(name) {
                self.error("invalid_value:name", &item, err);
            }
            self.check_protected_name(&item, name);
            self.check_groups(&item, &toolset.groups);
            // a selector is a tag as `key=value`
            if let Some(selector) = &toolset.selector
                && !selector
                    .split_once('=')
                    .is_some_and(|(key, value)| !key.is_empty() && !value.is_empty())
            {
                self.error(
                    "invalid_value:selector",
                    &item,
                    format!("selector '{selector}' is not of the form key=value"),
                );
            }
            self.check_tool_refs(&item, &toolset.tools);
        }
        // agents
        for (name, agent) in &self.manifest.agents {
            let item = format!("agent {name}");
            if let Err(err) = check_name(name) {
                self.error("invalid_value:name", &item, err);
            }
            self.check_groups(&item, &agent.groups);
            self.check_tool_refs(&item, &agent.tools);
            // a harness reference must be a harness tool when the manifest defines it
            if let Some(harness) = &agent.harness {
                self.check_tool_refs(&item, std::slice::from_ref(harness));
                if let Some(tool) = self.manifest.tools.get(&harness.tool)
                    && tool
                        .effective_kind()
                        .is_some_and(|kind| kind != ToolKind::Harness)
                {
                    self.error(
                        "invalid_value:harness",
                        &item,
                        format!("harness '{}' is not a Harness tool", harness.tool),
                    );
                }
            }
            for toolset in &agent.toolsets {
                if !self.manifest.toolsets.contains_key(&toolset.toolset) {
                    self.warning(
                        "unresolved_ref:toolset",
                        &item,
                        format!("refers to toolset '{}', which this manifest does not define; it must already exist in the target group", toolset.toolset),
                    );
                }
            }
            for sub_agent in &agent.sub_agents {
                if !self.manifest.agents.contains_key(sub_agent) {
                    self.warning(
                        "unresolved_ref:agent",
                        &item,
                        format!("refers to agent '{sub_agent}', which this manifest does not define; it must already exist in the target group"),
                    );
                }
            }
        }
        // workflows
        for (name, workflow) in &self.manifest.workflows {
            let item = format!("workflow {name}");
            if let Err(err) = check_name(name) {
                self.error("invalid_value:name", &item, err);
            }
            self.check_protected_name(&item, name);
            self.check_groups(&item, &workflow.groups);
            if workflow.agents.is_empty() {
                self.error("missing_field:agents", &item, "runs no agents");
            }
            for agent in &workflow.agents {
                if !self.manifest.agents.contains_key(agent) {
                    self.warning(
                        "unresolved_ref:agent",
                        &item,
                        format!("refers to agent '{agent}', which this manifest does not define; it must already exist in the target group"),
                    );
                }
            }
            if let Some(entry) = &workflow.entry_agent
                && !workflow.agents.contains(entry)
            {
                self.error(
                    "invalid_value:entry_agent",
                    &item,
                    format!("entry agent '{entry}' is not one of the workflow's agents"),
                );
            }
        }
    }

    /// Check every pipeline
    fn check_pipelines(&mut self) {
        for (name, pipeline) in &self.manifest.pipelines {
            if let Err(err) = check_name(name) {
                self.error("invalid_value:name", &format!("pipeline {name}"), err);
            }
            self.check_protected_name(&format!("pipeline {name}"), name);
            for (label, version) in &pipeline.versions {
                let item = format!("pipeline {name}@{label}");
                if let Err(err) = check_label(label) {
                    self.error("invalid_value:version", &item, err);
                }
                // a pipeline version needs its config
                let Some(config) = &version.config else {
                    if version.config_from.is_none() {
                        self.error(
                            "missing_field:config",
                            &item,
                            "has neither config nor config_from",
                        );
                    }
                    continue;
                };
                if config.group.is_empty() {
                    self.error("missing_field:groups", &item, "its config names no group");
                }
                if version.incomplete {
                    self.warning(
                        "invalid_value:stages.*",
                        &item,
                        "is flagged incomplete: a stage has no converted tool",
                    );
                }
                // the order must be stages, or groups of stages
                let Some(stages) = config.stages().filter(|stages| !stages.is_empty()) else {
                    self.error(
                        "invalid_value:order",
                        &item,
                        "its order must be a non-empty list of stages or of lists of stages",
                    );
                    continue;
                };
                // every stage needs a tool behind it
                for stage in &stages {
                    if !version.stage_tools.contains_key(stage) {
                        self.error(
                            "missing_field:stage_tools.*",
                            &item,
                            format!("stage '{stage}' has no entry in stage_tools"),
                        );
                    }
                }
                for (stage, stage_tool) in &version.stage_tools {
                    if !stages.contains(stage) {
                        self.warning(
                            "invalid_value:stage_tools.*",
                            &item,
                            format!("stage_tools names stage '{stage}', which is not in the order"),
                        );
                    }
                    self.check_tool_ref(
                        &item,
                        &stage_tool.tool,
                        stage_tool.version.as_deref(),
                        "missing_ref:tool",
                    );
                }
            }
        }
    }

    /// Check the dependency graph for cycles
    fn check_cycles(&mut self) {
        for cycle in graph::cycles(&graph::dependencies(self.manifest)) {
            // a loop of agents delegating to each other is only worth a warning
            let agents_only = cycle
                .iter()
                .all(|item| item.kind == super::plan_types::ItemKind::Agent);
            let path = cycle
                .iter()
                .chain(cycle.first())
                .map(|item| item.name.clone())
                .collect::<Vec<_>>()
                .join(" -> ");
            let kind = cycle[0].kind;
            let item = cycle[0].to_string();
            let message = format!("depends on itself through {path}");
            if agents_only {
                self.warning(&format!("cycle:{kind}"), &item, message);
            } else {
                self.error(&format!("cycle:{kind}"), &item, message);
            }
        }
    }
}

/// Find the names of the environment variables a spec sets that look like secrets
///
/// # Arguments
///
/// * `value` - The spec (or part of one) to scan
/// * `names` - Where the names found are collected
fn secret_env_names(value: &Value, names: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                // an `env` object maps variable names to values; only the names are read
                if key == "env"
                    && let Value::Object(vars) = child
                {
                    for var in vars.keys() {
                        let lower = var.to_lowercase();
                        if SECRET_NAME_PARTS.iter().any(|part| lower.contains(part)) {
                            names.push(var.clone());
                        }
                    }
                }
                secret_env_names(child, names);
            }
        }
        Value::Array(items) => {
            for item in items {
                secret_env_names(item, names);
            }
        }
        _ => {}
    }
}

/// Check a manifest and everything loading it reported
///
/// # Arguments
///
/// * `manifest` - The manifest to check
/// * `report` - What loading the legacy part of the manifest converted and skipped
/// * `sources` - The sources (specs, configs, policies) that could not be read
#[must_use]
pub fn validate_manifest(
    manifest: &ToolboxManifestV2,
    report: &ConvertReport,
    sources: &[SourceFailure],
) -> Vec<Issue> {
    let mut checker = Checker {
        manifest,
        issues: Vec::new(),
    };
    // a manifest built in memory could carry a format this thorctl doesn't write
    if manifest.format_version != FORMAT_VERSION {
        checker.error(
            "invalid_value:format_version",
            &manifest.name,
            format!(
                "format_version {} is not {FORMAT_VERSION}",
                manifest.format_version
            ),
        );
    }
    // what loading skipped, warned about, and collided on
    for skipped in &report.skipped {
        // say what the skip means for a toolbox rather than for a stored image
        let code = skipped.reason.code();
        let meaning = match code.as_str() {
            "missing_field:image" => {
                "names no container image, so it was not converted into a tool"
            }
            "invalid_value:stages.*" => {
                "runs a stage whose tool was not converted, so the pipeline was left out"
            }
            "missing_field:config" => "has no config, so it was not converted",
            "invalid_value:name" | "invalid_value:version" => {
                "has a name or version label that can't be used, so it was not converted"
            }
            _ => "was not converted",
        };
        checker.warning(&code, &skipped.item, meaning);
    }
    for note in &report.notes {
        checker.issues.push(Issue::new(
            Severity::Note,
            "note:conversion",
            &note.item,
            &note.note,
        ));
    }
    for warning in &report.warnings {
        checker.warning("warning:conversion", &manifest.name, warning);
    }
    for name in &report.collisions {
        checker.error(
            &format!("collision:{name}"),
            name,
            "is defined more than once, in more than one section or with more than one group",
        );
    }
    // sources that could not be read
    for source in sources {
        checker.error(
            &format!("invalid_value:{}", source.field),
            &source.item,
            &source.message,
        );
    }
    // the manifest's own contents
    checker.check_tools_and_builds();
    checker.check_entities();
    checker.check_pipelines();
    checker.check_cycles();
    checker.issues
}

/// The result of validating a manifest, printable in every output format
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationReport {
    /// The toolbox's name
    pub name: String,
    /// The format the manifest was read in
    pub source: SourceFormat,
    /// How many of each kind of item the manifest holds
    pub counts: super::types::Counts,
    /// Whether the manifest has no errors (and, when strict, no warnings or skipped items)
    pub valid: bool,
    /// The number of errors
    pub errors: usize,
    /// The number of warnings
    pub warnings: usize,
    /// Every issue found, errors first
    pub issues: Vec<Issue>,
    /// What reading the legacy part of the manifest converted
    pub converted: ConvertReport,
}

impl ValidationReport {
    /// Build the report for a loaded manifest
    ///
    /// # Arguments
    ///
    /// * `loaded` - The loaded manifest
    /// * `sources` - The sources that could not be read
    /// * `strict` - Whether warnings make the manifest invalid
    #[must_use]
    pub fn new(loaded: &Loaded, sources: &[SourceFailure], strict: bool) -> Self {
        // find the issues, most serious first and otherwise in the order found
        let mut issues = validate_manifest(&loaded.manifest, &loaded.report, sources);
        issues.sort_by_key(|issue| issue.severity);
        let errors = issues
            .iter()
            .filter(|issue| issue.severity == Severity::Error)
            .count();
        let warnings = issues
            .iter()
            .filter(|issue| issue.severity == Severity::Warning)
            .count();
        ValidationReport {
            name: loaded.manifest.name.clone(),
            source: loaded.source,
            counts: loaded.manifest.counts(),
            valid: errors == 0 && (!strict || warnings == 0),
            errors,
            warnings,
            issues,
            converted: loaded.report.clone(),
        }
    }
}

impl View for ValidationReport {
    /// Render the issues as a table
    fn table(&self) -> String {
        // one row per issue
        let rows = self
            .issues
            .iter()
            .map(|issue| {
                vec![
                    issue.severity.as_str().to_owned(),
                    issue.code.clone(),
                    issue.item.clone(),
                    issue.message.clone(),
                ]
            })
            .collect();
        table_from(&["SEVERITY", "CODE", "ITEM", "MESSAGE"], rows)
    }

    /// Render the summary and every issue as text
    ///
    /// # Arguments
    ///
    /// * `_ansi` - Whether to style the text
    fn human(&self, _ansi: bool) -> String {
        let counts = &self.counts;
        // the header names the toolbox and how it was read, then counts what it holds
        let mut lines = vec![
            format!("Toolbox '{}' ({} format)", self.name, self.source.as_str()),
            format!(
                "  tools: {} ({} versions), builds: {} ({} labels), toolsets: {}, agents: {}, workflows: {}, pipelines: {}, network policies: {}",
                counts.tools,
                counts.tool_versions,
                counts.builds,
                counts.build_versions,
                counts.toolsets,
                counts.agents,
                counts.workflows,
                counts.pipelines,
                counts.network_policies,
            ),
        ];
        // one line per issue
        lines.extend(self.issues.iter().map(|issue| {
            format!(
                "{}[{}] {}: {}",
                issue.severity.as_str(),
                issue.code,
                issue.item,
                issue.message
            )
        }));
        // the verdict
        let verdict = if self.valid { "valid" } else { "invalid" };
        lines.push(format!(
            "Result: {verdict} ({} errors, {} warnings)",
            self.errors, self.warnings
        ));
        lines.join("\n")
    }
}

/// Check a toolbox manifest and print the result
///
/// Returns whether the manifest is valid, so the caller can exit non-zero when it is not.
///
/// # Arguments
///
/// * `cmd` - The `toolbox validate` command
pub async fn validate(cmd: &ValidateToolbox) -> Result<bool, Error> {
    // read the manifest without drawing a progress bar, since the report goes to stdout
    let bar = Bar::new_or_quiet(
        "toolbox validate",
        "Reading manifest...",
        BarKind::UnboundIO,
        true,
    );
    let mut loaded = load_any(&cmd.manifest, &bar).await?;
    // read the specs and configs the manifest points at, keeping what can't be read as issues
    let sources = resolve_sources(&mut loaded.manifest, loaded.base_dir.as_deref()).await;
    // check everything and print the report
    let report = ValidationReport::new(&loaded, &sources, cmd.strict);
    output::print(&report, cmd.format.format.unwrap_or(OutputFormat::Human))?;
    Ok(report.valid)
}
