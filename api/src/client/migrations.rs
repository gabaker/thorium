//! Pure conversions for moving image-centric data onto tools
//!
//! An upgrade converts every image into a tool with a first tool instance, pins each pipeline
//! stage to the tool instance migrated from its image, and moves the tool names stored in
//! entity provenance into `legacy_tools`. The conversions here take their inputs as values and
//! return what would be written, with no client and no I/O, so a check can compute everything a
//! migration would do while writing nothing, and the same code can run offline.
//!
//! Every reason an item is skipped is a [`MigrationReason`]: a category and a field, rendered
//! `<category>:<field>`, that never holds a name or a stored value. Each reason has a fixed
//! remedy ([`remedy`]). A pass reports its skipped items and notes through the runner's applied
//! report, and [`SkippedItem`] and [`MigrationNote`] convert into that report's items.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use uuid::Uuid;

use crate::models::{
    Dependencies, DroppedImageField, Entity, EntityKinds, EntityMetadata, Image, ImageConversion,
    ImageRequest, Pipeline, ToolInstanceSummary, ToolSpec, ToolVersion, WorkloadSpec,
};

#[cfg(test)]
mod tests;

/// The tag key naming the image a migrated tool was created from
pub const MIGRATED_FROM_IMAGE_TAG: &str = "MigratedFromImage";

/// The version label of a tool migrated from an image without a version
pub const MIGRATED_VERSION: &str = "migrated";

/// The namespace migrated tool ids are derived in, so one image always maps to one tool id
pub const MIGRATED_TOOL_NAMESPACE: Uuid =
    Uuid::from_u128(0x6a1f_3c52_9e0d_4b7a_8c21_d4e5_f607_1829);

/// The namespace the ids of migrated tool instances are derived in, apart from the tool ids
pub const MIGRATED_INSTANCE_NAMESPACE: Uuid =
    Uuid::from_u128(0x3b8e_7d14_52a6_4c0f_9e73_a1c2_b4d5_e608);

/// The username a migrated tool is published as when its image's creator can't publish it
pub const MIGRATION_OPERATOR: &str = "thorium-migration";

/// When migrated tools are created, as seconds since the Unix epoch (2026-10-01 00:00:00 UTC)
///
/// Images carry no creation time, and a migration that is repeated after a crash must write
/// the rows it already wrote again instead of beside them, so every migrated tool, its first
/// instance, and the edge between them are created at this one time.
pub const MIGRATED_AT_SECS: i64 = 1_790_812_800;

/// The parts of an env variable name that make it look like a secret, matched ignoring case
pub const SECRET_ENV_NAME_PARTS: [&str; 6] =
    ["KEY", "TOKEN", "SECRET", "PASSWORD", "PASSWD", "CREDENTIAL"];

/// The entity kinds whose stored provenance holds tool names
pub const PROVENANCE_KINDS: [EntityKinds; 3] = [
    EntityKinds::FileSystem,
    EntityKinds::DecompiledFunction,
    EntityKinds::WindowsProcessTree,
];

/// An item a migration step skipped
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedItem {
    /// The group the item is in
    pub group: String,
    /// What the item is (an image, pipeline stage, entity row, or reaction) and its name or id
    pub item: String,
    /// Why it was skipped, as a reason code that never holds a value; the remedy printed for it
    /// is fixed per code
    pub reason: MigrationReason,
}

impl SkippedItem {
    /// Build a skipped item
    ///
    /// # Arguments
    ///
    /// * `group` - The group the item is in
    /// * `item` - What the item is and its name or id
    /// * `reason` - Why it was skipped
    #[must_use]
    pub fn new(group: impl Into<String>, item: impl Into<String>, reason: MigrationReason) -> Self {
        // gather the item's parts
        SkippedItem {
            group: group.into(),
            item: item.into(),
            reason,
        }
    }
}

/// A skipped item as the runner's applied report lists it, with the reason as its code
#[cfg(feature = "scylla-utils")]
impl From<&SkippedItem> for crate::models::schema::SkippedItem {
    /// Convert a skipped item into the applied report's item
    ///
    /// # Arguments
    ///
    /// * `item` - The skipped item to convert
    fn from(item: &SkippedItem) -> Self {
        // the report keeps the reason as its `<category>:<field>` code, which never holds a value
        crate::models::schema::SkippedItem {
            group: item.group.clone(),
            item: item.item.clone(),
            reason: item.reason.code(),
        }
    }
}

/// A note on one migrated item
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationNote {
    /// The group the item is in
    pub group: String,
    /// The item the note is about (an image name)
    pub item: String,
    /// The note (for example "published as the operator account; creator alice is no longer in
    /// the group")
    pub note: String,
}

impl MigrationNote {
    /// Build a note
    ///
    /// # Arguments
    ///
    /// * `group` - The group the item is in
    /// * `item` - The item the note is about
    /// * `note` - The note
    #[must_use]
    pub fn new(group: impl Into<String>, item: impl Into<String>, note: impl Into<String>) -> Self {
        // gather the note's parts
        MigrationNote {
            group: group.into(),
            item: item.into(),
            note: note.into(),
        }
    }
}

#[cfg(feature = "scylla-utils")]
impl MigrationNote {
    /// Convert this note into the applied report's note
    ///
    /// # Arguments
    ///
    /// * `warning` - Whether the admin should act on the note
    #[must_use]
    pub fn report_note(&self, warning: bool) -> crate::models::schema::ReportNote {
        // the report's note carries the same group, item, and fixed text, plus whether to act on it
        crate::models::schema::ReportNote {
            group: self.group.clone(),
            item: self.item.clone(),
            note: self.note.clone(),
            warning,
        }
    }
}

/// One item a migration step mapped to a tool
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationMapping {
    /// The group the item is in
    pub group: String,
    /// The image, or the pipeline and stage, that was mapped
    pub from: String,
    /// The tool it maps to
    pub tool: Uuid,
    /// The tool instance it maps to
    pub tool_instance: Uuid,
}

/// The kind of problem a reason code describes
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationCategory {
    /// A field the conversion needs has no value
    MissingField,
    /// A field holds a value the conversion can't use
    InvalidValue,
    /// A stored value doesn't parse
    MalformedJson,
    /// A write still failed after its retries
    WriteFailed,
}

