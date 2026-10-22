//! Refuse deleting an entity that something explicitly references
//!
//! A shared definition (a tool, a tool instance, a toolset, an agent, or a workflow) is
//! referenced explicitly by the graph edges other entities hold to it, by the stage tools of
//! pipelines, and by the tool ids that tool instance specs name. Deleting it would leave those
//! referrers pointing at nothing, so the delete is a `409` that names the referrers. Membership
//! that only follows a tag selector is resolved when an instance is built, so it never blocks
//! a delete. Referrers the caller can't see are counted, never named.

use futures::{StreamExt, stream};
use std::collections::{BTreeMap, BTreeSet};
use tracing::instrument;
use uuid::Uuid;

use crate::conflict;
use crate::models::backends::db;
use crate::models::backends::entities::models as model_entities;
use crate::models::{
    AgenticToolBacking, AssociationKind, AssociationTargetColumn, Entity, EntityKinds,
    EntityListParams, EntityMetadata, GroupPolicy, ToolSpec, User,
};
use crate::utils::{ApiError, Shared};

/// The most referrers a refusal names before it only counts the rest
const MAX_NAMED: usize = 10;

/// How many groups to read per page when listing every group
const GROUP_PAGE: usize = 1000;

/// How many tool instances to read at once when looking for specs that name an entity
const READ_CONCURRENCY: usize = 16;

/// Whether edges of a kind keep their target from being deleted
///
/// The graph edges of agents, toolsets, and workflows block it, and so does the `Provides` edge
/// from a harness instance to the native tool instances it provides.
///
/// # Arguments
///
/// * `kind` - The kind of the edge
fn blocks_delete(kind: AssociationKind) -> bool {
    // graph edges and the harness's provided tools reference their targets
    kind.is_graph_edge() || kind == AssociationKind::Provides
}

/// Whether a tool instance's spec explicitly references an entity
///
/// A container backed agentic tool pins an Image tool instance.
///
/// # Arguments
///
/// * `spec` - The spec of the tool instance that may reference the entity
/// * `target` - The id of the entity being deleted
fn spec_references(spec: &ToolSpec, target: Uuid) -> bool {
    // each spec kind names other tools in its own way
    match spec {
        ToolSpec::Agentic(agentic) => match &agentic.backing {
            // a container backing pins one Image tool instance
            AgenticToolBacking::Container(container) => container.tool_instance == target,
            // the other backings name no tool in Thorium
            _ => false,
        },
        // Image and Harness specs name no tool instance
        ToolSpec::Image(_) | ToolSpec::Harness(_) => false,
    }
}

/// Build the refusal message of a delete that referrers block
///
/// Names at most `MAX_NAMED` of the visible referrers and then counts the rest, and counts the
/// referrers the caller can't see without naming them.
///
/// # Arguments
///
/// * `label` - The kind and name of the entity being deleted, such as ``toolset `scan` ``
/// * `visible` - The described referrers the caller can see
/// * `hidden` - How many referrers the caller can't see
pub(crate) fn refusal_message(label: &str, visible: &[String], hidden: usize) -> String {
    // name the first referrers the caller can see
    let mut parts = visible
        .iter()
        .take(MAX_NAMED)
        .cloned()
        .collect::<Vec<String>>();
    // count the visible referrers past the bound
    if visible.len() > MAX_NAMED {
        parts.push(format!("{} more", visible.len() - MAX_NAMED));
    }
    // count the referrers the caller can't see
    if hidden > 0 {
        parts.push(format!("{hidden} you can't see"));
    }
    format!(
        "{label} is still referenced by {}; delete or change them first",
        parts.join(", ")
    )
}

/// How a referrer references its target
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Relation {
    /// An entity holds a graph edge of this kind to the target
    Edge(AssociationKind),
    /// A pipeline stage runs the target
    PipelineStage,
    /// A tool instance's spec names the target by id
    ToolSpec,
}

/// One thing that explicitly references a target, as the caller may see it
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Referrer {
    /// How the referrer references its target
    pub relation: Relation,
    /// The id of the referenced target
    pub target: Uuid,
    /// The kind of the referring entity, or `None` for a pipeline
    pub kind: Option<EntityKinds>,
    /// The referring entity's id, or `None` for a pipeline
    pub id: Option<Uuid>,
    /// The referrer's name
    pub name: String,
    /// The groups the reference was found in
    pub groups: BTreeSet<String>,
}

