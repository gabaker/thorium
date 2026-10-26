//! Naming the tools that back pipeline stages
//!
//! The API stores a stage's tool as an id: a tool id follows the tool's latest version and a tool
//! instance id is pinned to one version. People and exported files name tools instead, as
//! `TOOL` or `TOOL@VERSION`, so this module converts between the two.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;

use serde::{Deserialize, Serialize};
use thorium::models::{EntityKinds, EntityListOpts, EntityMetadata};
use thorium::{Error, Thorium};
use uuid::Uuid;

/// How many tools one listing page may hold; a group cannot realistically exceed it
const TOOL_LIST_LIMIT: usize = 10_000;

/// The tool that backs one pipeline stage, by name
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StageToolRef {
    /// The tool's name (or its id)
    pub tool: String,
    /// The version label the stage is pinned to, or `None` to follow the tool's latest version
    #[serde(default)]
    pub version: Option<String>,
}

impl StageToolRef {
    /// Parse `TOOL` or `TOOL@VERSION`
    ///
    /// # Arguments
    ///
    /// * `raw` - The text to parse
    pub fn parse(raw: &str) -> Result<Self, Error> {
        // an id never holds a version, so it is taken whole
        if Uuid::parse_str(raw).is_ok() {
            return Ok(StageToolRef {
                tool: raw.to_owned(),
                version: None,
            });
        }
        // split off the version after the last @
        let (tool, version) = match raw.rsplit_once('@') {
            Some((tool, version)) => (tool, Some(version)),
            None => (raw, None),
        };
        // neither half may be empty
        if tool.trim().is_empty() || version.is_some_and(|version| version.trim().is_empty()) {
            return Err(Error::new(format!(
                "'{raw}' is not a tool like TOOL or TOOL@VERSION"
            )));
        }
        Ok(StageToolRef {
            tool: tool.trim().to_owned(),
            version: version.map(|version| version.trim().to_owned()),
        })
    }
}

impl fmt::Display for StageToolRef {
    /// Write the tool as `TOOL` or `TOOL@VERSION`
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.version {
            Some(version) => write!(f, "{}@{version}", self.tool),
            None => write!(f, "{}", self.tool),
        }
    }
}

/// Parse a `STAGE=TOOL[@VERSION]` argument
///
/// # Arguments
///
/// * `raw` - The text to parse
pub fn parse_stage_tool(raw: &str) -> Result<(String, StageToolRef), String> {
    // the stage is everything before the first =
    let Some((stage, tool)) = raw.split_once('=') else {
        return Err(format!("'{raw}' is not STAGE=TOOL or STAGE=TOOL@VERSION"));
    };
    // a stage needs a name
    if stage.trim().is_empty() {
        return Err(format!("'{raw}' names no stage"));
    }
    // the rest names the tool
    let tool = StageToolRef::parse(tool).map_err(|err| err.to_string())?;
    Ok((stage.trim().to_owned(), tool))
}

/// Name the tools that back a pipeline's stages
///
/// A stage backed by a tool instance reads as the instance's tool pinned to its version, and a
/// stage backed by a tool reads as the tool following its latest version.
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `stage_tools` - The pipeline's stage tools by stage
pub async fn describe(
    thorium: &Thorium,
    stage_tools: &BTreeMap<String, Uuid>,
) -> Result<BTreeMap<String, StageToolRef>, Error> {
    // each distinct id is read once however many stages use it
    let mut named: HashMap<Uuid, StageToolRef> = HashMap::new();
    let mut refs = BTreeMap::new();
    for (stage, id) in stage_tools {
        // reuse a name we already read
        if let Some(found) = named.get(id) {
            refs.insert(stage.clone(), found.clone());
            continue;
        }
        // read the entity that backs this stage
        let entity = thorium.entities.get(*id).await?;
        let found = match &entity.metadata {
            // a tool follows its latest version
            EntityMetadata::Tool(_) => StageToolRef {
                tool: entity.name.clone(),
                version: None,
            },
            // a tool instance is pinned to its version of its tool
            EntityMetadata::ToolInstance(meta) => StageToolRef {
                tool: thorium.entities.get(meta.tool).await?.name,
                version: Some(meta.version.to_string()),
            },
            _ => {
                return Err(Error::new(format!(
                    "Stage '{stage}' is backed by {id}, which is a {}, not a tool",
                    entity.kind
                )));
            }
        };
        named.insert(*id, found.clone());
        refs.insert(stage.clone(), found);
    }
    Ok(refs)
}

/// List the ids of the tools in a group by name
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `group` - The group to list tools in
/// * `wanted` - The names to keep
async fn tools_named(
    thorium: &Thorium,
    group: &str,
    wanted: &BTreeSet<&str>,
) -> Result<HashMap<String, Vec<Uuid>>, Error> {
    // list the tools of the group, page by page
    let opts = EntityListOpts::default()
        .kind(EntityKinds::Tool)
        .groups(vec![group])
        .limit(TOOL_LIST_LIMIT);
    let mut cursor = thorium.entities.list(&opts).await?;
    let mut found: HashMap<String, Vec<Uuid>> = HashMap::new();
    loop {
        // keep the tools we were asked for
        for line in cursor.data.drain(..) {
            if wanted.contains(line.name.as_str()) {
                found.entry(line.name).or_default().push(line.id);
            }
        }
        // stop once every page was read
        if cursor.exhausted() {
            break;
        }
        cursor.refill().await?;
    }
    Ok(found)
}