impl MigrationCategory {
    /// Get the name this category is rendered with
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        // each category is written in snake case
        match self {
            MigrationCategory::MissingField => "missing_field",
            MigrationCategory::InvalidValue => "invalid_value",
            MigrationCategory::MalformedJson => "malformed_json",
            MigrationCategory::WriteFailed => "write_failed",
        }
    }
}

impl fmt::Display for MigrationCategory {
    /// Write this category's name
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // write the snake case name
        f.write_str(self.as_str())
    }
}

/// Why an item was skipped: a category and the field it concerns, rendered `<category>:<field>`
///
/// The field is a path into the item's shape (`image`, `dependencies.results.images.*`), never a
/// name or a stored value, so items skipped for the same reason group under one code.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct MigrationReason {
    /// The kind of problem
    pub category: MigrationCategory,
    /// The field the problem concerns
    pub field: String,
}

impl MigrationReason {
    /// Build a reason code
    ///
    /// # Arguments
    ///
    /// * `category` - The kind of problem
    /// * `field` - The field the problem concerns
    #[must_use]
    pub fn new(category: MigrationCategory, field: impl Into<String>) -> Self {
        // gather the code's parts
        MigrationReason {
            category,
            field: field.into(),
        }
    }

    /// Build a `missing_field` reason code
    ///
    /// # Arguments
    ///
    /// * `field` - The field that has no value
    #[must_use]
    pub fn missing_field(field: impl Into<String>) -> Self {
        // a field the conversion needs is unset
        MigrationReason::new(MigrationCategory::MissingField, field)
    }

    /// Build an `invalid_value` reason code
    ///
    /// # Arguments
    ///
    /// * `field` - The field holding a value the conversion can't use
    #[must_use]
    pub fn invalid_value(field: impl Into<String>) -> Self {
        // a field holds an unusable value
        MigrationReason::new(MigrationCategory::InvalidValue, field)
    }

    /// Build a `malformed_json` reason code
    ///
    /// # Arguments
    ///
    /// * `field` - The stored field that doesn't parse
    #[must_use]
    pub fn malformed_json(field: impl Into<String>) -> Self {
        // a stored value doesn't parse
        MigrationReason::new(MigrationCategory::MalformedJson, field)
    }

    /// Build a `write_failed` reason code
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of object whose write failed
    #[must_use]
    pub fn write_failed(kind: impl Into<String>) -> Self {
        // a write failed after its retries
        MigrationReason::new(MigrationCategory::WriteFailed, kind)
    }

    /// Render this reason as its code, `<category>:<field>`
    #[must_use]
    pub fn code(&self) -> String {
        // join the category and the field
        format!("{}:{}", self.category, self.field)
    }

    /// Get the fixed remedy printed for this reason
    #[must_use]
    pub fn remedy(&self) -> &'static str {
        // every code has one fixed remedy
        remedy(self)
    }
}

impl fmt::Display for MigrationReason {
    /// Write this reason's code
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // write `<category>:<field>`
        write!(f, "{}:{}", self.category, self.field)
    }
}

/// The remedy for an image that wasn't converted into a tool
const SKIPPED_IMAGE_REMEDY: &str = "The image wasn't migrated: create its tool with `thorctl tools create` and a spec, then set every stage of each pipeline that ran the image with `thorctl pipelines stage-tool`, the stages that converted included, since the pipeline is left unchanged; no rerun is needed";

/// The remedy for an image whose name a tool that wasn't migrated from it already uses
const NAME_CONFLICT_REMEDY: &str = "A tool with the image's name already exists in the group and wasn't migrated from it: rename that tool and rerun the step, or create the image's tool with `thorctl tools create` and set every stage of each pipeline that ran the image with `thorctl pipelines stage-tool`";

/// The remedy for an image skipped because none of the names in one of its dependency lists
/// resolve
const EMPTIED_DEPENDENCY_REMEDY: &str = "None of the images a dependency list names were migrated, and an empty list would allow every tool: create the image's tool with `thorctl tools create` and a spec listing the tools it depends on, then set every stage of each pipeline that ran the image with `thorctl pipelines stage-tool`";

/// The remedy for a dependency name that was dropped from a migrated tool's list
const DROPPED_DEPENDENCY_REMEDY: &str = "A dependency name that names no migrated image in the group was dropped from the migrated tool's list: publish a new tool instance listing the right tool with `thorctl tools publish`";

/// The remedy for a pipeline stage whose image wasn't migrated
const SKIPPED_STAGE_REMEDY: &str = "The stage's image wasn't migrated, so the pipeline is left unchanged and reactions on it are refused: create the tool with `thorctl tools create`, then set every stage of the pipeline, the stages that converted included, with `thorctl pipelines stage-tool <pipeline> <stage> <tool>`; no rerun is needed";

/// The remedy for a stored value that doesn't parse
const MALFORMED_REMEDY: &str = "The stored value doesn't parse and was left unchanged: fix or delete the object, then rerun the step";

/// The remedy for a write that failed after its retries
const WRITE_FAILED_REMEDY: &str =
    "The write still failed after its retries: check the API and backend logs, then rerun the step";

/// The remedy for any other invalid value
const INVALID_VALUE_REMEDY: &str =
    "The object holds a value the migration can't use: fix it, then rerun the step";

/// Get the fixed remedy printed for a reason code
///
/// # Arguments
///
/// * `reason` - The reason to get the remedy of
#[must_use]
pub fn remedy(reason: &MigrationReason) -> &'static str {
    // pick the remedy by category, then by the field it concerns
    match reason.category {
        // a field the image conversion needs is unset, so the image wasn't converted
        MigrationCategory::MissingField => SKIPPED_IMAGE_REMEDY,
        MigrationCategory::InvalidValue => {
            // tell a name conflict, a dropped or emptied dependency, and a stage apart
            let field = reason.field.as_str();
            if field == "name" {
                NAME_CONFLICT_REMEDY
            } else if field.starts_with("dependencies.") && field.ends_with(".*") {
                DROPPED_DEPENDENCY_REMEDY
            } else if field.starts_with("dependencies.") {
                EMPTIED_DEPENDENCY_REMEDY
            } else if field == "stages.*" {
                SKIPPED_STAGE_REMEDY
            } else {
                INVALID_VALUE_REMEDY
            }
        }
        MigrationCategory::MalformedJson => MALFORMED_REMEDY,
        MigrationCategory::WriteFailed => WRITE_FAILED_REMEDY,
    }
}

