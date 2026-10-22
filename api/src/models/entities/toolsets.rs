//! Contains models for toolsets and toolset instances
//!
//! A [`ToolSetEntity`] is an editable, named group of agentic tools that agents attach as one
//! unit. Its members are tools it includes explicitly and the tools a tag selector matches in
//! every one of its groups. A [`ToolSetInstanceEntity`] is a constant snapshot of a toolset's
//! resolved members, with a content digest over its settings and its members' spec hashes.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

use crate::models::Entity;

#[cfg(feature = "client")]
pub(crate) mod resolve;
#[cfg(test)]
mod tests;

#[cfg(feature = "client")]
pub use resolve::{
    ResolvedToolSet, ToolSetBuild, build_tool_set_instance, newest_tool_set_instance,
    resolve_tool_set,
};

/// The tag naming the toolset an instance belongs to
pub const TOOL_SET_ID_TAG: &str = "ToolSetId";

/// The tag naming each tool a toolset instance's members follow at latest
pub const TOOL_SET_FOLLOWS_TOOL_TAG: &str = "ToolSetFollowsTool";

/// The tag naming the selector a toolset instance was built with, as `key=value`
pub const TOOL_SET_SELECTOR_TAG: &str = "ToolSetSelector";

/// The default for the most members one toolset instance may have
pub const DEFAULT_MAX_TOOLS_PER_TOOLSET: usize = 64;

/// How many tools a toolset may consider for each member it may have
///
/// Explicit edges and the tools its selector's tag is on past this many are refused unread.
const CANDIDATES_PER_MEMBER: usize = 4;

/// The limits on a toolset's members
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSetLimits {
    /// The most members one toolset instance may have
    pub max_tools_per_toolset: usize,
}

impl Default for ToolSetLimits {
    /// Build the default toolset limits
    fn default() -> Self {
        ToolSetLimits {
            max_tools_per_toolset: DEFAULT_MAX_TOOLS_PER_TOOLSET,
        }
    }
}

impl From<&crate::conf::Events> for ToolSetLimits {
    /// Get the toolset limits an events config sets
    ///
    /// # Arguments
    ///
    /// * `events` - The events config
    fn from(events: &crate::conf::Events) -> Self {
        ToolSetLimits {
            max_tools_per_toolset: events.max_tools_per_toolset,
        }
    }
}

impl ToolSetLimits {
    /// Get the most tools a toolset may consider: its explicit edges, and the tools its
    /// selector's tag is on in any one of its groups
    #[must_use]
    pub fn max_candidates(&self) -> usize {
        // a few candidates for each member it may keep
        self.max_tools_per_toolset
            .saturating_mul(CANDIDATES_PER_MEMBER)
    }
}

/// The default for settings that are on unless turned off
const fn default_true() -> bool {
    true
}

/// The editable root definition of a named group of agentic tools that agents can attach
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ToolSetEntity {
    /// Whether only read-only tools (whose instance declares no effects) are included
    #[serde(default)]
    pub read_only: bool,
    /// The tag that adds every agentic tool carrying it in every one of this toolset's groups as
    /// a member
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<ToolSetSelector>,
    /// Whether the event handler should create new instances automatically on changes
    #[serde(default = "default_true")]
    pub auto_instance: bool,
    /// The explicitly included tools (Tool = latest, `ToolInstance` = pinned; populated from
    /// associations)
    #[serde(default)]
    pub tools: Vec<Entity>,
    /// When this toolset was last modified (any edit, including its `Includes` edges)
    pub modified: DateTime<Utc>,
    /// The newest ready instance of this toolset (populated on get, not stored)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_instance: Option<Uuid>,
    /// The highest revision any instance of this toolset was created at, kept so a deleted
    /// instance's revision is never given out again (0 before the first instance, and in
    /// toolsets stored before it was kept, whose next revision follows their newest instance)
    #[serde(default)]
    pub highest_revision: u64,
}

impl ToolSetEntity {
    /// Clear the fields that are filled in when the toolset is read
    pub fn clear_populated(&mut self) {
        // drop the included tools and the latest instance
        self.tools.clear();
        self.latest_instance = None;
    }
}