impl Referrer {
    /// Describe this referrer for a refusal
    fn describe(&self) -> String {
        // pipelines are named by group, entities by kind, name, and id
        match (&self.kind, self.id) {
            (Some(kind), Some(id)) => format!("{kind} `{}` ({id})", self.name),
            (None, Some(id)) => format!("entity {id}"),
            _ => format!(
                "pipeline `{}` in group `{}`",
                self.name,
                self.groups.iter().next().cloned().unwrap_or_default()
            ),
        }
    }
}

/// Everything that explicitly references some targets
#[derive(Debug, Default)]
pub(crate) struct References {
    /// The referrers the caller can see, in a stable order
    pub visible: Vec<Referrer>,
    /// How many referrers the caller can't see
    pub hidden: usize,
}

impl References {
    /// Whether nothing references the targets
    pub fn is_empty(&self) -> bool {
        self.visible.is_empty() && self.hidden == 0
    }

    /// Build the refusal message of a delete these references block
    ///
    /// # Arguments
    ///
    /// * `label` - The kind and name of the entity being deleted, such as ``toolset `scan` ``
    pub fn message(&self, label: &str) -> String {
        // describe the referrers the caller can see and count the rest
        let described = self
            .visible
            .iter()
            .map(Referrer::describe)
            .collect::<Vec<String>>();
        refusal_message(label, &described, self.hidden)
    }
}

/// The referrers found so far, split by whether the caller can see them
#[derive(Default)]
struct Found {
    /// The referrers the caller can see, by what identifies them
    visible: BTreeMap<String, Referrer>,
    /// What identifies the referrers the caller can't see
    hidden: BTreeSet<String>,
}

impl Found {
    /// Whether a referrer the caller can see is already recorded
    ///
    /// # Arguments
    ///
    /// * `key` - What identifies the referrer
    fn knows(&self, key: &str) -> bool {
        self.visible.contains_key(key)
    }

    /// Record a referrer the caller can see
    ///
    /// A referrer seen in several groups keeps all of them, and a referrer seen in any group the
    /// caller can see is named even when it's also in a group the caller can't see.
    ///
    /// # Arguments
    ///
    /// * `key` - What identifies the referrer
    /// * `referrer` - The referrer, built only when the caller can see it
    fn add_visible(&mut self, key: String, referrer: Referrer) {
        // a referrer the caller can see is named and no longer hidden
        self.hidden.remove(&key);
        match self.visible.get_mut(&key) {
            Some(known) => known.groups.extend(referrer.groups),
            None => {
                self.visible.insert(key, referrer);
            }
        }
    }

    /// Record a referrer the caller can't see
    ///
    /// # Arguments
    ///
    /// * `key` - What identifies the referrer
    fn add_hidden(&mut self, key: String) {
        // only a referrer never seen in a visible group is only counted
        if !self.visible.contains_key(&key) {
            self.hidden.insert(key);
        }
    }

    /// Finish into the references the caller may see
    fn finish(self) -> References {
        References {
            visible: self.visible.into_values().collect(),
            hidden: self.hidden.len(),
        }
    }
}

/// List the names of every group
///
/// # Arguments
///
/// * `shared` - Shared Thorium objects
#[instrument(name = "references::all_group_names", skip_all, err(Debug))]
async fn all_group_names(shared: &Shared) -> Result<Vec<String>, ApiError> {
    // page through the group names
    let mut names = Vec::new();
    let mut cursor = 0;
    loop {
        // read the next page of names
        let page = db::groups::list(cursor, GROUP_PAGE, shared).await?;
        names.extend(page.names);
        // stop once every page was read
        match page.cursor {
            Some(next) => cursor = next,
            None => break,
        }
    }
    Ok(names)
}

/// List the ids of every tool instance in some groups
///
/// # Arguments
///
/// * `user` - The user deleting the entity
/// * `groups` - The groups to list from
/// * `shared` - Shared Thorium objects
#[instrument(
    name = "references::instance_ids",
    skip_all,
    fields(groups = groups.len()),
    err(Debug)
)]
async fn instance_ids(
    user: &User,
    groups: &[String],
    shared: &Shared,
) -> Result<BTreeSet<Uuid>, ApiError> {
    // list the tool instances in these groups
    let params = EntityListParams {
        groups: groups.to_vec(),
        kinds: vec![EntityKinds::ToolInstance],
        limit: 10_000,
        ..EntityListParams::default()
    };
    let mut cursor = db::entities::list(user, params, true, shared).await?;
    let mut ids = BTreeSet::new();
    loop {
        // keep each instance once
        ids.extend(cursor.data.drain(..).map(|line| line.id));
        // stop once every page was read
        if cursor.exhausted() {
            break;
        }
        cursor.next(shared).await?;
    }
    Ok(ids)
}