/// The dependency lists that name other tools
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DependencyKind {
    /// The tools whose results are downloaded
    Results,
    /// The tools whose children are downloaded
    Children,
    /// The tools whose filesystems are reconstructed
    FileSystems,
}

impl DependencyKind {
    /// Get the name of this list's field in the dependency settings
    #[must_use]
    pub fn field(&self) -> &'static str {
        // each list is a field of the dependency settings
        match self {
            DependencyKind::Results => "results",
            DependencyKind::Children => "children",
            DependencyKind::FileSystems => "filesystems",
        }
    }

    /// Get the reason code of a name in this list that doesn't resolve
    #[must_use]
    pub fn dropped_reason(&self) -> MigrationReason {
        // any name in the list, never the name itself
        MigrationReason::invalid_value(format!("dependencies.{}.images.*", self.field()))
    }

    /// Get the reason code of an image skipped because this list would become empty
    #[must_use]
    pub fn emptied_reason(&self) -> MigrationReason {
        // the list as a whole
        MigrationReason::invalid_value(format!("dependencies.{}.images", self.field()))
    }
}

/// The ids of the tools migrated from every image, derived from each image's group and name
///
/// The id of the tool migrated from an image is the `UUIDv5` of `<group>/<image name>` in
/// [`MIGRATED_TOOL_NAMESPACE`], so every dependency name maps to its tool's id before that tool
/// is published, cycles between images don't matter, and a rerun computes the same map.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolIdMap {
    /// The id of the tool migrated from each image, by group and image name
    ids: BTreeMap<(String, String), Uuid>,
}

impl ToolIdMap {
    /// Get the id of the tool migrated from an image
    ///
    /// # Arguments
    ///
    /// * `group` - The image's group
    /// * `name` - The image's name
    #[must_use]
    pub fn tool_id(group: &str, name: &str) -> Uuid {
        // derive the id from the image's group and name
        Uuid::new_v5(
            &MIGRATED_TOOL_NAMESPACE,
            format!("{group}/{name}").as_bytes(),
        )
    }

    /// Get the id of the first tool instance migrated from an image
    ///
    /// # Arguments
    ///
    /// * `group` - The image's group
    /// * `name` - The image's name
    #[must_use]
    pub fn first_instance_id(group: &str, name: &str) -> Uuid {
        // derive the id from the image's group and name in the instance namespace
        Uuid::new_v5(
            &MIGRATED_INSTANCE_NAMESPACE,
            format!("{group}/{name}").as_bytes(),
        )
    }

    /// Map every image to the id of the tool migrated from it
    ///
    /// # Arguments
    ///
    /// * `images` - The images to map
    #[must_use]
    pub fn from_images<'a, I: IntoIterator<Item = &'a Image>>(images: I) -> Self {
        // map each image by its group and name
        let ids = images
            .into_iter()
            .map(|image| {
                let key = (image.group.clone(), image.name.clone());
                (key, ToolIdMap::tool_id(&image.group, &image.name))
            })
            .collect();
        ToolIdMap { ids }
    }

    /// Map every named image to the id of the tool migrated from it
    ///
    /// Images that exist only as requests (an offline conversion) have no stored [`Image`], so
    /// they are mapped by their group and name alone.
    ///
    /// # Arguments
    ///
    /// * `names` - The group and name of each image to map
    #[must_use]
    pub fn from_names<'a, I: IntoIterator<Item = (&'a str, &'a str)>>(names: I) -> Self {
        // map each image by its group and name
        let ids = names
            .into_iter()
            .map(|(group, name)| {
                let key = (group.to_owned(), name.to_owned());
                (key, ToolIdMap::tool_id(group, name))
            })
            .collect();
        ToolIdMap { ids }
    }

    /// Get the id of the tool migrated from an image in this map
    ///
    /// # Arguments
    ///
    /// * `group` - The image's group
    /// * `name` - The image's name
    #[must_use]
    pub fn get(&self, group: &str, name: &str) -> Option<Uuid> {
        // look the image up by its group and name
        self.ids.get(&(group.to_owned(), name.to_owned())).copied()
    }

    /// Remove a skipped image, so names of it no longer resolve
    ///
    /// # Arguments
    ///
    /// * `group` - The image's group
    /// * `name` - The image's name
    pub fn remove(&mut self, group: &str, name: &str) -> Option<Uuid> {
        // drop the image's entry
        self.ids.remove(&(group.to_owned(), name.to_owned()))
    }

    /// Get how many images this map holds
    #[must_use]
    pub fn len(&self) -> usize {
        // one entry per image
        self.ids.len()
    }

    /// Check whether this map holds no images
    #[must_use]
    pub fn is_empty(&self) -> bool {
        // a map without entries is empty
        self.ids.is_empty()
    }

    /// Iterate over every image's group, name, and tool id
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str, Uuid)> {
        // flatten each key into its parts
        self.ids
            .iter()
            .map(|((group, name), id)| (group.as_str(), name.as_str(), *id))
    }

    /// Remove every image whose dependency lists would empty, until none would
    ///
    /// Removing an image can empty the list of another image that names it, so this repeats
    /// until nothing more is removed. Each removed image is added to `skipped` with its reason.
    ///
    /// # Arguments
    ///
    /// * `images` - The images being migrated
    /// * `skipped` - The images already skipped by group and name, with their reasons
    pub fn settle<'a, I>(
        &mut self,
        images: I,
        skipped: &mut BTreeMap<(String, String), MigrationReason>,
    ) where
        I: IntoIterator<Item = &'a Image>,
        I::IntoIter: Clone,
    {
        // settle on each image's group, name, and dependency settings
        self.settle_dependencies(
            images.into_iter().map(|image| {
                (
                    image.group.as_str(),
                    image.name.as_str(),
                    &image.dependencies,
                )
            }),
            skipped,
        );
    }

    /// Remove every image whose dependency lists would empty, until none would
    ///
    /// This is [`ToolIdMap::settle`] for images known only by their group, name, and dependency
    /// settings, such as the image requests of an offline conversion.
    ///
    /// # Arguments
    ///
    /// * `images` - The group, name, and dependency settings of each image being migrated
    /// * `skipped` - The images already skipped by group and name, with their reasons
    pub fn settle_dependencies<'a, I>(
        &mut self,
        images: I,
        skipped: &mut BTreeMap<(String, String), MigrationReason>,
    ) where
        I: IntoIterator<Item = (&'a str, &'a str, &'a Dependencies)>,
        I::IntoIter: Clone,
    {
        // remove every image already skipped
        for (group, name) in skipped.keys() {
            self.remove(group, name);
        }
        // keep checking the images until a pass removes none
        let images = images.into_iter();
        loop {
            // check every image that is still mapped
            let mut removed = false;
            for (group, name, dependencies) in images.clone() {
                // skip images that are no longer mapped
                if self.get(group, name).is_none() {
                    continue;
                }
                // remove an image whose dependency lists would empty
                if let Err(reason) = rewrite_dependencies(group, dependencies, self) {
                    self.remove(group, name);
                    skipped.insert((group.to_owned(), name.to_owned()), reason);
                    removed = true;
                }
            }
            // stop once nothing more was removed
            if !removed {
                break;
            }
        }
    }
}