/// A tag key and value that selects the tools in a toolset
#[derive(Debug, Clone, Default, Serialize, Deserialize, Hash, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(
    any(feature = "api", feature = "client"),
    derive(thorium_derive::DigestValue)
)]
pub struct ToolSetSelector {
    /// The tag key a tool must carry in every one of the toolset's groups
    pub key: String,
    /// The tag value a tool must carry for that key in every one of the toolset's groups
    pub value: String,
}

/// The tools a selector matches, split by whether they carry its tag in every toolset group
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SelectorMatches {
    /// The tools tagged in every one of the toolset's groups, in id order
    pub matched: Vec<Uuid>,
    /// The tools tagged in some of the toolset's groups, with the groups they lack, in id order
    pub partial: Vec<(Uuid, Vec<String>)>,
}

impl ToolSetSelector {
    /// Build a selector for a tag key and value
    ///
    /// # Arguments
    ///
    /// * `key` - The tag key
    /// * `value` - The tag value
    pub fn new(key: impl Into<String>, value: impl Into<String>) -> Self {
        ToolSetSelector {
            key: key.into(),
            value: value.into(),
        }
    }

    /// Split the tools carrying a selector's tag by whether they carry it in every toolset group
    ///
    /// Tags are stored per group, so a tool is a match only when it carries the tag in every
    /// one of the toolset's groups. A tool tagged only in groups the toolset isn't in isn't
    /// considered at all.
    ///
    /// # Arguments
    ///
    /// * `groups` - The toolset's groups
    /// * `tagged` - Each tool carrying the tag and the groups it carries it in
    #[must_use]
    pub fn partition(
        groups: &[String],
        tagged: &BTreeMap<Uuid, BTreeSet<String>>,
    ) -> SelectorMatches {
        // check each tagged tool against every toolset group
        let mut matches = SelectorMatches::default();
        for (tool, tool_groups) in tagged {
            // find the toolset groups this tool isn't tagged in
            let missing = groups
                .iter()
                .filter(|group| !tool_groups.contains(*group))
                .cloned()
                .collect::<Vec<String>>();
            // a tool tagged in none of the toolset's groups isn't a candidate
            if missing.len() == groups.len() {
                continue;
            }
            // a tool tagged in every group matches, and one tagged in some is partial
            if missing.is_empty() {
                matches.matched.push(*tool);
            } else {
                matches.partial.push((*tool, missing));
            }
        }
        matches
    }
}

impl std::fmt::Display for ToolSetSelector {
    /// Write a selector as `key=value`
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}={}", self.key, self.value)
    }
}

impl std::str::FromStr for ToolSetSelector {
    /// A selector missing its key, its value, or the `=` between them fails with a message saying
    /// why
    type Err = String;

    /// Parse a selector from `key=value`, splitting at the first `=`
    ///
    /// # Arguments
    ///
    /// * `raw` - The selector to parse
    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        // split the key from the value at the first equals sign
        let Some((key, value)) = raw.split_once('=') else {
            return Err(format!("selector `{raw}` must be written as key=value"));
        };
        // both parts must be set
        if key.is_empty() || value.is_empty() {
            return Err(format!(
                "selector `{raw}` needs both a key and a value (key=value)"
            ));
        }
        Ok(ToolSetSelector::new(key, value))
    }
}

/// A constant, fully resolved snapshot of a toolset's members
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ToolSetInstanceEntity {
    /// The ID of the toolset this is an instance of
    pub tool_set: Uuid,
    /// The groups the toolset was in at instancing time
    pub tool_set_groups: Vec<String>,
    /// The monotonically increasing revision of this instance within its toolset
    pub revision: u64,
    /// The ID of the instance this instance supersedes
    pub previous: Option<Uuid>,
    /// Whether this instance is valid and can be attached
    pub status: ToolSetInstanceStatus,
    /// Non fatal issues found while instancing (e.g. members excluded by the read only filter)
    #[serde(default)]
    pub warnings: Vec<String>,
    /// What caused this instance to be created
    pub trigger: ToolSetInstanceTrigger,
    /// The user whose change or request caused this instance (not in the digest)
    #[serde(default)]
    pub triggered_by: String,
    /// Whether the read only filter was on at instancing time
    pub read_only: bool,
    /// The toolset's tag selector at instancing time
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<ToolSetSelector>,
    /// Every tool instance this toolset gives to an agent
    pub members: Vec<ToolSetMember>,
    /// Every tool that was considered but left out, and why
    #[serde(default)]
    pub excluded: Vec<ToolSetExclusion>,
    /// The content digest of this toolset instance
    pub digest: String,
}

