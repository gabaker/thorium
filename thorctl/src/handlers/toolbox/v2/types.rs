//! The toolbox v2 manifest types
//!
//! Every map is a `BTreeMap` so a serialized manifest has a stable order, and every struct refuses
//! unknown keys so a misspelled key is reported instead of silently ignored. Specs are kept as
//! JSON values because they may hold `ref:` strings where the API wants an id (see
//! [`super::refs`]); they are parsed into the API's own types once the references are resolved.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use thorium::models::{
    AgentConfig, EventTrigger, ImageRequest, ModelAuthLevel, ModelMatcher, NetworkPolicyRequest,
    PipelineRequest, ToolKind, ToolVersion, WorkflowLimits,
};
use uuid::Uuid;

use crate::handlers::toolbox::build::BaseImage;

/// The format version of the manifests this module reads and writes
pub const FORMAT_VERSION: u32 = 2;

/// The longest name a tool, toolset, agent, workflow, or pipeline can have in a manifest
pub const MAX_NAME_LEN: usize = 128;

/// Get the format version a manifest without a `format_version` key has
fn default_format_version() -> u32 {
    // a manifest that omits the key is read as the current format
    FORMAT_VERSION
}

/// Whether a bool is false (so a flag that is only written when set is skipped)
///
/// # Arguments
///
/// * `value` - The flag to check
// serde's `skip_serializing_if` passes a reference
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(value: &bool) -> bool {
    // flags are skipped while they hold their default
    !*value
}

/// Check that a name can stand for a tool, toolset, agent, workflow, or pipeline in a manifest
///
/// Names appear inside `ref:` strings and `group/name@label` displays, so they can't hold
/// whitespace, control characters, or any of `/ @ : ,`.
///
/// # Arguments
///
/// * `name` - The name to check
pub fn check_name(name: &str) -> Result<(), String> {
    // an empty name can't be referred to
    if name.is_empty() {
        return Err("the name is empty".to_owned());
    }
    // a very long name can't be a tool or pipeline name in Thorium
    if name.len() > MAX_NAME_LEN {
        return Err(format!("the name is longer than {MAX_NAME_LEN} characters"));
    }
    // find the first character that would break a reference or a display
    match name
        .chars()
        .find(|c| c.is_whitespace() || c.is_control() || matches!(c, '/' | '@' | ':' | ','))
    {
        Some(bad) => Err(format!("the name contains {bad:?}")),
        None => Ok(()),
    }
}

/// Check that a version label can name a tool version in a manifest
///
/// # Arguments
///
/// * `label` - The label to check
pub fn check_label(label: &str) -> Result<(), String> {
    // an empty label can't tell versions apart
    if label.is_empty() {
        return Err("the version label is empty".to_owned());
    }
    // labels appear after an `@` in references, so they can't hold the separators
    match label
        .chars()
        .find(|c| c.is_whitespace() || c.is_control() || matches!(c, '/' | '@' | ','))
    {
        Some(bad) => Err(format!("the version label contains {bad:?}")),
        None => Ok(()),
    }
}

/// The top level of a toolbox v2 manifest (`toolbox.json`)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolboxManifestV2 {
    /// The format version of this manifest (always 2)
    #[serde(default = "default_format_version")]
    pub format_version: u32,
    /// The name of this toolbox
    pub name: String,
    /// The registry the tools' images can be found at, when the toolbox declares one
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry: Option<String>,
    /// Every registry the toolbox's images are published to
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub registries: Vec<String>,
    /// The base path bundled images are pushed under on import
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_path_prefix: Option<String>,
    /// Whether the toolbox bundles container image tarballs next to its manifests
    #[serde(default, skip_serializing_if = "is_false")]
    pub bundled_images: bool,
    /// The groups the items use, with the descriptions to create them with
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub groups: BTreeMap<String, GroupManifest>,
    /// The network policies the tools name
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub network_policies: Vec<NetworkPolicyRequest>,
    /// The tools, keyed by name
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tools: BTreeMap<String, ToolManifest>,
    /// Image build contexts that have no tool of their own, keyed by name then label
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub builds: BTreeMap<String, BTreeMap<String, BuildManifest>>,
    /// The toolsets, keyed by name
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub toolsets: BTreeMap<String, ToolSetManifest>,
    /// The agents, keyed by name
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub agents: BTreeMap<String, AgentManifest>,
    /// The workflows, keyed by name
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub workflows: BTreeMap<String, WorkflowManifest>,
    /// The pipelines, keyed by name
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub pipelines: BTreeMap<String, PipelineManifestV2>,
}