/// The ids of the tools an image's dependency lists name
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DependencyTools {
    /// The tools whose results are downloaded
    pub results: Vec<Uuid>,
    /// The tools whose children are downloaded
    pub children: Vec<Uuid>,
    /// The tools whose filesystems are reconstructed
    pub filesystems: Vec<Uuid>,
}

/// An image's dependency lists rewritten as tool ids
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DependencyRewrite {
    /// The ids of the tools each list names
    pub tools: DependencyTools,
    /// The names that resolve to no migrated image in the group, by the list they were in
    pub dropped: Vec<(DependencyKind, String)>,
}

/// Rewrite one dependency list's image names as tool ids
///
/// # Arguments
///
/// * `kind` - The list being rewritten
/// * `group` - The group of the image the list belongs to
/// * `names` - The image names in the list
/// * `map` - The ids of the tools migrated from every image
/// * `dropped` - The names that don't resolve
fn rewrite_list(
    kind: DependencyKind,
    group: &str,
    names: &[String],
    map: &ToolIdMap,
    dropped: &mut Vec<(DependencyKind, String)>,
) -> Result<Vec<Uuid>, MigrationReason> {
    // resolve each name in the image's own group only
    let mut ids = Vec::with_capacity(names.len());
    for name in names {
        match map.get(group, name) {
            // list each tool once
            Some(id) => {
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
            // a name without a migrated image is dropped
            None => dropped.push((kind, name.clone())),
        }
    }
    // a non-empty list that would become empty would allow every tool, so it's refused
    if !names.is_empty() && ids.is_empty() {
        return Err(kind.emptied_reason());
    }
    Ok(ids)
}

/// Rewrite an image's dependency lists as the ids of the tools migrated from the images they name
///
/// A name that resolves to no image in the group, or to a skipped image, is dropped. A non-empty
/// list that would become empty gives that list's `emptied` reason instead, since an empty list
/// allows every tool.
///
/// # Arguments
///
/// * `group` - The image's group
/// * `dependencies` - The image's dependency settings
/// * `map` - The ids of the tools migrated from every image
pub fn rewrite_dependencies(
    group: &str,
    dependencies: &Dependencies,
    map: &ToolIdMap,
) -> Result<DependencyRewrite, MigrationReason> {
    // rewrite each list in a fixed order
    let mut dropped = Vec::new();
    let results = rewrite_list(
        DependencyKind::Results,
        group,
        &dependencies.results.images,
        map,
        &mut dropped,
    )?;
    let children = rewrite_list(
        DependencyKind::Children,
        group,
        &dependencies.children.images,
        map,
        &mut dropped,
    )?;
    let filesystems = rewrite_list(
        DependencyKind::FileSystems,
        group,
        &dependencies.filesystems.images,
        map,
        &mut dropped,
    )?;
    // gather the rewritten lists
    let tools = DependencyTools {
        results,
        children,
        filesystems,
    };
    Ok(DependencyRewrite { tools, dropped })
}

/// Get the env variable names that look like secrets, sorted
///
/// # Arguments
///
/// * `names` - The env variable names to check
#[must_use]
pub fn secret_env_names<'a, I: IntoIterator<Item = &'a String>>(names: I) -> Vec<String> {
    // keep the names holding any secret-like part, ignoring case
    let mut found = names
        .into_iter()
        .filter(|name| {
            let upper = name.to_uppercase();
            SECRET_ENV_NAME_PARTS
                .iter()
                .any(|part| upper.contains(part))
        })
        .cloned()
        .collect::<Vec<String>>();
    found.sort_unstable();
    found
}

/// Who a migrated tool is published as
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigratedSubmitter {
    /// The image's creator, who still exists and is in the image's group
    Creator(String),
    /// The account the migration runs as
    Operator,
}

impl MigratedSubmitter {
    /// The username the tool is published as
    #[must_use]
    pub fn username(&self) -> &str {
        // the creator publishes their own tool, and the operator account everything else
        match self {
            MigratedSubmitter::Creator(creator) => creator,
            MigratedSubmitter::Operator => MIGRATION_OPERATOR,
        }
    }
}

/// When migrated tools are created
///
/// # Panics
///
/// Never in practice: the constant is a valid timestamp.
#[must_use]
pub fn migrated_at() -> DateTime<Utc> {
    // the constant is a valid timestamp
    DateTime::from_timestamp(MIGRATED_AT_SECS, 0).expect("the migration time is a valid timestamp")
}

/// The tool an image converts into, ready to be published
#[derive(Debug, Clone, PartialEq)]
pub struct MigratedTool {
    /// The tool's id, derived from the image's group and name
    pub id: Uuid,
    /// The group the tool is published in (the image's)
    pub group: String,
    /// The tool's name (the image's)
    pub name: String,
    /// The tool's description (the image's)
    pub description: Option<String>,
    /// The version label of the tool's first instance
    pub version: ToolVersion,
    /// The spec of the tool's first instance
    pub spec: ToolSpec,
    /// Who the tool is published as
    pub submitter: MigratedSubmitter,
    /// The tags the tool is published with, naming the image it was migrated from
    pub tags: HashMap<String, HashSet<String>>,
    /// The non-default image fields the spec's environment never reads (`env` by name only)
    pub dropped: Vec<DroppedImageField>,
    /// The ids of the tools the image's dependency lists name
    pub dependency_tools: DependencyTools,
}

/// An image's conversion into a tool, with what the report says about it
#[derive(Debug, Clone, PartialEq)]
pub struct ImageMigration {
    /// The tool the image converts into
    pub tool: MigratedTool,
    /// The notes on this image (who the tool is published as, its version, dropped fields and
    /// dependency names)
    pub notes: Vec<MigrationNote>,
    /// The warnings on this image (env variable names that look like secrets)
    pub warnings: Vec<MigrationNote>,
    /// The issues of an image that is migrated anyway (dependency names that were dropped)
    pub issues: Vec<SkippedItem>,
}

/// What an image's conversion needs besides the image
#[derive(Debug, Clone, Copy)]
pub struct ImageContext<'a> {
    /// Whether the image's creator still exists and is in the image's group
    pub creator_in_group: bool,
    /// When the image was last modified, if known
    pub last_modified: Option<DateTime<Utc>>,
    /// The ids of the tools migrated from every image
    pub map: &'a ToolIdMap,
}