impl ToolSetInstanceEntity {
    /// Sort the members by tool and tool instance and the exclusions by tool
    ///
    /// Equal instances then serialize identically whatever order their graph was read in.
    pub fn sort_lists(&mut self) {
        // members order by their derived order and exclusions by their tool
        self.members.sort();
        self.excluded.sort();
    }

    /// Whether this instance resolved cleanly and can be attached
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.status == ToolSetInstanceStatus::Ready
    }

    /// Build the name a toolset instance gets: `{toolset name}#r{revision}`
    ///
    /// # Arguments
    ///
    /// * `tool_set` - The name of the toolset
    /// * `revision` - The instance's revision
    #[must_use]
    pub fn name_for(tool_set: &str, revision: u64) -> String {
        format!("{tool_set}#r{revision}")
    }

    /// Compute this instance's content digest
    ///
    /// The digest covers `read_only`, the selector, and each member's spec hash and inclusion,
    /// whatever order the members are listed in.
    #[cfg(any(feature = "api", feature = "client"))]
    pub fn compute_digest(&self) -> Result<String, serde_json::Error> {
        // digest under the current scheme
        self.compute_digest_with_scheme(super::digests::ThorDigest::SCHEME)
    }

    /// Compute this instance's content digest under a scheme prefix
    ///
    /// # Arguments
    ///
    /// * `scheme` - The scheme prefix to use
    #[cfg(any(feature = "api", feature = "client"))]
    pub(crate) fn compute_digest_with_scheme(
        &self,
        scheme: &str,
    ) -> Result<String, serde_json::Error> {
        // hash the settings and the members' hashes and inclusions
        let members = self
            .members
            .iter()
            .map(|member| (member.spec_hash.as_str(), member.inclusion))
            .collect::<Vec<(&str, ToolSetInclusion)>>();
        super::digests::ThorDigest::toolset_instance_with_scheme(
            scheme,
            self.read_only,
            self.selector.as_ref(),
            &members,
        )
    }
}

/// A tool instance as it was resolved into a toolset instance
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ToolSetMember {
    /// The ID of the tool (root) entity
    pub tool: Uuid,
    /// The ID of the exact tool instance used
    pub tool_instance: Uuid,
    /// The name of the tool
    pub name: String,
    /// The groups the tool instance was in at instancing time
    pub groups: Vec<String>,
    /// The version label of the tool instance
    pub version: String,
    /// The content hash of the tool instance
    pub spec_hash: String,
    /// The function name a model calls this tool by
    pub function_name: String,
    /// How this tool became a member
    pub inclusion: ToolSetInclusion,
}

/// How a tool became a member of a toolset instance
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum ToolSetInclusion {
    /// An `Includes` edge to the Tool, resolved to its latest instance
    ExplicitLatest,
    /// An `Includes` edge to this exact Tool Instance
    ExplicitPinned,
    /// The tool carries the toolset's selector tag, resolved to its latest instance
    Selector,
}

impl ToolSetInclusion {
    /// Get the name of this inclusion
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ToolSetInclusion::ExplicitLatest => "ExplicitLatest",
            ToolSetInclusion::ExplicitPinned => "ExplicitPinned",
            ToolSetInclusion::Selector => "Selector",
        }
    }

    /// Whether a member included this way follows its tool's latest instance
    #[must_use]
    pub fn follows_latest(self) -> bool {
        // only an explicit pin stays on one instance
        self != ToolSetInclusion::ExplicitPinned
    }
}

/// A tool that was considered for a toolset instance but left out
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ToolSetExclusion {
    /// The ID of the tool (root) entity
    pub tool: Uuid,
    /// The ID of the tool instance that was considered, if one was resolved
    pub tool_instance: Option<Uuid>,
    /// The name of the tool
    pub name: String,
    /// How this tool would have become a member
    pub inclusion: ToolSetInclusion,
    /// Why this tool was left out
    pub reason: ToolSetExclusionReason,
    /// The toolset groups the selector's tag is missing from (only for `NotTaggedInAllGroups`)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_groups: Vec<String>,
}