impl ToolboxManifestV2 {
    /// Build an empty manifest with a name
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the toolbox
    pub fn new(name: impl Into<String>) -> Self {
        // an empty manifest in the current format
        ToolboxManifestV2 {
            format_version: FORMAT_VERSION,
            name: name.into(),
            registry: None,
            registries: Vec::new(),
            image_path_prefix: None,
            bundled_images: false,
            groups: BTreeMap::new(),
            network_policies: Vec::new(),
            tools: BTreeMap::new(),
            builds: BTreeMap::new(),
            toolsets: BTreeMap::new(),
            agents: BTreeMap::new(),
            workflows: BTreeMap::new(),
            pipelines: BTreeMap::new(),
        }
    }

    /// Count what this manifest holds
    #[must_use]
    pub fn counts(&self) -> Counts {
        // count each section; versions are counted across every tool or pipeline
        Counts {
            tools: self.tools.len(),
            tool_versions: self.tools.values().map(|tool| tool.versions.len()).sum(),
            builds: self.builds.len(),
            build_versions: self.builds.values().map(BTreeMap::len).sum(),
            toolsets: self.toolsets.len(),
            agents: self.agents.len(),
            workflows: self.workflows.len(),
            pipelines: self.pipelines.len(),
            pipeline_versions: self
                .pipelines
                .values()
                .map(|pipeline| pipeline.versions.len())
                .sum(),
            network_policies: self.network_policies.len(),
            groups: self.groups.len(),
        }
    }

    /// Get every group an item of this manifest names, sorted
    #[must_use]
    pub fn named_groups(&self) -> Vec<String> {
        // gather the groups of every kind of item into one sorted set
        let mut groups: std::collections::BTreeSet<String> = self.groups.keys().cloned().collect();
        groups.extend(self.tools.values().flat_map(|tool| tool.groups.clone()));
        groups.extend(
            self.toolsets
                .values()
                .flat_map(|toolset| toolset.groups.clone()),
        );
        groups.extend(self.agents.values().flat_map(|agent| agent.groups.clone()));
        groups.extend(
            self.workflows
                .values()
                .flat_map(|workflow| workflow.groups.clone()),
        );
        groups.extend(
            self.pipelines
                .values()
                .flat_map(|pipeline| pipeline.versions.values())
                .filter_map(|version| version.config.as_ref())
                .map(|config| config.group.clone()),
        );
        groups.into_iter().collect()
    }
}

/// How many of each kind of item a manifest holds
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counts {
    /// The number of tools
    pub tools: usize,
    /// The number of tool versions across every tool
    pub tool_versions: usize,
    /// The number of build-only image names
    pub builds: usize,
    /// The number of build-only image labels across every name
    pub build_versions: usize,
    /// The number of toolsets
    pub toolsets: usize,
    /// The number of agents
    pub agents: usize,
    /// The number of workflows
    pub workflows: usize,
    /// The number of pipelines
    pub pipelines: usize,
    /// The number of pipeline versions across every pipeline
    pub pipeline_versions: usize,
    /// The number of network policies
    pub network_policies: usize,
    /// The number of groups declared in the `groups` section
    pub groups: usize,
}