/// Describe an image in a skipped item
///
/// # Arguments
///
/// * `name` - The image's name
fn image_item(name: &str) -> String {
    // name the item's type and the image
    format!("image {name}")
}

/// Choose who a migrated tool is published as, noting the choice
///
/// The image's creator publishes it while they still exist and are in the image's group, and
/// the operator account publishes it otherwise.
///
/// # Arguments
///
/// * `image` - The image being converted
/// * `creator_in_group` - Whether the image's creator still exists and is in its group
/// * `notes` - The image's notes to add the choice to
fn choose_submitter(
    image: &Image,
    creator_in_group: bool,
    notes: &mut Vec<MigrationNote>,
) -> MigratedSubmitter {
    // the creator publishes the tool only while still in the group
    if creator_in_group && !image.creator.is_empty() {
        notes.push(MigrationNote::new(
            &image.group,
            &image.name,
            format!("published as its creator `{}`", image.creator),
        ));
        MigratedSubmitter::Creator(image.creator.clone())
    } else {
        notes.push(MigrationNote::new(
            &image.group,
            &image.name,
            format!(
                "published as the operator account; creator `{}` no longer exists or is no longer in the group",
                image.creator
            ),
        ));
        MigratedSubmitter::Operator
    }
}

/// Choose a migrated tool's version: the image's, or the migrated label with a note of when the
/// image was last modified
///
/// # Arguments
///
/// * `image` - The image being converted
/// * `last_modified` - When the image was last modified, if known
/// * `notes` - The image's notes to add the label's note to
fn choose_version(
    image: &Image,
    last_modified: Option<DateTime<Utc>>,
    notes: &mut Vec<MigrationNote>,
) -> ToolVersion {
    // keep the image's own version
    if let Some(version) = &image.version {
        return version.clone();
    }
    // label an image without one as migrated, noting when it was last modified
    let modified = last_modified.map_or_else(|| "unknown".to_owned(), |time| time.to_rfc3339());
    notes.push(MigrationNote::new(
        &image.group,
        &image.name,
        format!("no version: labeled `{MIGRATED_VERSION}`; image last modified {modified}"),
    ));
    ToolVersion::Custom(MIGRATED_VERSION.to_owned())
}

/// Build the warning for an image whose env variable names look like secrets, naming the
/// variables and never their values
///
/// # Arguments
///
/// * `group` - The image's group
/// * `name` - The image's name
/// * `env` - The image's env variables
fn secret_warning(
    group: &str,
    name: &str,
    env: &HashMap<String, Option<String>>,
) -> Option<MigrationNote> {
    // find the names that look like secrets
    let names = secret_env_names(env.keys());
    if names.is_empty() {
        return None;
    }
    Some(MigrationNote::new(
        group,
        name,
        format!(
            "env variables whose names look like secrets: {}; the migrated spec is visible to the tool's groups, so move their values to a K8s Secret volume",
            names.join(", ")
        ),
    ))
}

/// Convert an image into the tool it's migrated to
///
/// The spec is the image's conversion for its scaler's environment. An image the conversion
/// rejects (a container environment without an image to run) is skipped with
/// `missing_field:image`, and an image whose dependency list would become empty is skipped with
/// that list's code. The spec's dependency settings name the tools migrated from the images the
/// image's lists name; names that don't resolve are dropped with a note and an issue.
///
/// # Arguments
///
/// * `image` - The image to convert
/// * `ctx` - What the conversion needs besides the image
pub fn convert_image(image: &Image, ctx: &ImageContext<'_>) -> Result<ImageMigration, SkippedItem> {
    // the tool keeps the image's group and name
    let group = image.group.as_str();
    let name = image.name.as_str();
    // the only way the spec conversion rejects an image is a missing container image
    let Ok(conversion) = WorkloadSpec::from_image(image) else {
        return Err(SkippedItem::new(
            group,
            image_item(name),
            MigrationReason::missing_field("image"),
        ));
    };
    // rewrite the dependency names as tool ids
    let rewrite = match rewrite_dependencies(group, &image.dependencies, ctx.map) {
        Ok(rewrite) => rewrite,
        Err(reason) => return Err(SkippedItem::new(group, image_item(name), reason)),
    };
    // note and raise an issue for each dropped dependency name
    let (mut notes, issues) = dropped_dependency_notes(group, name, &rewrite);
    // choose who publishes the tool and its version, noting both
    let submitter = choose_submitter(image, ctx.creator_in_group, &mut notes);
    let version = choose_version(image, ctx.last_modified, &mut notes);
    // list the fields the environment drops, naming env variables only
    notes.extend(dropped_fields_note(group, name, &conversion.dropped));
    // warn about env variable names that look like secrets, never their values
    let warnings = secret_warning(group, name, &image.env)
        .into_iter()
        .collect();
    // build the tool
    let tool = migrated_tool_from(
        group,
        name,
        image.description.clone(),
        version,
        submitter,
        conversion,
        rewrite.tools,
    );
    Ok(ImageMigration {
        tool,
        notes,
        warnings,
        issues,
    })
}