impl ToolSetExclusion {
    /// Describe why this tool was left out
    #[must_use]
    pub fn describe(&self) -> String {
        match self.reason {
            ToolSetExclusionReason::NotReadOnly => {
                "declares effects, and the toolset is read only".to_owned()
            }
            ToolSetExclusionReason::NotAgentic => "isn't an agentic tool".to_owned(),
            ToolSetExclusionReason::NoLatest => "has no instance that isn't deprecated".to_owned(),
            ToolSetExclusionReason::NotTaggedInAllGroups => {
                // name each group the tag is missing from
                let groups = self
                    .missing_groups
                    .iter()
                    .map(|group| format!("`{group}`"))
                    .collect::<Vec<String>>()
                    .join(", ");
                format!("not tagged in {groups}")
            }
        }
    }
}

/// Why a tool was left out of a toolset instance
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum ToolSetExclusionReason {
    /// The toolset is read only and this tool instance declares at least one effect
    NotReadOnly,
    /// The selector matched a tool that is not an agentic tool
    NotAgentic,
    /// The selector matched a tool that has no non-deprecated instance
    NoLatest,
    /// The tool carries the selector's tag in some of the toolset's groups but not in all of them
    NotTaggedInAllGroups,
}

/// Whether a toolset instance can be attached
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum ToolSetInstanceStatus {
    /// This instance resolved cleanly and can be attached to agents
    Ready,
    /// This instance failed validation and cannot be attached to agents
    Invalid {
        /// The reasons this instance is invalid
        errors: Vec<String>,
    },
}

impl ToolSetInstanceStatus {
    /// Get the name of this status, as its intrinsic tag records it
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            ToolSetInstanceStatus::Ready => "Ready",
            ToolSetInstanceStatus::Invalid { .. } => "Invalid",
        }
    }
}

/// What caused a toolset instance to be created
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum ToolSetInstanceTrigger {
    /// The toolset or one of its `Includes` edges changed
    ToolSetUpdated,
    /// A tool this toolset includes at latest, or that its selector matches, got a new instance
    ToolUpdated {
        /// The tool that was updated
        tool: Uuid,
        /// The new instance of that tool
        tool_instance: Uuid,
    },
    /// A user explicitly requested a new instance
    Manual,
}

/// A request to create a toolset entity and its `Includes` edges
#[derive(Debug, Clone, Default, Serialize, Deserialize, Hash)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ToolSetEntityRequest {
    /// The tools to include (Tool IDs = latest, `ToolInstance` IDs = pinned)
    #[serde(default)]
    pub tools: Vec<Uuid>,
    /// Whether only read-only tools (whose instance declares no effects) are included
    pub read_only: Option<bool>,
    /// The tag that adds every agentic tool carrying it in every one of this toolset's groups as
    /// a member
    pub selector: Option<ToolSetSelector>,
    /// Whether the event handler should create new instances automatically on changes
    pub auto_instance: Option<bool>,
    /// Whether to add the toolset's groups that an included tool is missing to that tool
    /// instead of rejecting the edge
    #[serde(default)]
    pub extend_target_groups: bool,
}

impl ToolSetEntityRequest {
    /// Include a tool (latest) or tool instance (pinned)
    ///
    /// # Arguments
    ///
    /// * `tool` - The tool or tool instance to include
    #[must_use]
    pub fn tool(mut self, tool: Uuid) -> Self {
        self.tools.push(tool);
        self
    }

    /// Set the tag selector
    ///
    /// # Arguments
    ///
    /// * `selector` - The tag every selected tool carries
    #[must_use]
    pub fn selector(mut self, selector: ToolSetSelector) -> Self {
        self.selector = Some(selector);
        self
    }

    /// Set whether only read-only tools are included
    ///
    /// # Arguments
    ///
    /// * `read_only` - Whether only read-only tools are included
    #[must_use]
    pub fn read_only(mut self, read_only: bool) -> Self {
        self.read_only = Some(read_only);
        self
    }

    /// Set whether new instances are built automatically
    ///
    /// # Arguments
    ///
    /// * `auto_instance` - Whether new instances are built automatically
    #[must_use]
    pub fn auto_instance(mut self, auto_instance: bool) -> Self {
        self.auto_instance = Some(auto_instance);
        self
    }