/// The tool a target belongs to, whose own instances never reference it
///
/// # Arguments
///
/// * `target` - A tool or tool instance
fn own_tool(target: &Entity) -> Uuid {
    // an instance belongs to its tool and a tool to itself
    match &target.metadata {
        EntityMetadata::ToolInstance(instance) => instance.tool,
        _ => target.id,
    }
}

/// Find the tool instances whose specs reference any of the targets
///
/// The specs name their referent by id and no edge records them, so every tool instance in
/// every group is read once for all of the targets. Instances of a target's own tool never
/// reference it.
///
/// # Arguments
///
/// * `user` - The user asking
/// * `targets` - The tools and tool instances to find referrers for
/// * `found` - Where the referrers are recorded
/// * `shared` - Shared Thorium objects
#[instrument(
    name = "references::spec_referrers",
    skip_all,
    fields(targets = targets.len()),
    err(Debug)
)]
async fn spec_referrers(
    user: &User,
    targets: &[Entity],
    found: &mut Found,
    shared: &Shared,
) -> Result<(), ApiError> {
    // list every tool instance in every group
    let groups = all_group_names(shared).await?;
    let ids = instance_ids(user, &groups, shared).await?;
    // read the instances a few at a time, skipping ones deleted since they were listed
    let reads = stream::iter(ids.into_iter().map(|id| {
        let groups = &groups;
        async move { db::entities::get_untagged(groups, id, shared).await }
    }))
    .buffer_unordered(READ_CONCURRENCY)
    .collect::<Vec<Result<Entity, ApiError>>>()
    .await;
    for read in reads {
        // an instance deleted since the listing references nothing
        let other = match read {
            Ok(other) => other,
            Err(err) if err.status() == axum::http::StatusCode::NOT_FOUND => continue,
            Err(err) => return Err(err),
        };
        // only tool instances reference other tools
        let EntityMetadata::ToolInstance(meta) = &other.metadata else {
            continue;
        };
        // name the instance only when the caller can see one of its groups
        let visible = user.is_admin() || other.groups.iter().any(|g| user.groups.contains(g));
        for target in targets {
            // an instance never references its own tool, and only some specs name the target
            if meta.tool == own_tool(target) || !spec_references(&meta.spec, target.id) {
                continue;
            }
            let key = format!("{}:{}", other.id, target.id);
            if visible {
                let referrer = Referrer {
                    relation: Relation::ToolSpec,
                    target: target.id,
                    kind: Some(other.kind),
                    id: Some(other.id),
                    name: other.name.clone(),
                    groups: other.groups.iter().cloned().collect(),
                };
                found.add_visible(key, referrer);
            } else {
                found.add_hidden(key);
            }
        }
    }
    Ok(())
}

/// Find the pipelines whose stages run any of the targets
///
/// # Arguments
///
/// * `user` - The user asking
/// * `target` - The tool or tool instance to find referrers for
/// * `groups` - Every group of the target
/// * `found` - Where the referrers are recorded
/// * `shared` - Shared Thorium objects
#[instrument(
    name = "references::pipeline_referrers",
    skip_all,
    fields(target = target.id.to_string()),
    err(Debug)
)]
async fn pipeline_referrers(
    user: &User,
    target: &Entity,
    groups: &[String],
    found: &mut Found,
    shared: &Shared,
) -> Result<(), ApiError> {
    // pipelines are kept per group
    for group in groups {
        for pipeline in db::pipelines::list_all(group, shared).await? {
            // only pipelines with a stage backed by the target reference it
            if !pipeline.stage_tools.values().any(|id| *id == target.id) {
                continue;
            }
            // name the pipeline only when the caller can see its group
            let key = format!("pipeline:{group}:{}:{}", pipeline.name, target.id);
            if user.is_admin() || user.groups.contains(group) {
                let referrer = Referrer {
                    relation: Relation::PipelineStage,
                    target: target.id,
                    kind: None,
                    id: None,
                    name: pipeline.name,
                    groups: BTreeSet::from([group.clone()]),
                };
                found.add_visible(key, referrer);
            } else {
                found.add_hidden(key);
            }
        }
    }
    Ok(())
}