/// Convert an image request into the tool it would be migrated to
///
/// This is [`convert_image`] for an image that exists only as a request, such as a legacy
/// toolbox entry converted offline. The spec is the request's conversion for its scaler's
/// environment (the same conversion a stored image gets), a request the conversion rejects is
/// skipped with `missing_field:image`, and dependency names are rewritten, noted, and refused
/// exactly as for a stored image. The tool has the given group, name, and version label rather
/// than the request's own. A request has no creator or modification time, so the tool is
/// published as the operator account, which an offline caller ignores, and no note is made of
/// who publishes it or of the version.
///
/// # Arguments
///
/// * `group` - The group the tool is published in
/// * `name` - The tool's name
/// * `label` - The version label of the tool's first instance
/// * `request` - The image request to convert
/// * `map` - The ids of the tools migrated from every image
pub fn convert_image_request(
    group: &str,
    name: &str,
    label: ToolVersion,
    request: &ImageRequest,
    map: &ToolIdMap,
) -> Result<ImageMigration, SkippedItem> {
    // the only way the spec conversion rejects a request is a missing container image
    let Ok(conversion) = WorkloadSpec::from_image_request(request) else {
        return Err(SkippedItem::new(
            group,
            image_item(name),
            MigrationReason::missing_field("image"),
        ));
    };
    // rewrite the dependency names as tool ids
    let rewrite = match rewrite_dependencies(group, &request.dependencies, map) {
        Ok(rewrite) => rewrite,
        Err(reason) => return Err(SkippedItem::new(group, image_item(name), reason)),
    };
    // note and raise an issue for each dropped dependency name
    let (mut notes, issues) = dropped_dependency_notes(group, name, &rewrite);
    // list the fields the environment drops, naming env variables only
    notes.extend(dropped_fields_note(group, name, &conversion.dropped));
    // warn about env variable names that look like secrets, never their values
    let warnings = secret_warning(group, name, &request.env)
        .into_iter()
        .collect();
    // build the tool
    let tool = migrated_tool_from(
        group,
        name,
        request.description.clone(),
        label,
        MigratedSubmitter::Operator,
        conversion,
        rewrite.tools,
    );
    Ok(ImageMigration {
        tool,
        notes,
        warnings,
        issues,
    })
}

/// Note and raise an issue for each dependency name a rewrite dropped
///
/// # Arguments
///
/// * `group` - The image's group
/// * `name` - The image's name
/// * `rewrite` - The image's rewritten dependency lists
fn dropped_dependency_notes(
    group: &str,
    name: &str,
    rewrite: &DependencyRewrite,
) -> (Vec<MigrationNote>, Vec<SkippedItem>) {
    // add a note and an issue for each dropped name
    let mut notes = Vec::new();
    let mut issues = Vec::new();
    for (kind, dropped) in &rewrite.dropped {
        notes.push(MigrationNote::new(
            group,
            name,
            format!(
                "dropped `{dropped}` from the {} dependency list: it names no migrated image in the group",
                kind.field()
            ),
        ));
        issues.push(SkippedItem::new(
            group,
            image_item(name),
            kind.dropped_reason(),
        ));
    }
    (notes, issues)
}

/// Note the fields an image's environment never reads, naming env variables only
///
/// # Arguments
///
/// * `group` - The image's group
/// * `name` - The image's name
/// * `dropped` - The fields the conversion dropped
fn dropped_fields_note(
    group: &str,
    name: &str,
    dropped: &[DroppedImageField],
) -> Option<MigrationNote> {
    // an empty list needs no note
    if dropped.is_empty() {
        return None;
    }
    // list the dropped fields in their fixed order
    let fields = dropped
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<String>>()
        .join(", ");
    Some(MigrationNote::new(
        group,
        name,
        format!("dropped fields its environment never reads: {fields}"),
    ))
}

/// Build a migrated tool from an image's conversion and its rewritten dependency lists
///
/// # Arguments
///
/// * `group` - The tool's group
/// * `name` - The tool's name
/// * `description` - The tool's description
/// * `version` - The version label of the tool's first instance
/// * `submitter` - Who the tool is published as
/// * `conversion` - The image's conversion to a spec
/// * `tools` - The ids of the tools the image's dependency lists name
fn migrated_tool_from(
    group: &str,
    name: &str,
    description: Option<String>,
    version: ToolVersion,
    submitter: MigratedSubmitter,
    conversion: ImageConversion,
    tools: DependencyTools,
) -> MigratedTool {
    // name the tools the image's dependency lists name in the spec
    let mut spec = conversion.spec;
    let dependencies = &mut spec.data.dependencies;
    dependencies.results.tools.clone_from(&tools.results);
    dependencies.children.tools.clone_from(&tools.children);
    dependencies
        .filesystems
        .tools
        .clone_from(&tools.filesystems);
    // tag the tool with the image it was migrated from
    let mut tags = HashMap::new();
    tags.insert(
        MIGRATED_FROM_IMAGE_TAG.to_owned(),
        HashSet::from([name.to_owned()]),
    );
    // build the tool
    MigratedTool {
        id: ToolIdMap::tool_id(group, name),
        group: group.to_owned(),
        name: name.to_owned(),
        description,
        version,
        spec: ToolSpec::Image(spec),
        submitter,
        tags,
        dropped: conversion.dropped,
        dependency_tools: tools,
    }
}

/// An entity in a group, as the conversions need to see it
#[derive(Debug, Clone)]
pub struct GroupEntity {
    /// The entity's id
    pub id: Uuid,
    /// The entity's kind
    pub kind: EntityKinds,
    /// The group this entity is seen in
    pub group: String,
    /// The entity's name
    pub name: String,
    /// The image named by the entity's `MigratedFromImage` tag in this group, if it has one
    pub migrated_from_image: Option<String>,
    /// The entity's tool instances, if it's a tool
    pub instances: Vec<ToolInstanceSummary>,
}

impl GroupEntity {
    /// Describe an entity as seen in one of its groups
    ///
    /// # Arguments
    ///
    /// * `entity` - The entity, read with its tags (and its instances, if it's a tool)
    /// * `group` - The group it's seen in
    #[must_use]
    pub fn from_entity(entity: &Entity, group: &str) -> Self {
        // find the image this entity's tag names in this group
        let migrated_from_image = entity.tags.get(MIGRATED_FROM_IMAGE_TAG).and_then(|values| {
            values
                .iter()
                .filter(|(_, groups)| groups.contains(group))
                .map(|(value, _)| value.clone())
                .min()
        });
        // get a tool's instances
        let instances = match &entity.metadata {
            EntityMetadata::Tool(tool) => tool.instances.clone(),
            _ => Vec::new(),
        };
        GroupEntity {
            id: entity.id,
            kind: entity.kind,
            group: group.to_owned(),
            name: entity.name.clone(),
            migrated_from_image,
            instances,
        }
    }