    /// Add the toolset's missing groups to included tools instead of rejecting the edges
    #[must_use]
    pub fn extend_target_groups(mut self) -> Self {
        self.extend_target_groups = true;
        self
    }

    /// Get the form fields this request sends, by name
    #[must_use]
    pub fn form_fields(&self) -> Vec<(&'static str, String)> {
        // the kind, then each included tool
        let mut fields = vec![("kind", super::EntityKinds::ToolSet.as_str().to_owned())];
        fields.extend(
            self.tools
                .iter()
                .map(|id| ("metadata[toolset_tools][]", id.to_string())),
        );
        // the selector travels as its two parts
        if let Some(selector) = &self.selector {
            fields.push(("metadata[toolset_selector_key]", selector.key.clone()));
            fields.push(("metadata[toolset_selector_value]", selector.value.clone()));
        }
        // the settings that are set
        if let Some(read_only) = self.read_only {
            fields.push(("metadata[toolset_read_only]", read_only.to_string()));
        }
        if let Some(auto_instance) = self.auto_instance {
            fields.push(("metadata[toolset_auto_instance]", auto_instance.to_string()));
        }
        // widen targets instead of rejecting edges only when asked
        if self.extend_target_groups {
            fields.push(("metadata[extend_target_groups]", "true".to_owned()));
        }
        fields
    }

    /// Add this toolset request's metadata to a form
    ///
    /// # Arguments
    ///
    /// * `form` - The form to add too
    #[cfg(feature = "client")]
    #[must_use]
    pub fn add_to_form(self, form: reqwest::multipart::Form) -> reqwest::multipart::Form {
        // add each of our fields to the form
        self.form_fields()
            .into_iter()
            .fold(form, |form, (name, value)| form.text(name, value))
    }
}

/// A request to create a toolset instance (from the event handler or, rarely, a user)
#[derive(Debug, Clone, Serialize, Deserialize, Hash)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ToolSetInstanceEntityRequest {
    /// The JSON encoded `ToolSetInstanceEntity` to validate and store
    pub instance: String,
}

impl ToolSetInstanceEntityRequest {
    /// Build a request to submit a toolset instance
    ///
    /// # Arguments
    ///
    /// * `instance` - The toolset instance to submit
    pub fn new(instance: &ToolSetInstanceEntity) -> Result<Self, serde_json::Error> {
        // encode the instance the way the form carries it
        Ok(ToolSetInstanceEntityRequest {
            instance: serde_json::to_string(instance)?,
        })
    }

    /// Get the form fields this request sends, by name
    #[must_use]
    pub fn form_fields(&self) -> Vec<(&'static str, String)> {
        // the kind, then the instance as one JSON field
        vec![
            (
                "kind",
                super::EntityKinds::ToolSetInstance.as_str().to_owned(),
            ),
            ("metadata[toolset_instance]", self.instance.clone()),
        ]
    }

    /// Add this toolset instance request's metadata to a form
    ///
    /// # Arguments
    ///
    /// * `form` - The form to add too
    #[cfg(feature = "client")]
    #[must_use]
    pub fn add_to_form(self, form: reqwest::multipart::Form) -> reqwest::multipart::Form {
        // add each of our fields to the form
        self.form_fields()
            .into_iter()
            .fold(form, |form, (name, value)| form.text(name, value))
    }
}

/// The fields of a toolset update, used to build its form fields
#[derive(Debug, Clone, Copy)]
pub struct ToolSetUpdateFields<'a> {
    /// Whether only read-only tools are included
    pub read_only: Option<&'a bool>,
    /// The new tag selector
    pub selector: Option<&'a ToolSetSelector>,
    /// Whether to clear the tag selector
    pub clear_selector: Option<&'a bool>,
    /// Whether new instances are built automatically
    pub auto_instance: Option<&'a bool>,
    /// The tools to include
    pub add_tools: &'a [Uuid],
    /// The tools to stop including
    pub remove_tools: &'a [Uuid],
    /// Whether to widen the targets of added edges instead of rejecting them
    pub extend_target_groups: bool,
}

