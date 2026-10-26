//! Entities for unit tests of the agent and toolset commands

use chrono::Utc;
use std::collections::HashMap;
use thorium::models::{
    AgentEntity, Entity, EntityKinds, EntityMetadata, ToolInstanceEntity, ToolSetEntity, ToolSpec,
    ToolVersion,
};
use uuid::Uuid;

/// Build an entity with a fresh id
///
/// # Arguments
///
/// * `name` - The entity's name
/// * `kind` - The entity's kind
/// * `metadata` - The entity's metadata
/// * `groups` - The groups the entity is in
pub fn entity(name: &str, kind: EntityKinds, metadata: EntityMetadata, groups: &[&str]) -> Entity {
    // fill every field a test doesn't care about
    Entity {
        id: Uuid::new_v4(),
        name: name.to_owned(),
        kind,
        metadata,
        description: None,
        submitter: "tester".to_owned(),
        groups: groups.iter().map(|group| (*group).to_owned()).collect(),
        tags: HashMap::default(),
        image: None,
        created: Utc::now(),
    }
}

/// Build a tool in the group `lab`
///
/// # Arguments
///
/// * `name` - The tool's name
pub fn tool(name: &str) -> Entity {
    // the root's metadata isn't read by the commands under test
    entity(name, EntityKinds::Tool, EntityMetadata::Other, &["lab"])
}

/// Build a version of a tool, named `{tool}@{version}`
///
/// # Arguments
///
/// * `tool` - The tool the version belongs to
/// * `version` - The version label
pub fn tool_instance(tool: &Entity, version: &str) -> Entity {
    // an instance carries its tool's id and its own version label
    let meta = ToolInstanceEntity {
        tool: tool.id,
        version: ToolVersion::Custom(version.to_owned()),
        previous: None,
        spec_hash: "hash".to_owned(),
        deprecated: false,
        spec: ToolSpec::default(),
        portability: None,
        provides: Vec::new(),
    };
    entity(
        &format!("{}@{version}", tool.name),
        EntityKinds::ToolInstance,
        EntityMetadata::ToolInstance(meta),
        &["lab"],
    )
}

/// Build a toolset in the group `lab`
///
/// # Arguments
///
/// * `name` - The toolset's name
pub fn toolset(name: &str) -> Entity {
    // the root's metadata isn't read by the commands under test
    entity(name, EntityKinds::ToolSet, EntityMetadata::Other, &["lab"])
}

/// Build an AI agent in the group `lab` with its edges filled in
///
/// # Arguments
///
/// * `name` - The agent's name
/// * `fill` - Sets the agent's edges
pub fn agent(name: &str, fill: impl FnOnce(&mut AgentEntity)) -> Entity {
    // start from an agent with no edges
    let mut meta = AgentEntity {
        modified: Utc::now(),
        ..AgentEntity::default()
    };
    fill(&mut meta);
    entity(
        name,
        EntityKinds::Agent,
        EntityMetadata::Agent(meta),
        &["lab"],
    )
}

/// Build a toolset in the group `lab` with its settings and included tools
///
/// # Arguments
///
/// * `name` - The toolset's name
/// * `fill` - Sets the toolset's settings and tools
pub fn toolset_with(name: &str, fill: impl FnOnce(&mut ToolSetEntity)) -> Entity {
    // start from a toolset that includes nothing and builds instances automatically
    let mut meta = ToolSetEntity {
        auto_instance: true,
        modified: Utc::now(),
        ..ToolSetEntity::default()
    };
    fill(&mut meta);
    entity(
        name,
        EntityKinds::ToolSet,
        EntityMetadata::ToolSet(meta),
        &["lab"],
    )
}