    /// Get this tool's first instance: the oldest one
    #[must_use]
    pub fn first_instance(&self) -> Option<Uuid> {
        // the oldest instance is the first one published
        self.instances
            .iter()
            .min_by_key(|instance| instance.created)
            .map(|instance| instance.id)
    }
}

/// Whether an image still needs its tool
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageClass {
    /// The image has no tool yet
    New,
    /// An earlier run already migrated the image to this tool
    AlreadyMigrated {
        /// The tool migrated from the image
        tool: Uuid,
    },
    /// A tool that wasn't migrated from the image already has its name
    Conflict(SkippedItem),
}

/// Find the tool migrated from an image in its own group
///
/// Only a tool counts: an entity of another kind with the tag doesn't.
///
/// # Arguments
///
/// * `group` - The image's group
/// * `image` - The image's name
/// * `existing` - The entities in the image's group
fn migrated_tool<'a>(
    group: &str,
    image: &str,
    existing: &'a [GroupEntity],
) -> Option<&'a GroupEntity> {
    // keep the tools of this group tagged with the image
    existing.iter().find(|entity| {
        entity.kind == EntityKinds::Tool
            && entity.group == group
            && entity.migrated_from_image.as_deref() == Some(image)
    })
}

/// Classify an image against the entities already in its group
///
/// # Arguments
///
/// * `group` - The image's group
/// * `name` - The image's name
/// * `existing` - The entities in the image's group
#[must_use]
pub fn classify_image(group: &str, name: &str, existing: &[GroupEntity]) -> ImageClass {
    // a tool tagged with this image was migrated by an earlier run
    if let Some(tool) = migrated_tool(group, name, existing) {
        return ImageClass::AlreadyMigrated { tool: tool.id };
    }
    // any other tool with the image's name is a conflict
    let conflict = existing.iter().any(|entity| {
        entity.kind == EntityKinds::Tool && entity.group == group && entity.name == name
    });
    if conflict {
        return ImageClass::Conflict(SkippedItem::new(
            group,
            image_item(name),
            MigrationReason::invalid_value("name"),
        ));
    }
    ImageClass::New
}

/// The tool a pipeline stage resolves to
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageTarget {
    /// The first instance of the stored tool migrated from the stage's image
    Pinned {
        /// The tool migrated from the stage's image
        tool: Uuid,
        /// That tool's first instance, which the stage is pinned to
        tool_instance: Uuid,
    },
    /// The tool the stage's image would be migrated to, which isn't stored yet
    Planned {
        /// The id the tool would get
        tool: Uuid,
    },
}

/// How one pipeline's unconverted stages resolve
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StageResolution {
    /// The stages that resolve, by name
    pub stages: BTreeMap<String, StageTarget>,
    /// The stages that don't resolve because their image wasn't migrated
    pub skipped: Vec<SkippedItem>,
}

impl StageResolution {
    /// Record a stage's target, or skip the stage when it has none
    ///
    /// # Arguments
    ///
    /// * `group` - The pipeline's group
    /// * `pipeline` - The pipeline's name
    /// * `stage` - The stage's name
    /// * `target` - What the stage resolved to, if anything
    fn settle_stage(
        &mut self,
        group: &str,
        pipeline: &str,
        stage: &str,
        target: Option<StageTarget>,
    ) {
        match target {
            // a resolved stage is recorded by name
            Some(target) => {
                self.stages.insert(stage.to_owned(), target);
            }
            // an unresolved stage's image wasn't migrated
            None => self.skipped.push(SkippedItem::new(
                group,
                stage_item(pipeline, stage),
                MigrationReason::invalid_value("stages.*"),
            )),
        }
    }

    /// Get the `stage_tools` entries to add: each pinned stage's tool instance
    #[must_use]
    pub fn stage_tools(&self) -> BTreeMap<String, Uuid> {
        // only stages pinned to a stored tool instance can be written
        self.stages
            .iter()
            .filter_map(|(stage, target)| match target {
                StageTarget::Pinned { tool_instance, .. } => Some((stage.clone(), *tool_instance)),
                StageTarget::Planned { .. } => None,
            })
            .collect()
    }

    /// Check whether the pipeline would change
    #[must_use]
    pub fn changes(&self) -> bool {
        // a pipeline changes when any stage resolves
        !self.stages.is_empty()
    }

    /// Map each pinned stage to its tool and tool instance for the report
    ///
    /// # Arguments
    ///
    /// * `pipeline` - The pipeline these stages are in
    #[must_use]
    pub fn mappings(&self, pipeline: &Pipeline) -> Vec<MigrationMapping> {
        // only a pinned stage has a tool instance to map to
        self.stages
            .iter()
            .filter_map(|(stage, target)| match target {
                StageTarget::Pinned {
                    tool,
                    tool_instance,
                } => Some(MigrationMapping {
                    group: pipeline.group.clone(),
                    from: format!("{}/{stage}", pipeline.name),
                    tool: *tool,
                    tool_instance: *tool_instance,
                }),
                StageTarget::Planned { .. } => None,
            })
            .collect()
    }
}

/// Resolve each of a pipeline's stages without a `stage_tools` entry to a tool
///
/// A stage resolves to the first instance of the tool in the pipeline's own group tagged with
/// the stage's image. Before that tool is stored, a stage whose image is in `map` resolves to
/// the id its tool would get. Any other stage's image wasn't migrated, so it's skipped with
/// `invalid_value:stages.*`. Stages that already have an entry are left alone.
///
/// # Arguments
///
/// * `pipeline` - The pipeline whose stages to resolve
/// * `existing` - The entities in the pipeline's group
/// * `map` - The ids the images to be migrated would get, before their tools are stored
#[must_use]
pub fn resolve_stages(
    pipeline: &Pipeline,
    existing: &[GroupEntity],
    map: Option<&ToolIdMap>,
) -> StageResolution {
    // resolve each stage no tool backs yet, in the pipeline's own group only
    let group = pipeline.group.as_str();
    let mut resolution = StageResolution::default();
    for stage in pipeline.unconverted_stages() {
        // a stored tool pins the stage to its first instance
        let stored = migrated_tool(group, stage, existing).and_then(|tool| {
            tool.first_instance()
                .map(|tool_instance| StageTarget::Pinned {
                    tool: tool.id,
                    tool_instance,
                })
        });
        // otherwise the tool the image would be migrated to
        let planned = || {
            map.and_then(|map| map.get(group, stage))
                .map(|tool| StageTarget::Planned { tool })
        };
        resolution.settle_stage(group, &pipeline.name, stage, stored.or_else(planned));
    }
    resolution
}