impl ToolSetUpdateFields<'_> {
    /// Get the form fields this update sends, by name
    #[must_use]
    pub fn form_fields(&self) -> Vec<(&'static str, String)> {
        // the single-valued fields that are set
        let mut fields = Vec::new();
        if let Some(read_only) = self.read_only {
            fields.push(("metadata[toolset_read_only]", read_only.to_string()));
        }
        if let Some(selector) = self.selector {
            fields.push(("metadata[toolset_selector_key]", selector.key.clone()));
            fields.push(("metadata[toolset_selector_value]", selector.value.clone()));
        }
        if let Some(clear) = self.clear_selector {
            fields.push(("metadata[clear_toolset_selector]", clear.to_string()));
        }
        if let Some(auto_instance) = self.auto_instance {
            fields.push(("metadata[toolset_auto_instance]", auto_instance.to_string()));
        }
        // each tool to include or stop including
        fields.extend(
            self.add_tools
                .iter()
                .map(|id| ("metadata[add_toolset_tools][]", id.to_string())),
        );
        fields.extend(
            self.remove_tools
                .iter()
                .map(|id| ("metadata[remove_toolset_tools][]", id.to_string())),
        );
        // widen targets instead of rejecting edges only when asked
        if self.extend_target_groups {
            fields.push(("metadata[extend_target_groups]", "true".to_owned()));
        }
        fields
    }
}

cfg_if::cfg_if! {
    if #[cfg(feature = "api")] {
        use crate::models::EntityMetadataForm;
        use crate::utils::ApiError;
        use crate::bad;

        /// Build a selector from its two form fields, which must be set together and non-empty,
        /// with no `=` in the key
        ///
        /// # Arguments
        ///
        /// * `key` - The selector key field
        /// * `value` - The selector value field
        pub(crate) fn selector_from_fields(
            key: Option<String>,
            value: Option<String>,
        ) -> Result<Option<ToolSetSelector>, ApiError> {
            match (key, value) {
                // neither part leaves the selector alone
                (None, None) => Ok(None),
                // both parts must be non-empty
                (Some(key), Some(value)) => {
                    if key.is_empty() {
                        return bad!("toolset_selector_key can't be empty".to_owned());
                    }
                    // the selector's tag is `key=value`, so the key can't hold the separator
                    if key.contains('=') {
                        return bad!(format!("toolset_selector_key `{key}` can't contain `=`"));
                    }
                    if value.is_empty() {
                        return bad!("toolset_selector_value can't be empty".to_owned());
                    }
                    Ok(Some(ToolSetSelector { key, value }))
                }
                // one part alone names the one that's missing
                (Some(_), None) => {
                    bad!("toolset_selector_value is required with toolset_selector_key".to_owned())
                }
                (None, Some(_)) => {
                    bad!("toolset_selector_key is required with toolset_selector_value".to_owned())
                }
            }
        }

        impl ToolSetEntityRequest {
            /// Take the toolset fields of a submitted entity form
            ///
            /// # Arguments
            ///
            /// * `form` - The submitted metadata form
            pub fn from_form(form: &mut EntityMetadataForm) -> Result<Self, ApiError> {
                // the selector's two parts are set together
                let selector = selector_from_fields(
                    form.toolset_selector_key.take(),
                    form.toolset_selector_value.take(),
                )?;
                Ok(ToolSetEntityRequest {
                    tools: std::mem::take(&mut form.toolset_tools),
                    read_only: form.toolset_read_only.take(),
                    selector,
                    auto_instance: form.toolset_auto_instance.take(),
                    extend_target_groups: form.extend_target_groups == Some(true),
                })
            }
        }

        impl ToolSetInstanceEntityRequest {
            /// Parse the submitted toolset instance of an entity form
            ///
            /// # Arguments
            ///
            /// * `form` - The submitted metadata form
            pub fn parse_form(form: &mut EntityMetadataForm) -> Result<ToolSetInstanceEntity, ApiError> {
                // the instance is required
                let Some(raw) = form.toolset_instance.take() else {
                    return bad!("a toolset instance needs a toolset_instance".to_owned());
                };
                // it must be a whole toolset instance
                match serde_json::from_str(&raw) {
                    Ok(instance) => Ok(instance),
                    Err(err) => bad!(format!("toolset_instance is not a valid toolset instance: {err}")),
                }
            }
        }
    }
}