/// Find the entities whose graph edges point at a target
///
/// # Arguments
///
/// * `user` - The user asking
/// * `target` - The entity to find referrers for
/// * `groups` - Every group of the target
/// * `found` - Where the referrers are recorded
/// * `shared` - Shared Thorium objects
#[instrument(
    name = "references::edge_referrers",
    skip_all,
    fields(target = target.id.to_string()),
    err(Debug)
)]
async fn edge_referrers(
    user: &User,
    target: &Entity,
    groups: &[String],
    found: &mut Found,
    shared: &Shared,
) -> Result<(), ApiError> {
    // find the edges pointing at the target in each of its groups
    let referrers =
        db::associations::referrers_in_groups(user, &[target.id], groups, shared).await?;
    for (group, refs) in referrers {
        // a group the caller can't see doesn't show its referrers
        let visible = user.is_admin() || user.groups.contains(&group);
        for referrer in refs {
            // only entities holding a graph edge to the target reference it
            let AssociationTargetColumn::Entity(id) = &referrer.source else {
                continue;
            };
            if !blocks_delete(referrer.kind) || *id == target.id {
                continue;
            }
            let key = format!("{id}:{}", target.id);
            if !visible {
                found.add_hidden(key);
                continue;
            }
            // read the referrer once, and just add this group to one already read
            let described = if found.knows(&key) {
                None
            } else {
                db::entities::get_untagged(std::slice::from_ref(&group), *id, shared)
                    .await
                    .ok()
            };
            let (kind, name) = described
                .map(|entity| (Some(entity.kind), entity.name))
                .unwrap_or((None, format!("entity {id}")));
            let referrer = Referrer {
                relation: Relation::Edge(referrer.kind),
                target: target.id,
                kind,
                id: Some(*id),
                name,
                groups: BTreeSet::from([group.clone()]),
            };
            found.add_visible(key, referrer);
        }
    }
    Ok(())
}

/// Find everything that explicitly references some targets
///
/// The targets are a tool and its instances, or any other shared definitions. Each target's
/// groups are all of the groups it is in, whichever of them the caller can see. The result
/// names the referrers in groups the caller can see and counts the rest.
///
/// # Arguments
///
/// * `user` - The user asking
/// * `targets` - The entities to find referrers for
/// * `shared` - Shared Thorium objects
#[instrument(
    name = "references::find_references",
    skip_all,
    fields(targets = targets.len()),
    err(Debug)
)]
pub(crate) async fn find_references(
    user: &User,
    targets: &[Entity],
    shared: &Shared,
) -> Result<References, ApiError> {
    let mut found = Found::default();
    let mut tools = Vec::new();
    for target in targets {
        // only shared definitions are referenced this way, and model configuration has its own
        // check
        if target.kind.group_policy() != GroupPolicy::Contained
            || model_entities::is_model_kind(target.kind)
        {
            continue;
        }
        // look in every group the target is in, including the ones the caller can't see
        let groups = db::entities::all_groups(target.id, shared).await?;
        // find the graph edges that point at the target
        edge_referrers(user, target, &groups, &mut found, shared).await?;
        // tools and tool instances are also named by pipelines and by tool instance specs
        if matches!(target.kind, EntityKinds::Tool | EntityKinds::ToolInstance) {
            pipeline_referrers(user, target, &groups, &mut found, shared).await?;
            tools.push(target.clone());
        }
    }
    // read the tool instances once for all of the tools and tool instances
    if !tools.is_empty() {
        spec_referrers(user, &tools, &mut found, shared).await?;
    }
    Ok(found.finish())
}