/// A group an item of the manifest uses
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GroupManifest {
    /// The description to create the group with
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// A tool and its versions
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolManifest {
    /// The kind of tool (derived from the first version's spec when unset)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ToolKind>,
    /// The groups the tool is in
    #[serde(default)]
    pub groups: Vec<String>,
    /// A description of the tool
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The user tags on the tool root, keyed by tag key
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tags: BTreeMap<String, Vec<String>>,
    /// The label that is published last, which becomes the tool's latest
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest: Option<String>,
    /// The harness environment variables, in the text `tools harness-env --set` takes (harness
    /// tools only)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub harness_env: Vec<String>,
    /// The model ids no run of this harness may resolve to (harness tools only)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled_models: Vec<String>,
    /// The versions of the tool, keyed by label
    #[serde(default)]
    pub versions: BTreeMap<String, ToolVersionManifest>,
}

impl ToolManifest {
    /// Get the kind of this tool: the declared kind, else the kind of the first spec found
    #[must_use]
    pub fn effective_kind(&self) -> Option<ToolKind> {
        // a declared kind wins; otherwise read the top level key of any inline spec
        self.kind.or_else(|| {
            self.versions
                .values()
                .filter_map(|version| version.spec.as_ref())
                .find_map(spec_kind)
        })
    }

    /// Get this tool's version labels in the order they are published
    ///
    /// `SemVer` labels come first in ascending order, then every other label in lexicographic
    /// order, then the manifest's `latest` label (if it names a version) last.
    #[must_use]
    pub fn publish_order(&self) -> Vec<String> {
        // split the labels by how they parse, leaving out the label that goes last
        let mut semver: Vec<(semver::Version, String)> = Vec::new();
        let mut custom: Vec<String> = Vec::new();
        for label in self.versions.keys() {
            // the latest label is published after everything else
            if self.latest.as_deref() == Some(label.as_str()) {
                continue;
            }
            match ToolVersion::from(label.as_str()) {
                ToolVersion::SemVer(version) => semver.push((version, label.clone())),
                ToolVersion::Custom(_) => custom.push(label.clone()),
            }
        }
        // order each group, then join them with the latest label at the end
        semver.sort();
        custom.sort();
        let mut order: Vec<String> = semver.into_iter().map(|(_, label)| label).collect();
        order.extend(custom);
        if let Some(latest) = &self.latest
            && self.versions.contains_key(latest)
        {
            order.push(latest.clone());
        }
        order
    }
}

/// Get the kind of tool a spec's top level key names
///
/// # Arguments
///
/// * `spec` - The JSON spec to read the kind of
#[must_use]
pub fn spec_kind(spec: &Value) -> Option<ToolKind> {
    // a spec is an object with exactly one key, the name of its kind
    let object = spec.as_object()?;
    match object.keys().next()?.as_str() {
        "Agentic" => Some(ToolKind::Agentic),
        "Image" => Some(ToolKind::Image),
        "Harness" => Some(ToolKind::Harness),
        _ => None,
    }
}

/// The image config a legacy toolbox entry carried, kept until its spec is built
///
/// This is never read from or written to a v2 document; it only lets a manifest loaded from a
/// legacy toolbox remember what its spec is to be built from.
#[derive(Debug, Clone, Default)]
pub struct LegacyImage {
    /// The inline (or fetched) image request
    pub config: Option<ImageRequest>,
}

/// One version of a tool
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolVersionManifest {
    /// The directory this version's `manifest.toml` lives in, relative to the toolbox file
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub dir: String,
    /// The tool spec as JSON (top level key `Agentic`, `Image`, or `Harness`)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<Value>,
    /// A URL or path the spec is read from instead of holding it inline
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec_from: Option<String>,
    /// How to build this version's container image
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<BuildManifest>,
    /// The native agentic tools this harness version provides (harness versions only)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provides: Vec<ToolRef>,
    /// URLs to fetch network policy definitions from
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub network_policies_from: Vec<String>,
    /// Network policies bundled with this version
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub network_policies: Vec<NetworkPolicyRequest>,
    /// The legacy image config this version is to be converted from, if it came from a legacy
    /// toolbox
    #[serde(skip)]
    pub legacy: Option<LegacyImage>,
}

impl ToolVersionManifest {
    /// Whether this version has (or can obtain) a spec
    #[must_use]
    pub fn has_spec_source(&self) -> bool {
        // an inline spec, a spec source, or a legacy config all give the version a spec
        self.spec.is_some() || self.spec_from.is_some() || self.legacy.is_some()
    }
}

/// How to build the container image of a tool version or a build-only context
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildManifest {
    /// The directory this entry's `manifest.toml` lives in, relative to the toolbox file
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub dir: String,
    /// The build context, relative to the toolbox file
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub build_path: String,
    /// Whether the image is built at all
    #[serde(default = "default_build_image")]
    pub build_image: bool,
    /// Every registry tag the image is built and pushed with
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub image_tags: Vec<String>,
    /// The base image settings, when the build substitutes one
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_image: Option<BaseImage>,
}

/// Get whether an image is built when a build entry doesn't say
fn default_build_image() -> bool {
    // entries build unless they opt out
    true
}

/// A reference to a tool, optionally pinned to one of its versions
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolRef {
    /// The name of the tool
    pub tool: String,
    /// The version label to pin (unset follows the tool's latest)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// A reference to a toolset by name
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolSetRef {
    /// The name of the toolset
    pub toolset: String,
}

/// A toolset
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolSetManifest {
    /// The groups the toolset is in
    #[serde(default)]
    pub groups: Vec<String>,
    /// A description of the toolset
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Whether only read-only tools are included
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only: Option<bool>,
    /// The `key=value` tag that adds every agentic tool carrying it as a member
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,
    /// Whether the event handler creates new instances automatically
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_instance: Option<bool>,
    /// The member tools (without a version they follow the tool's latest)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolRef>,
}

/// An agent
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentManifest {
    /// The groups the agent is in
    #[serde(default)]
    pub groups: Vec<String>,
    /// A description of the agent
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The agent's configuration
    #[serde(default)]
    pub config: AgentConfig,
    /// The levels that may supply model auth for the agent
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_auth: Vec<ModelAuthLevel>,
    /// The harness tool the agent runs in
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<ToolRef>,
    /// The tools the agent can call
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolRef>,
    /// The toolsets the agent can call the tools of
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub toolsets: Vec<ToolSetRef>,
    /// The names of the agents this agent can delegate to
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sub_agents: Vec<String>,
    /// Whether to add the agent's groups to any tool or toolset it names that lacks them
    #[serde(default, skip_serializing_if = "is_false")]
    pub extend_target_groups: bool,
}

/// A workflow
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowManifest {
    /// The groups the workflow is in
    #[serde(default)]
    pub groups: Vec<String>,
    /// A description of the workflow
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The names of the agents the workflow runs
    #[serde(default)]
    pub agents: Vec<String>,
    /// The name of the agent a run starts with
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_agent: Option<String>,
    /// The limits on a run of the workflow
    #[serde(default)]
    pub limits: WorkflowLimits,
    /// The models the workflow's agents default to
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub default_models: Vec<ModelMatcher>,
    /// The levels that may supply model auth for the workflow
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_auth: Vec<ModelAuthLevel>,
    /// Whether the event handler creates new instances automatically
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_instance: Option<bool>,
}

/// A pipeline and its versions
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PipelineManifestV2 {
    /// The versions of the pipeline, keyed by label
    #[serde(default)]
    pub versions: BTreeMap<String, PipelineVersionV2>,
}

/// One version of a pipeline
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PipelineVersionV2 {
    /// The directory this version's `manifest.toml` lives in, relative to the toolbox file
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub dir: String,
    /// A description of the pipeline for the toolbox, not for Thorium
    #[serde(default)]
    pub description: String,
    /// The pipeline's configuration, inline
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<PipelineConfig>,
    /// A URL or path the configuration is read from instead of holding it inline
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_from: Option<String>,
    /// The tool that backs each stage, keyed by stage name
    #[serde(default)]
    pub stage_tools: BTreeMap<String, StageTool>,
    /// Whether the pipeline kept a stage whose tool could not be converted
    #[serde(default, skip_serializing_if = "is_false")]
    pub incomplete: bool,
}

/// The tool behind a pipeline stage
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StageTool {
    /// The name of the tool
    pub tool: String,
    /// The version label to pin (unset follows the tool's latest)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// The configuration of a pipeline, without its stage tools
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PipelineConfig {
    /// The group the pipeline is in
    pub group: String,
    /// The name of the pipeline
    pub name: String,
    /// The order the stages run in (a list of stages, or a list of lists of stages)
    pub order: Value,
    /// The number of seconds the pipeline has to meet its SLA
    #[serde(default)]
    pub sla: Option<u64>,
    /// The triggers that run the pipeline
    #[serde(default)]
    pub triggers: BTreeMap<String, EventTrigger>,
    /// The pipeline's description in Thorium
    #[serde(default)]
    pub description: Option<String>,
}

impl PipelineConfig {
    /// Build the pipeline request for this config once its stage tools are resolved to ids
    ///
    /// # Arguments
    ///
    /// * `stage_tools` - The id of the tool (or tool instance) behind each stage
    #[must_use]
    pub fn to_request(&self, stage_tools: BTreeMap<String, Uuid>) -> PipelineRequest {
        // copy the config into the API's request, adding the resolved stage tools
        PipelineRequest {
            group: self.group.clone(),
            name: self.name.clone(),
            order: self.order.clone(),
            sla: self.sla,
            triggers: self.triggers.clone().into_iter().collect(),
            description: self.description.clone(),
            stage_tools,
        }
    }

    /// Get the stage names in this config's order, flattened
    ///
    /// Returns `None` when the order is neither a list of stages nor a list of lists of stages.
    #[must_use]
    pub fn stages(&self) -> Option<Vec<String>> {
        // the order is an array whose items are all strings or all arrays of strings
        let items = self.order.as_array()?;
        let mut stages = Vec::new();
        for item in items {
            match item {
                Value::String(stage) => stages.push(stage.clone()),
                Value::Array(group) => {
                    for stage in group {
                        stages.push(stage.as_str()?.to_owned());
                    }
                }
                _ => return None,
            }
        }
        Some(stages)
    }
}