/// Find the id a stage tool is stored as
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `group` - The pipeline's group
/// * `stage` - The stage the tool backs, for messages
/// * `wanted` - The tool as named
/// * `by_name` - The tools of the group by name
async fn resolve_one(
    thorium: &Thorium,
    group: &str,
    stage: &str,
    wanted: &StageToolRef,
    by_name: &HashMap<String, Vec<Uuid>>,
) -> Result<Uuid, Error> {
    // a tool named by id is read directly
    let tool_id = if let Ok(id) = Uuid::parse_str(&wanted.tool) {
        id
    } else {
        // otherwise the name must name exactly one tool of the group
        match by_name.get(&wanted.tool).map(Vec::as_slice) {
            Some([id]) => *id,
            None | Some([]) => {
                return Err(Error::new(format!(
                    "Stage '{stage}': tool '{}' was not found in group '{group}'",
                    wanted.tool
                )));
            }
            Some(ids) => {
                return Err(Error::new(format!(
                    "Stage '{stage}': several tools in group '{group}' are named '{}'; use one of their ids: {}",
                    wanted.tool,
                    ids.iter()
                        .map(ToString::to_string)
                        .collect::<Vec<String>>()
                        .join(", ")
                )));
            }
        }
    };
    // without a version the stage follows the tool, which an id of an instance already pins
    let Some(version) = &wanted.version else {
        return Ok(tool_id);
    };
    // a pinned version must be one of the tool's instances
    let tool = thorium.entities.get(tool_id).await?;
    let EntityMetadata::Tool(meta) = &tool.metadata else {
        return Err(Error::new(format!(
            "Stage '{stage}': '{}' is a {}, so it has no versions",
            wanted.tool, tool.kind
        )));
    };
    meta.instances
        .iter()
        .find(|summary| summary.version.to_string() == *version)
        .map(|summary| summary.id)
        .ok_or_else(|| {
            Error::new(format!(
                "Stage '{stage}': tool '{}' has no version '{version}' (it has: {})",
                tool.name,
                meta.instances
                    .iter()
                    .map(|summary| summary.version.to_string())
                    .collect::<Vec<String>>()
                    .join(", ")
            ))
        })
}

/// Find the ids a pipeline's stage tools are stored as
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `group` - The pipeline's group, whose tools the names are looked up in
/// * `refs` - The tools by stage
pub async fn resolve(
    thorium: &Thorium,
    group: &str,
    refs: &BTreeMap<String, StageToolRef>,
) -> Result<BTreeMap<String, Uuid>, Error> {
    // one listing answers every name
    let wanted: BTreeSet<&str> = refs
        .values()
        .filter(|found| Uuid::parse_str(&found.tool).is_err())
        .map(|found| found.tool.as_str())
        .collect();
    let by_name = if wanted.is_empty() {
        HashMap::new()
    } else {
        tools_named(thorium, group, &wanted).await?
    };
    // find each stage's tool
    let mut ids = BTreeMap::new();
    for (stage, found) in refs {
        ids.insert(
            stage.clone(),
            resolve_one(thorium, group, stage, found, &by_name).await?,
        );
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::{StageToolRef, parse_stage_tool};

    /// A bare tool follows its latest version and `TOOL@VERSION` pins one
    #[test]
    fn parses_tools_and_versions() {
        // a bare name has no version
        let bare = StageToolRef::parse("strings").unwrap();
        assert_eq!(bare.tool, "strings");
        assert_eq!(bare.version, None);
        // a version follows the last @
        let pinned = StageToolRef::parse("strings@1.2.0").unwrap();
        assert_eq!(pinned.tool, "strings");
        assert_eq!(pinned.version.as_deref(), Some("1.2.0"));
        assert_eq!(pinned.to_string(), "strings@1.2.0");
        // an id is taken whole
        let id = "69ff8820-f688-4968-bd6f-45a4cb0f78a9";
        assert_eq!(StageToolRef::parse(id).unwrap().tool, id);
        // either half missing is refused
        assert!(StageToolRef::parse("").is_err());
        assert!(StageToolRef::parse("@1.0.0").is_err());
        assert!(StageToolRef::parse("strings@").is_err());
    }

    /// A stage tool argument is `STAGE=TOOL[@VERSION]`
    #[test]
    fn parses_stage_tool_arguments() {
        // a stage and a pinned tool
        let (stage, tool) = parse_stage_tool("scan=strings@2.0.0").unwrap();
        assert_eq!(stage, "scan");
        assert_eq!(tool.to_string(), "strings@2.0.0");
        // the stage is everything before the first =
        let (stage, tool) = parse_stage_tool("scan=strings").unwrap();
        assert_eq!((stage.as_str(), tool.version), ("scan", None));
        // a missing stage, tool or = is refused
        assert!(parse_stage_tool("strings").is_err());
        assert!(parse_stage_tool("=strings").is_err());
        assert!(parse_stage_tool("scan=").is_err());
    }
}