/// Make sure nothing explicitly references an entity that is being deleted
///
/// Only shared definitions are checked; model configuration has its own check. The refusal is a
/// `409` naming the referrers the caller can see and counting the rest.
///
/// # Arguments
///
/// * `user` - The user deleting the entity
/// * `entity` - The entity being deleted
/// * `shared` - Shared Thorium objects
#[instrument(
    name = "references::check_delete",
    skip_all,
    fields(id = entity.id.to_string()),
    err(Debug)
)]
pub(crate) async fn check_delete(
    user: &User,
    entity: &Entity,
    shared: &Shared,
) -> Result<(), ApiError> {
    // find what references the entity
    let references = find_references(user, std::slice::from_ref(entity), shared).await?;
    // an entity nothing references can be deleted
    if references.is_empty() {
        return Ok(());
    }
    // refuse the delete, naming what the caller can see
    let label = format!("{} `{}`", entity.kind, entity.name);
    conflict!(references.message(&label))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A refusal names few referrers in full
    #[test]
    fn message_names_referrers() {
        // two visible referrers are both named
        let visible = vec!["agent `a`".to_owned(), "agent `b`".to_owned()];
        let msg = refusal_message("toolset `kit`", &visible, 0);
        assert_eq!(
            msg,
            "toolset `kit` is still referenced by agent `a`, agent `b`; delete or change them first"
        );
    }

    /// A refusal names a bounded list and counts the rest and the hidden referrers
    #[test]
    fn message_is_bounded() {
        // far more referrers than the bound, and some the caller can't see
        let visible = (0..25).map(|i| format!("agent `a{i}`")).collect::<Vec<_>>();
        let msg = refusal_message("tool `scan`", &visible, 3);
        assert!(msg.contains("agent `a9`"), "{msg}");
        assert!(!msg.contains("agent `a10`"), "{msg}");
        assert!(msg.contains("15 more"), "{msg}");
        assert!(msg.contains("3 you can't see"), "{msg}");
    }

    /// A refusal with only hidden referrers counts them and names nothing
    #[test]
    fn message_counts_hidden() {
        let msg = refusal_message("agent `lead`", &[], 2);
        assert_eq!(
            msg,
            "agent `lead` is still referenced by 2 you can't see; delete or change them first"
        );
    }

    /// Build a referrer of a toolset in some groups
    ///
    /// # Arguments
    ///
    /// * `id` - The referrer's id
    /// * `groups` - The groups the reference was found in
    fn agent(id: Uuid, groups: &[&str]) -> Referrer {
        Referrer {
            relation: Relation::Edge(AssociationKind::Calls),
            target: Uuid::nil(),
            kind: Some(EntityKinds::Agent),
            id: Some(id),
            name: "x".to_owned(),
            groups: groups.iter().map(|group| (*group).to_owned()).collect(),
        }
    }

    /// Found keeps a referrer named once any group it is seen in is visible, and keeps every
    /// group the referrer was seen in
    #[test]
    fn found_prefers_visible() {
        // seen first where the caller can't look, then where they can
        let id = Uuid::new_v4();
        let mut found = Found::default();
        found.add_hidden("x".to_owned());
        assert_eq!(found.hidden.len(), 1);
        found.add_visible("x".to_owned(), agent(id, &["a"]));
        assert!(found.hidden.is_empty());
        assert!(found.knows("x"));
        // a later hidden sighting doesn't hide it again, and a visible one adds its group
        found.add_hidden("x".to_owned());
        found.add_visible("x".to_owned(), agent(id, &["b"]));
        let references = found.finish();
        assert_eq!(references.hidden, 0);
        assert_eq!(references.visible.len(), 1);
        let groups = references.visible[0].groups.iter().collect::<Vec<_>>();
        assert_eq!(groups, ["a", "b"]);
    }

    /// References describe visible referrers and count hidden ones
    #[test]
    fn references_build_the_message() {
        // one visible agent and two referrers the caller can't see
        let id = Uuid::new_v4();
        let references = References {
            visible: vec![agent(id, &["a"])],
            hidden: 2,
        };
        assert!(!references.is_empty());
        let msg = references.message("toolset `kit`");
        assert_eq!(
            msg,
            format!(
                "toolset `kit` is still referenced by Agent `x` ({id}), 2 you can't see; delete or change them first"
            )
        );
        assert!(References::default().is_empty());
    }

    /// Only the blocking edge kinds reference their targets
    #[test]
    fn blocking_kinds() {
        // the graph edges and the harness's provided tools block
        for kind in [
            AssociationKind::Orchestrates,
            AssociationKind::DelegatesTo,
            AssociationKind::Calls,
            AssociationKind::RunsIn,
            AssociationKind::Includes,
            AssociationKind::Provides,
        ] {
            assert!(blocks_delete(kind), "{kind}");
        }
        // an instance's links to its root and general associations don't
        for kind in [
            AssociationKind::PublishedFrom,
            AssociationKind::BuiltFrom,
            AssociationKind::AssociatedWith,
        ] {
            assert!(!blocks_delete(kind), "{kind}");
        }
    }

    /// A spec references the instance a container backing pins, and nothing else of an agentic
    /// tool
    #[test]
    fn specs_reference_pinned_instances() {
        let (named, other) = (Uuid::new_v4(), Uuid::new_v4());
        // a container backed agentic tool pins one instance
        let container = crate::models::AgenticTool {
            backing: AgenticToolBacking::Container(crate::models::ContainerToolBacking {
                tool_instance: named,
                ..crate::models::ContainerToolBacking::default()
            }),
            ..crate::models::AgenticTool::default()
        };
        let agentic = ToolSpec::Agentic(container);
        assert!(spec_references(&agentic, named));
        assert!(!spec_references(&agentic, other));
        // an agentic tool backed by Thorium's MCP server names no tool
        let mcp = ToolSpec::Agentic(crate::models::AgenticTool::default());
        assert!(!spec_references(&mcp, named));
    }
}
