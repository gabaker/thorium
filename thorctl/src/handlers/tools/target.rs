//! Finding the tool, and optionally one of its versions, a command line target names

use thorium::models::{
    Entity, EntityKinds, EntityMetadata, ToolEntity, ToolInstanceEntity, ToolVersion,
};
use thorium::{Error, Thorium};
use uuid::Uuid;

use super::find_tool;

/// A target as typed: a tool and, after an `@`, one of its versions
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolRef {
    /// The tool's id or name (or the id of one of its versions)
    pub tool: String,
    /// The version label, when one was named
    pub version: Option<String>,
}

impl ToolRef {
    /// Split a target into a tool and an optional version
    ///
    /// An id names a tool or a version by itself, so it is never split.
    ///
    /// # Arguments
    ///
    /// * `raw` - The target as typed (`scan`, `scan@1.2.0`, or an id)
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        // an id has no version part
        if Uuid::parse_str(raw).is_ok() {
            return ToolRef {
                tool: raw.to_owned(),
                version: None,
            };
        }
        // otherwise a version follows the last `@` when both sides have text
        match raw.rsplit_once('@') {
            Some((tool, version)) if !tool.is_empty() && !version.is_empty() => ToolRef {
                tool: tool.to_owned(),
                version: Some(version.to_owned()),
            },
            _ => ToolRef {
                tool: raw.to_owned(),
                version: None,
            },
        }
    }
}

/// A tool and the version a target selected
#[derive(Debug, Clone)]
pub struct Resolved {
    /// The tool
    pub tool: Entity,
    /// The tool's own settings
    pub meta: ToolEntity,
    /// The version the target named (the latest when `latest` was asked for), if there is one
    pub instance: Option<Entity>,
    /// Whether the target named the version (by label or id) rather than defaulting to the latest
    pub explicit: bool,
}

impl Resolved {
    /// The selected version's settings
    ///
    /// # Arguments
    ///
    /// * `what` - What the caller needs the version for, for the error
    pub fn instance_meta(&self, what: &str) -> Result<&ToolInstanceEntity, Error> {
        // read the selected version
        match self.instance.as_ref().map(|entity| &entity.metadata) {
            Some(EntityMetadata::ToolInstance(meta)) => Ok(meta),
            _ => Err(Error::new(format!(
                "Tool {} has no version to {what}",
                self.tool.name
            ))),
        }
    }
}

/// Read the settings of a tool entity
///
/// # Arguments
///
/// * `tool` - The tool entity
pub fn tool_meta(tool: &Entity) -> Result<ToolEntity, Error> {
    // only tools have tool settings
    match &tool.metadata {
        EntityMetadata::Tool(meta) => Ok(meta.clone()),
        _ => Err(Error::new(format!("{} is not a tool", tool.name))),
    }
}

/// Find a version of a tool by its label
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `tool` - The tool
/// * `label` - The version label
pub async fn find_version(thorium: &Thorium, tool: &Entity, label: &str) -> Result<Entity, Error> {
    // list the tool's versions and find the label
    let wanted = ToolVersion::from(label);
    let versions = thorium.entities.tool_instances(tool.id).await?;
    match versions.iter().find(|summary| summary.version == wanted) {
        Some(summary) => thorium.entities.get(summary.id).await,
        None => Err(Error::new(format!(
            "Tool {} has no version {label}; its versions are {}",
            tool.name,
            versions
                .iter()
                .map(|summary| summary.version.to_string())
                .collect::<Vec<String>>()
                .join(", ")
        ))),
    }
}

/// Find the tool a target names, and the version it selects
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `raw` - The target as typed (a tool, `TOOL@VERSION`, or the id of a tool or a version)
/// * `latest` - Whether a target without a version selects the tool's latest version
pub async fn resolve(thorium: &Thorium, raw: &str, latest: bool) -> Result<Resolved, Error> {
    let target = ToolRef::parse(raw);
    // an id may name a version, which selects its tool too
    if let Ok(id) = Uuid::parse_str(&target.tool) {
        let found = thorium.entities.get(id).await?;
        if found.kind == EntityKinds::ToolInstance {
            let EntityMetadata::ToolInstance(meta) = &found.metadata else {
                return Err(Error::new(format!("{raw} is not a tool version")));
            };
            let tool = thorium.entities.get(meta.tool).await?;
            let meta = tool_meta(&tool)?;
            return Ok(Resolved {
                tool,
                meta,
                instance: Some(found),
                explicit: true,
            });
        }
    }
    // otherwise find the tool by id or name
    let tool = find_tool(thorium, &target.tool).await?;
    let meta = tool_meta(&tool)?;
    // pick the version named, or the latest one when asked for
    let explicit = target.version.is_some();
    let instance = match (&target.version, meta.latest) {
        (Some(label), _) => Some(find_version(thorium, &tool, label).await?),
        (None, Some(id)) if latest => Some(thorium.entities.get(id).await?),
        (None, None) if latest => {
            return Err(Error::new(format!(
                "Tool {} has no versions yet",
                tool.name
            )));
        }
        _ => None,
    };
    Ok(Resolved {
        tool,
        meta,
        instance,
        explicit,
    })
}

#[cfg(test)]
mod tests {
    use super::ToolRef;

    /// A target splits at its last `@`, and an id is never split
    #[test]
    fn parses_targets() {
        // a bare name has no version
        assert_eq!(
            ToolRef::parse("scan"),
            ToolRef {
                tool: "scan".into(),
                version: None
            }
        );
        // a version follows the last @
        assert_eq!(
            ToolRef::parse("scan@1.2.0"),
            ToolRef {
                tool: "scan".into(),
                version: Some("1.2.0".into())
            }
        );
        assert_eq!(
            ToolRef::parse("a@b@2"),
            ToolRef {
                tool: "a@b".into(),
                version: Some("2".into())
            }
        );
        // an empty side is not a version
        assert_eq!(ToolRef::parse("scan@").version, None);
        assert_eq!(ToolRef::parse("@1").version, None);
        // an id is a tool or a version by itself
        let id = "69ff8820-f688-4968-bd6f-45a4cb0f78a9";
        assert_eq!(
            ToolRef::parse(id),
            ToolRef {
                tool: id.into(),
                version: None
            }
        );
    }
}