/// Resolve the stages of a pipeline request to the tools their images convert to
///
/// This is [`resolve_stages`] for a pipeline that exists only as a request, such as a legacy
/// toolbox pipeline converted offline, where no tool is stored yet. A stage whose image is in
/// `map` resolves to the id its tool would get, and any other stage's image wasn't converted,
/// so it's skipped with `invalid_value:stages.*`. A stage repeated in `stages` resolves once.
///
/// # Arguments
///
/// * `group` - The pipeline's group
/// * `pipeline` - The pipeline's name
/// * `stages` - The pipeline's stage names (the images it runs)
/// * `map` - The ids the images to be converted would get
#[must_use]
pub fn resolve_request_stages<'a, I: IntoIterator<Item = &'a str>>(
    group: &str,
    pipeline: &str,
    stages: I,
    map: &ToolIdMap,
) -> StageResolution {
    // resolve each stage through the map only, since nothing is stored
    let mut resolution = StageResolution::default();
    let mut seen = HashSet::new();
    for stage in stages {
        // a stage seen before is already resolved or skipped
        if !seen.insert(stage) {
            continue;
        }
        let planned = map
            .get(group, stage)
            .map(|tool| StageTarget::Planned { tool });
        resolution.settle_stage(group, pipeline, stage, planned);
    }
    resolution
}

/// Describe a pipeline stage in a skipped item
///
/// # Arguments
///
/// * `pipeline` - The pipeline's name
/// * `stage` - The stage's name
fn stage_item(pipeline: &str, stage: &str) -> String {
    // name the item's type, the pipeline, and the stage
    format!("pipeline {pipeline} stage {stage}")
}

/// One stored `entities` row, as the provenance rewrite sees it
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvenanceRow {
    /// The row's key
    pub key: String,
    /// The row's `kind` column
    pub kind: String,
    /// The row's `kind_data` column
    pub kind_data: String,
}

/// Whether a stored row's provenance still needs rewriting
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProvenanceClass {
    /// The row needs nothing: another kind, or no tool names left in `tools`
    Current,
    /// The row holds tool names in `tools`, as rows of this layout did
    Migratable {
        /// The layout the row is in
        layout: u32,
    },
    /// The row can't be read and is left as it is
    Unrecognized {
        /// Why the row can't be read
        reason: MigrationReason,
    },
}

/// Check whether a row's kind stores provenance
///
/// # Arguments
///
/// * `kind` - The row's `kind` column
fn is_provenance_kind(kind: &str) -> bool {
    // compare against each provenance kind's name
    PROVENANCE_KINDS.iter().any(|known| known.as_ref() == kind)
}

/// Parse a provenance row's kind data as the entity kind its `kind` column names
///
/// # Arguments
///
/// * `row` - The row to parse
fn parse_provenance(row: &ProvenanceRow) -> Result<EntityMetadata, MigrationReason> {
    // the stored kind data is the entity's metadata
    let reason = || MigrationReason::malformed_json("kind_data");
    let metadata = serde_json::from_str::<EntityMetadata>(&row.kind_data).map_err(|_| reason())?;
    // the metadata must be of the row's kind
    if EntityKinds::from(&metadata).as_ref() != row.kind {
        return Err(reason());
    }
    Ok(metadata)
}

/// Classify a stored row for the provenance rewrite
///
/// Rows of other kinds and provenance rows whose `tools` hold only tool instance ids (or
/// nothing) are current; provenance rows with tool names in `tools` are migratable from layout
/// 1; a provenance row that doesn't parse is unrecognized with `malformed_json:kind_data`.
///
/// # Arguments
///
/// * `row` - The row to classify
#[must_use]
pub fn classify(row: &ProvenanceRow) -> ProvenanceClass {
    // rows of other kinds hold no provenance
    if !is_provenance_kind(&row.kind) {
        return ProvenanceClass::Current;
    }
    // a row that doesn't parse as its kind is left as it is
    if let Err(reason) = parse_provenance(row) {
        return ProvenanceClass::Unrecognized { reason };
    }
    // find any stored tool string that isn't a tool instance id
    let value = serde_json::from_str::<serde_json::Value>(&row.kind_data).unwrap_or_default();
    let has_names = value
        .get(&row.kind)
        .and_then(|data| data.get("tools"))
        .and_then(serde_json::Value::as_array)
        .is_some_and(|tools| {
            tools
                .iter()
                .filter_map(serde_json::Value::as_str)
                .any(|tool| Uuid::parse_str(tool).is_err())
        });
    // only rows with names left in `tools` need rewriting
    if has_names {
        ProvenanceClass::Migratable { layout: 1 }
    } else {
        ProvenanceClass::Current
    }
}

/// Rewrite a stored row so every tool name in `tools` is in `legacy_tools`
///
/// The row is read the way the API reads it, where a stored string that parses as a UUID is a
/// tool instance id and stays in `tools`, and written back in that shape. Nothing adds names.
/// Rows of other kinds come back unchanged.
///
/// # Arguments
///
/// * `row` - The row to rewrite
pub fn up(row: ProvenanceRow) -> Result<ProvenanceRow, MigrationReason> {
    // rows of other kinds hold no provenance
    if !is_provenance_kind(&row.kind) {
        return Ok(row);
    }
    // read the row as the API does
    let metadata = parse_provenance(&row)?;
    // write it back in the shape the API reads
    let kind_data = serde_json::to_string(&metadata)
        .map_err(|_| MigrationReason::malformed_json("kind_data"))?;
    Ok(ProvenanceRow { kind_data, ..row })
}
