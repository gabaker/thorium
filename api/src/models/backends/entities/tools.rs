//! Backend logic for tools and tool instances
//!
//! Every tool instance is created by one shared publish path, whether it's a tool's first
//! instance, a new version published by updating a tool, or an instance created directly. The
//! path checks the spec, computes its hash, refuses duplicate labels and no-op versions, and
//! writes the instance, then its tool (moving `latest`), then the `PublishedFrom` edge between
//! them, so a failed write only ever leaves an unreferenced instance behind. A publish is a
//! validating half, which runs every check and builds what would be written, and a writing half,
//! which writes it.
//!
//! Entity updates, deletes, and instance creates of a tool or one of its instances hold the
//! tool's lock and read what they change after taking it, so publishes, edits, group changes,
//! deprecations, and deletes never act on each other's stale copies. An update reads its form
//! and uploads any image before taking the lock, so a slow request never holds it while its
//! body arrives. Tag changes take no lock.

use chrono::{DateTime, Utc};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::time::Duration;
use tracing::{Level, event, instrument};
use uuid::Uuid;

use crate::models::backends::db;
use crate::models::entities::digests::ThorDigest;
use crate::models::{
    AgenticToolBacking, AssociationKind, AssociationListOpts, AssociationTarget,
    AssociationTargetColumn, CrawlLimits, Directionality, Entity, EntityKinds, EntityMetadata,
    EntityMetadataForm, EntityMetadataUpdateForm, EntityResponse, Event, Group, GroupAllowAction,
    HarnessEnv, HarnessSpec, HarnessToolAccess, NetworkPolicy, NetworkPolicyUpdate, SystemSettings,
    ToolEntity, ToolEntityRequest, ToolInstanceEntity, ToolInstanceEntityRequest,
    ToolInstanceSummary, ToolKind, ToolPortabilityReport, ToolSpec, ToolVersion, User,
    WorkflowGraphChange, WorkloadEnvironment, WorkloadSpec,
};
use crate::utils::{ApiError, Shared, helpers};
use crate::{bad, bad_internal, conflict, internal_err, not_found, unauthorized};

/// How long a write to a tool waits for another write to the same tool to finish
///
/// Writes hold the lock well under the 30 seconds it lasts. A holder that died leaves its lock
/// to expire, and a request still waiting then takes the expired lock.
const PUBLISH_LOCK_WAIT: Duration = Duration::from_secs(20);

/// A tool spec that passed every check and the hash it publishes under
pub(crate) struct PreparedInstance {
    /// The parsed version label
    version: ToolVersion,
    /// The parsed and checked spec
    spec: ToolSpec,
    /// The spec's content hash
    spec_hash: String,
    /// The checked native tool instances a harness instance provides, sorted by id
    provides: Vec<Entity>,
}

/// Map a model validation message to a `400`
///
/// # Arguments
///
/// * `result` - The result of a model validation
fn bad_request<T>(result: Result<T, String>) -> Result<T, ApiError> {
    // turn any validation message into a bad request
    match result {
        Ok(value) => Ok(value),
        Err(msg) => bad!(msg),
    }
}

/// Map a digest failure to an internal error
///
/// # Arguments
///
/// * `result` - The result of a digest computation
fn digest_result<T>(result: Result<T, serde_json::Error>) -> Result<T, ApiError> {
    // digests of typed values never fail unless something is broken
    match result {
        Ok(value) => Ok(value),
        Err(err) => internal_err!(format!("Failed to hash a tool spec: {err}")),
    }
}

/// Get the root whose lock guards writes to an entity
///
/// A tool, toolset, or workflow guards itself and its instances, an agent, model endpoint, model
/// profile, or model credential guards itself, and entities of other kinds take no lock.
///
/// # Arguments
///
/// * `entity` - The entity being written
#[must_use]
pub(crate) fn lock_root(entity: &Entity) -> Option<Uuid> {
    // tools, toolsets, and workflows share their lock with their instances, and agent, toolset,
    // and workflow writes are serialized so a concurrent settings edit and edge change never drop
    // each other's changes; model endpoint, profile, and credential writes are serialized so a
    // refresh report, a ban change, and a hand edit never drop each other's changes
    match &entity.metadata {
        EntityMetadata::Tool(_)
        | EntityMetadata::Agent(_)
        | EntityMetadata::ToolSet(_)
        | EntityMetadata::WorkflowCatalog(_)
        | EntityMetadata::ModelEndpoint(_)
        | EntityMetadata::ModelProfile(_)
        | EntityMetadata::ModelCredential(_) => Some(entity.id),
        EntityMetadata::ToolInstance(instance) => Some(instance.tool),
        EntityMetadata::ToolSetInstance(instance) => Some(instance.tool_set),
        EntityMetadata::WorkflowInstance(instance) => Some(instance.workflow),
        _ => None,
    }
}

/// Take a tool's lock, waiting for another write to the tool to finish
///
/// Returns the token that releases the lock.
///
/// # Arguments
///
/// * `tool` - The id of the tool to lock
/// * `name` - The name of the entity being written, for the refusal
/// * `shared` - Shared Thorium objects
pub(crate) async fn lock(tool: Uuid, name: &str, shared: &Shared) -> Result<String, ApiError> {
    // wait for any other write to this tool
    db::entities::lock_instances(tool, name, PUBLISH_LOCK_WAIT, shared).await
}

/// Release a tool's lock
///
/// # Arguments
///
/// * `tool` - The id of the tool to unlock
/// * `token` - The token the lock was taken with
/// * `shared` - Shared Thorium objects
pub(crate) async fn unlock(tool: Uuid, token: &str, shared: &Shared) -> Result<(), ApiError> {
    // release the lock if it's still ours
    db::entities::unlock_instances(tool, token, shared).await
}

/// Release a root's lock once the write it guarded is done, logging a failed release
///
/// The lock expires on its own, so a failed release only delays the next write to the root and
/// never fails a write that already happened.
///
/// # Arguments
///
/// * `root` - The id of the root to unlock
/// * `token` - The token the lock was taken with
/// * `shared` - Shared Thorium objects
pub(crate) async fn unlock_after_write(root: Uuid, token: &str, shared: &Shared) {
    // log a failed release instead of failing the write
    if let Err(error) = unlock(root, token, shared).await {
        event!(
            Level::WARN,
            msg = "Failed to release a lock",
            root = root.to_string(),
            error = format!("{error:?}")
        );
    }
}

/// Check that a user may see every group they're told is missing, naming only those they can see
///
/// Returns a `401` without names when every missing group is hidden from the user.
///
/// # Arguments
///
/// * `user` - The user to name groups for
/// * `missing` - The missing groups
pub(crate) fn visible_groups(user: &User, missing: &[String]) -> Result<Vec<String>, ApiError> {
    // keep only the groups this user can see
    let visible = missing
        .iter()
        .filter(|group| user.is_admin() || user.groups.contains(*group))
        .cloned()
        .collect::<Vec<String>>();
    // a refusal caused only by hidden groups names nothing
    if visible.is_empty() {
        return unauthorized!();
    }
    Ok(visible)
}

/// Check the Image tool instance a container backing pins and get its spec hash
///
/// # Arguments
///
/// * `user` - The user publishing the agentic tool
/// * `tool_groups` - Every group of the agentic tool
/// * `pinned` - The id of the pinned tool instance
/// * `shared` - Shared Thorium objects
async fn check_pinned(
    user: &User,
    tool_groups: &[String],
    pinned: Uuid,
    shared: &Shared,
) -> Result<String, ApiError> {
    // a missing instance and one the user can't see get the same answer
    let missing = format!("tool instance `{pinned}` doesn't exist or you can't see it");
    let Ok(entity) = Entity::get(user, pinned, shared).await else {
        return bad!(missing);
    };
    // the pinned entity must be a tool instance
    let EntityMetadata::ToolInstance(instance) = &entity.metadata else {
        return bad!(missing);
    };
    // only Image tool instances can implement a container backing
    if instance.spec.kind() != ToolKind::Image {
        return bad!(format!(
            "tool instance `{}` is a {} tool instance, not an Image tool instance",
            entity.name,
            instance.spec.kind()
        ));
    }
    // a deprecated instance can't be newly pinned
    if instance.deprecated {
        return bad!(format!(
            "tool instance `{}` is deprecated and can't be pinned",
            entity.name
        ));
    }
    // the pinned instance must be in every group of the agentic tool
    let pinned_groups = db::entities::all_groups(pinned, shared).await?;
    let missing_groups = helpers::missing_groups(tool_groups, &pinned_groups);
    if !missing_groups.is_empty() {
        // name only the missing groups this user can see
        let visible = visible_groups(user, &missing_groups)?;
        return bad!(format!(
            "tool instance `{}` is not in the tool's groups {visible:?}",
            entity.name
        ));
    }
    Ok(instance.spec_hash.clone())
}

/// Check the native tool instances a harness instance provides
///
/// Each must be an existing Agentic tool instance with a `HarnessNative` backing that the user
/// can see, isn't deprecated, and is in every group of the harness. Returns them sorted by id.
///
/// # Arguments
///
/// * `user` - The user publishing the harness instance
/// * `kind` - The kind of the tool the instance is for
/// * `tool_groups` - Every group of the harness tool
/// * `provides` - The requested native tool instances
/// * `shared` - Shared Thorium objects
async fn check_provides(
    user: &User,
    kind: ToolKind,
    tool_groups: &[String],
    provides: &[Uuid],
    shared: &Shared,
) -> Result<Vec<Entity>, ApiError> {
    // nothing is provided without targets
    if provides.is_empty() {
        return Ok(Vec::new());
    }
    // only a harness has native tools built in
    if kind != ToolKind::Harness {
        return bad!(format!(
            "tool_provides is only valid for Harness tools, not {kind} tools"
        ));
    }
    // each native tool is provided once, and no more than an agent could ever be given
    let unique = provides.iter().copied().collect::<BTreeSet<Uuid>>();
    let crawl = CrawlLimits::from(&shared.config.thorium.events);
    let max = crawl.max_listed_tools_per_agent();
    if unique.len() > max {
        return bad!(format!(
            "a harness instance can provide at most {max} native tools (twice max_tools_per_agent ({})), not {}",
            crawl.max_tools_per_agent,
            unique.len()
        ));
    }
    // check each native tool instance
    let mut found = Vec::with_capacity(unique.len());
    for id in unique {
        // a missing instance and one the user can't see get the same answer
        let missing = format!("tool instance `{id}` doesn't exist or you can't see it");
        let Ok(entity) = Entity::get(user, id, shared).await else {
            return bad!(missing);
        };
        let EntityMetadata::ToolInstance(instance) = &entity.metadata else {
            return bad!(missing);
        };
        // only a native agentic tool can be built into a harness
        if !matches!(
            &instance.spec,
            ToolSpec::Agentic(agentic)
                if matches!(agentic.backing, AgenticToolBacking::HarnessNative { .. })
        ) {
            return bad!(format!(
                "tool instance `{}` isn't an Agentic tool instance with a HarnessNative backing, so a harness can't provide it",
                entity.name
            ));
        }
        // a deprecated instance can't be newly provided
        if instance.deprecated {
            return bad!(format!(
                "tool instance `{}` is deprecated and can't be provided",
                entity.name
            ));
        }
        // the native tool must be in every group of the harness
        let native_groups = db::entities::all_groups(id, shared).await?;
        let missing_groups = helpers::missing_groups(tool_groups, &native_groups);
        if !missing_groups.is_empty() {
            // name only the missing groups this user can see
            let visible = visible_groups(user, &missing_groups)?;
            return bad!(format!(
                "tool instance `{}` is not in the harness's groups {visible:?}",
                entity.name
            ));
        }
        found.push(entity);
    }
    Ok(found)
}

/// Get the stored spec hash of a tool instance entity
///
/// # Arguments
///
/// * `entity` - The tool instance
fn instance_spec_hash(entity: &Entity) -> &str {
    // only tool instances have a spec hash
    match &entity.metadata {
        EntityMetadata::ToolInstance(instance) => &instance.spec_hash,
        _ => "",
    }
}

/// Check that every id names a tool instance a user can see
///
/// Reads all of the ids in one lookup, so a request naming many tool instances costs one query.
/// A deprecated tool instance is still a tool instance and passes.
///
/// # Arguments
///
/// * `user` - The user naming the tool instances
/// * `ids` - The ids that must be tool instances
/// * `shared` - Shared Thorium objects
#[instrument(name = "tools::check_tool_instances", skip(user, shared), err(Debug))]
pub(crate) async fn check_tool_instances(
    user: &User,
    ids: &[Uuid],
    shared: &Shared,
) -> Result<(), ApiError> {
    // there is nothing to check without ids
    if ids.is_empty() {
        return Ok(());
    }
    // read each distinct id once
    let unique = ids.iter().copied().collect::<BTreeSet<Uuid>>();
    let unique = unique.into_iter().collect::<Vec<Uuid>>();
    // get the group and kind of every row of these entities
    let rows = db::entities::groups_and_kinds(&unique, shared).await?;
    // keep the tool instances this user can see; admins see every group
    let is_admin = user.is_admin();
    let visible = rows
        .iter()
        .filter(|row| row.kind == EntityKinds::ToolInstance)
        .filter(|row| is_admin || user.groups.contains(&row.group))
        .map(|row| row.id)
        .collect::<HashSet<Uuid>>();
    // a missing entity, another kind, and an instance the user can't see get the same answer
    if let Some(missing) = unique.iter().find(|id| !visible.contains(id)) {
        return bad!(format!(
            "tool instance `{missing}` doesn't exist or you can't see it"
        ));
    }
    Ok(())
}

/// Check the settings of a workload that only some publishers may use
///
/// A non-default security context needs an admin, and volumes must pass the host path
/// whitelist, as they do when an image is created.
///
/// # Arguments
///
/// * `user` - The user publishing the workload
/// * `workload` - The workload being published
/// * `settings` - The Thorium system settings
fn check_workload_privileges(
    user: &User,
    workload: &WorkloadSpec,
    settings: &SystemSettings,
) -> Result<(), ApiError> {
    // get the security context and volumes this environment sets
    let (security_context, volumes) = match &workload.environment {
        WorkloadEnvironment::K8s(env) => (Some(&env.security_context), env.volumes.as_slice()),
        WorkloadEnvironment::BareMetal(env) => (Some(&env.security_context), [].as_slice()),
        _ => (None, [].as_slice()),
    };
    // a non-default security context needs an admin publisher
    if security_context.is_some_and(|context| *context != crate::models::SecurityContext::default())
        && !user.is_admin()
    {
        return bad!(
            "only an admin can publish a workload with a non-default security_context".to_owned()
        );
    }
    // every volume must pass the same checks as an image's
    for volume in volumes {
        volume.validate(user, settings)?;
    }
    Ok(())
}

/// Check that every tool a workload's dependency settings name is a tool a user can see
///
/// Reads all of the ids in one lookup. A missing entity, an entity of another kind (a tool
/// instance included), and a tool the user can't see get the same answer.
///
/// # Arguments
///
/// * `user` - The user publishing the workload
/// * `workload` - The workload being published
/// * `shared` - Shared Thorium objects
#[instrument(name = "tools::check_dependency_tools", skip_all, err(Debug))]
async fn check_dependency_tools(
    user: &User,
    workload: &WorkloadSpec,
    shared: &Shared,
) -> Result<(), ApiError> {
    // get each listed tool once
    let ids = workload.data.dependencies.tools();
    if ids.is_empty() {
        return Ok(());
    }
    // get the group and kind of every row of these entities
    let rows = db::entities::groups_and_kinds(&ids, shared).await?;
    // keep the tools this user can see; admins see every group
    let is_admin = user.is_admin();
    let visible = rows
        .iter()
        .filter(|row| row.kind == EntityKinds::Tool)
        .filter(|row| is_admin || user.groups.contains(&row.group))
        .map(|row| row.id)
        .collect::<HashSet<Uuid>>();
    // refuse the first id that isn't a visible tool
    if let Some(missing) = ids.iter().find(|id| !visible.contains(id)) {
        return bad!(format!(
            "dependency tool `{missing}` doesn't exist, isn't a tool, or you can't see it"
        ));
    }
    Ok(())
}

/// Check a requested version and spec for a tool and compute the spec's hash
///
/// # Arguments
///
/// * `user` - The user publishing the instance
/// * `kind` - The kind of the tool the instance is for
/// * `tool_groups` - Every group of the tool
/// * `version` - The requested version label
/// * `spec` - The requested JSON encoded spec
/// * `provides` - The native tool instances a harness instance provides
/// * `shared` - Shared Thorium objects
#[instrument(name = "tools::prepare", skip_all, err(Debug))]
pub(crate) async fn prepare(
    user: &User,
    kind: ToolKind,
    tool_groups: &[String],
    version: &str,
    spec: &str,
    provides: &[Uuid],
    shared: &Shared,
) -> Result<PreparedInstance, ApiError> {
    // parse the version label
    let version = bad_request(ToolVersion::parse_label(version))?;
    // parse the typed spec
    let mut spec = bad_request(ToolSpec::parse_request(spec))?;
    // a default tool access block is stored unset, so it hashes like one left out
    if let ToolSpec::Harness(harness) = &mut spec
        && harness.interface.tool_access == Some(HarnessToolAccess::default())
    {
        harness.interface.tool_access = None;
    }
    // the spec must be of the tool's kind
    if spec.kind() != kind {
        return bad!(format!(
            "a {} spec can't be published for a {kind} tool",
            spec.kind()
        ));
    }
    // check everything that needs nothing outside of the spec
    bad_request(spec.validate(shared.config.thorium.entities.max_tool_schema_bytes))?;
    // check the parts of each kind that read other data
    let pinned_spec_hash = match &spec {
        ToolSpec::Agentic(agentic) => match &agentic.backing {
            AgenticToolBacking::ThoriumMcp { tool } => {
                // the MCP tool must be one Thorium's MCP server exposes
                if !crate::routes::mcp::ThoriumMCP::tool_names().contains(tool) {
                    return bad!(format!("`{tool}` is not a tool on Thorium's MCP server"));
                }
                None
            }
            AgenticToolBacking::Container(container) => {
                // the pinned Image instance must be usable by this tool
                Some(check_pinned(user, tool_groups, container.tool_instance, shared).await?)
            }
            // which harness provides a native tool is checked when a workflow is instanced
            AgenticToolBacking::HarnessNative { .. } => None,
        },
        ToolSpec::Image(workload) | ToolSpec::Harness(HarnessSpec { workload, .. }) => {
            // get the settings that limit workloads
            let settings = db::system::get_settings(shared).await?;
            // check the settings only some publishers may use
            check_workload_privileges(user, workload, &settings)?;
            // every tool its dependency settings name must be a tool the publisher can see
            check_dependency_tools(user, workload, shared).await?;
            None
        }
    };
    // check the native tools a harness instance provides
    let provides = check_provides(user, kind, tool_groups, provides, shared).await?;
    let provided_hashes = provides
        .iter()
        .map(instance_spec_hash)
        .collect::<Vec<&str>>();
    // hash the typed spec with the native tools it provides
    let spec_hash = digest_result(ThorDigest::provided_tool_spec(
        &spec,
        pinned_spec_hash.as_deref(),
        &provided_hashes,
    ))?;
    Ok(PreparedInstance {
        version,
        spec,
        spec_hash,
        provides,
    })
}

/// Check whether a candidate spec hash equals the hash of a tool's latest instance
///
/// A latest instance hashed under an older scheme is rehashed from its stored spec first.
///
/// # Arguments
///
/// * `scheme` - The active digest scheme
/// * `latest` - The latest instance's stored hash, spec, and pinned instance hash, if any
/// * `candidate` - The candidate hash under the active scheme
pub(crate) fn is_noop(
    scheme: &str,
    latest: Option<(&str, &ToolSpec, Option<&str>)>,
    candidate: &str,
) -> Result<bool, serde_json::Error> {
    // a tool with no latest instance can't get a no-op version
    let Some((stored, spec, pinned)) = latest else {
        return Ok(false);
    };
    // compare under the active scheme, rehashing an older stored hash
    ThorDigest::same_definition_with_scheme(
        scheme,
        stored,
        || ThorDigest::tool_spec_with_scheme(scheme, spec, pinned),
        candidate,
    )
}

/// Get the ids of a tool's instances from its `PublishedFrom` edges in some groups
///
/// # Arguments
///
/// * `user` - The user reading the instances
/// * `tool` - The id of the tool
/// * `groups` - The groups to read the edges in
/// * `shared` - Shared Thorium objects
async fn instance_ids(
    user: &User,
    tool: Uuid,
    groups: &[String],
    shared: &Shared,
) -> Result<Vec<Uuid>, ApiError> {
    // only instances published from this tool point at it
    edge_ids(
        user,
        tool,
        groups,
        (AssociationKind::PublishedFrom, Directionality::From),
        shared,
    )
    .await
}

/// Get the native tool instances a harness instance provides from its `Provides` edges in the
/// groups a user can see it in, sorted
///
/// # Arguments
///
/// * `user` - The user reading the harness instance
/// * `instance` - The harness instance
/// * `shared` - Shared Thorium objects
pub(crate) async fn provided_ids(
    user: &User,
    instance: &Entity,
    shared: &Shared,
) -> Result<Vec<Uuid>, ApiError> {
    // only a harness instance provides native tools
    let is_harness = matches!(
        &instance.metadata,
        EntityMetadata::ToolInstance(meta) if meta.spec.kind() == ToolKind::Harness
    );
    if !is_harness {
        return Ok(Vec::new());
    }
    // read the edges leaving the instance
    let mut ids = edge_ids(
        user,
        instance.id,
        &instance.groups,
        (AssociationKind::Provides, Directionality::To),
        shared,
    )
    .await?;
    ids.sort_unstable();
    Ok(ids)
}

/// Get the entities at the other end of an entity's edges of one kind and direction in some
/// groups, each once
///
/// # Arguments
///
/// * `user` - The user reading the edges
/// * `id` - The entity whose edges are read
/// * `groups` - The groups to read the edges in
/// * `wanted` - The kind and direction of the edges to keep
/// * `shared` - Shared Thorium objects
async fn edge_ids(
    user: &User,
    id: Uuid,
    groups: &[String],
    wanted: (AssociationKind, Directionality),
    shared: &Shared,
) -> Result<Vec<Uuid>, ApiError> {
    // list the entity's edges in these groups
    let source = AssociationTargetColumn::Entity(id);
    let opts = AssociationListOpts::default()
        .groups(groups.to_vec())
        .limit(500);
    let mut cursor = db::associations::list(user, opts, &source, shared).await?;
    // collect each entity at the other end of a wanted edge
    let mut ids = Vec::new();
    loop {
        // keep the edges of the wanted kind and direction
        for assoc in cursor.data.drain(..) {
            // skip every other kind and direction
            if assoc.kind == wanted.0 && assoc.direction == wanted.1 {
                // get the entity at the other end
                if let AssociationTargetColumn::Entity(other) = assoc.deserialize_other()? {
                    // add each entity once
                    if !ids.contains(&other) {
                        ids.push(other);
                    }
                }
            }
        }
        // stop once every page was read
        if cursor.exhausted() {
            break;
        }
        // get the next page of edges
        cursor.next(shared).await?;
    }
    Ok(ids)
}

/// Get every instance of a tool in some groups, newest first
///
/// # Arguments
///
/// * `user` - The user reading the instances
/// * `tool` - The id of the tool
/// * `groups` - The groups to read the instances in
/// * `shared` - Shared Thorium objects
pub(crate) async fn instances(
    user: &User,
    tool: Uuid,
    groups: &[String],
    shared: &Shared,
) -> Result<Vec<Entity>, ApiError> {
    // get the ids of the tool's instances
    let ids = instance_ids(user, tool, groups, shared).await?;
    // there is nothing to read without instances
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    // get the instances, which come back newest first
    let mut entities = db::entities::get_many(groups, &ids, shared).await?;
    // keep the creation order stable for instances created in the same millisecond
    entities.sort_by_key(|entity| std::cmp::Reverse(entity.created));
    Ok(entities)
}

/// Summarize a tool's instances, newest first
///
/// # Arguments
///
/// * `instances` - The tool's instances, newest first
#[must_use]
pub(crate) fn summarize(instances: &[Entity]) -> Vec<ToolInstanceSummary> {
    // summarize each instance in order
    instances
        .iter()
        .filter_map(|entity| match &entity.metadata {
            EntityMetadata::ToolInstance(instance) => Some(ToolInstanceSummary {
                id: entity.id,
                version: instance.version.clone(),
                spec_hash: instance.spec_hash.clone(),
                deprecated: instance.deprecated,
                created: entity.created,
            }),
            _ => None,
        })
        .collect()
}

/// Find a tool's latest instance: its newest instance that isn't deprecated
///
/// # Arguments
///
/// * `instances` - The tool's instances, newest first
#[must_use]
pub(crate) fn find_latest(instances: &[Entity]) -> Option<(Uuid, ToolVersion, String)> {
    // the first instance that isn't deprecated is the latest
    instances.iter().find_map(|entity| match &entity.metadata {
        EntityMetadata::ToolInstance(instance) if !instance.deprecated => Some((
            entity.id,
            instance.version.clone(),
            instance.spec_hash.clone(),
        )),
        _ => None,
    })
}

/// Get a tool's metadata or fail if an entity isn't a tool
///
/// # Arguments
///
/// * `tool` - The entity to get tool metadata from
fn tool_meta(tool: &mut Entity) -> Result<&mut ToolEntity, ApiError> {
    // only tools have tool metadata
    match &mut tool.metadata {
        EntityMetadata::Tool(meta) => Ok(meta),
        _ => bad!(format!("`{}` is not a tool", tool.name)),
    }
}

/// Read a tool's metadata or fail if an entity isn't a tool
///
/// # Arguments
///
/// * `tool` - The entity to read tool metadata from
fn read_tool_meta(tool: &Entity) -> Result<&ToolEntity, ApiError> {
    // only tools have tool metadata
    match &tool.metadata {
        EntityMetadata::Tool(meta) => Ok(meta),
        _ => bad!(format!("`{}` is not a tool", tool.name)),
    }
}

/// Make sure a new version's label is unused and its spec isn't identical to the latest's
///
/// # Arguments
///
/// * `tool` - The tool to publish an instance of, in all of its groups
/// * `prepared` - The checked version and spec
/// * `existing` - The tool's existing instances, newest first
/// * `shared` - Shared Thorium objects
async fn check_new_version(
    tool: &Entity,
    prepared: &PreparedInstance,
    existing: &[Entity],
    shared: &Shared,
) -> Result<(), ApiError> {
    // version labels are unique within a tool
    let used = existing.iter().any(|entity| match &entity.metadata {
        EntityMetadata::ToolInstance(instance) => instance.version == prepared.version,
        _ => false,
    });
    if used {
        return conflict!(format!(
            "version `{}` already exists for tool `{}`",
            prepared.version, tool.name
        ));
    }
    // find the latest instance to compare the spec against
    let latest_id = read_tool_meta(tool)?.latest;
    let latest = existing.iter().find(|entity| Some(entity.id) == latest_id);
    // get the latest instance's hash, spec, and the hash of the instance it pins
    let latest_parts = match latest.map(|entity| &entity.metadata) {
        Some(EntityMetadata::ToolInstance(instance)) => {
            // an older scheme's hash is rehashed with the pinned instance's current hash
            let pinned = if ThorDigest::scheme(&instance.spec_hash) == Some(ThorDigest::SCHEME) {
                None
            } else {
                match instance.spec.pinned_instance() {
                    Some(pinned) => Some(pinned_hash(pinned, &tool.groups, shared).await?),
                    None => None,
                }
            };
            Some((instance.spec_hash.clone(), instance.spec.clone(), pinned))
        }
        _ => None,
    };
    // a spec identical to the latest instance's is a no-op version
    let noop = digest_result(is_noop(
        ThorDigest::SCHEME,
        latest_parts
            .as_ref()
            .map(|(hash, spec, pinned)| (hash.as_str(), spec, pinned.as_deref())),
        &prepared.spec_hash,
    ))?;
    if noop {
        // name the latest version this spec is identical to
        let latest_version = read_tool_meta(tool)?
            .latest_version
            .as_ref()
            .map_or_else(String::new, ToString::to_string);
        return conflict!(format!(
            "the spec is identical to the latest version `{latest_version}` of tool `{}`",
            tool.name
        ));
    }
    Ok(())
}

/// Everything a publish writes, built by its validating half
pub(crate) struct PublishPlan {
    /// The new tool instance
    instance: Entity,
    /// The checked native tool instances a harness instance provides, sorted by id
    provides: Vec<Entity>,
    /// The instance the new one supersedes
    previous: Option<Uuid>,
    /// The new instance's version label, which becomes the tool's latest
    version: ToolVersion,
    /// The new instance's spec hash, which becomes the tool's latest
    spec_hash: String,
    /// When the instance is published
    now: DateTime<Utc>,
}

/// Run every check of a publish and build what it would write, writing nothing
///
/// # Arguments
///
/// * `user` - The user publishing the instance
/// * `tool` - The tool to publish an instance of, in all of its groups
/// * `prepared` - The checked version and spec
/// * `existing` - The tool's existing instances, newest first
/// * `shared` - Shared Thorium objects
#[instrument(name = "tools::validate_publish", skip_all, fields(tool = tool.id.to_string()), err(Debug))]
pub(crate) async fn validate_publish(
    user: &User,
    tool: &Entity,
    prepared: PreparedInstance,
    existing: &[Entity],
    shared: &Shared,
) -> Result<PublishPlan, ApiError> {
    // the label must be new and the spec must differ from the latest instance's
    check_new_version(tool, &prepared, existing, shared).await?;
    // the new instance supersedes the most recently created one
    let previous = existing.first().map(|entity| entity.id);
    // build the new instance in every group of its tool
    let now = Utc::now();
    let name = format!("{}@{}", tool.name, prepared.version);
    // tests may choose the new instance's id
    #[cfg(feature = "test-utilities")]
    let id = crate::test_utilities::faults::take_create_id(&name).unwrap_or_else(Uuid::new_v4);
    #[cfg(not(feature = "test-utilities"))]
    let id = Uuid::new_v4();
    let instance = Entity {
        id,
        name,
        kind: EntityKinds::ToolInstance,
        metadata: EntityMetadata::ToolInstance(ToolInstanceEntity {
            tool: tool.id,
            version: prepared.version.clone(),
            previous,
            spec_hash: prepared.spec_hash.clone(),
            deprecated: false,
            spec: prepared.spec,
            portability: None,
            provides: Vec::new(),
        }),
        description: None,
        submitter: user.username.clone(),
        groups: tool.groups.clone(),
        tags: HashMap::default(),
        image: None,
        created: now,
    };
    Ok(PublishPlan {
        instance,
        provides: prepared.provides,
        previous,
        version: prepared.version,
        spec_hash: prepared.spec_hash,
        now,
    })
}

/// Publish a checked spec as a new instance of a tool
///
/// Runs the publish's validating half, then its writing half. The caller holds the tool's
/// instance lock when the tool already exists.
///
/// # Arguments
///
/// * `user` - The user publishing the instance
/// * `tool` - The tool to publish an instance of, in all of its groups
/// * `prepared` - The checked version and spec
/// * `existing` - The tool's existing instances, newest first
/// * `tags` - The user tags to give the new instance
/// * `new_tool_tags` - The user tags of the tool when the tool itself still needs to be created
/// * `shared` - Shared Thorium objects
#[instrument(name = "tools::publish", skip_all, fields(tool = tool.id.to_string()), err(Debug))]
pub(crate) async fn publish(
    user: &User,
    tool: &mut Entity,
    prepared: PreparedInstance,
    existing: &[Entity],
    tags: HashMap<String, HashSet<String>>,
    new_tool_tags: Option<HashMap<String, HashSet<String>>>,
    shared: &Shared,
) -> Result<Entity, ApiError> {
    // check everything and build what would be written
    let plan = validate_publish(user, tool, prepared, existing, shared).await?;
    // write it
    Box::pin(write_publish(user, tool, plan, tags, new_tool_tags, shared)).await
}

/// Write a validated publish
///
/// The instance is written first, then the tool with its new `latest`, then the
/// `PublishedFrom` edge from the instance to the tool. The caller holds the tool's instance
/// lock when the tool already exists.
///
/// # Arguments
///
/// * `user` - The user publishing the instance
/// * `tool` - The tool to publish an instance of, in all of its groups
/// * `plan` - What the publish's validating half built
/// * `tags` - The user tags to give the new instance
/// * `new_tool_tags` - The user tags of the tool when the tool itself still needs to be created
/// * `shared` - Shared Thorium objects
#[instrument(name = "tools::write_publish", skip_all, fields(tool = tool.id.to_string()), err(Debug))]
pub(crate) async fn write_publish(
    user: &User,
    tool: &mut Entity,
    plan: PublishPlan,
    tags: HashMap<String, HashSet<String>>,
    new_tool_tags: Option<HashMap<String, HashSet<String>>>,
    shared: &Shared,
) -> Result<Entity, ApiError> {
    // take the parts of the plan
    let PublishPlan {
        instance,
        provides,
        previous,
        version,
        spec_hash,
        now,
    } = plan;
    // write the instance first, since nothing reads it until the edge exists
    db::entities::insert(user, &instance, tags, shared).await?;
    // a new workload starts from its previous version's runtime
    if matches!(
        &instance.metadata,
        EntityMetadata::ToolInstance(ToolInstanceEntity {
            spec: ToolSpec::Image(_) | ToolSpec::Harness(_),
            ..
        })
    ) {
        db::jobs::seed_workload_runtime(instance.id, previous, shared).await?;
    }
    // link a harness instance to its native tools before anything reads it
    write_provides(user, &instance, &provides, &tool.groups, shared).await?;
    // fail here when a test asked for a storage failure after the instance write
    #[cfg(feature = "test-utilities")]
    if new_tool_tags.is_some()
        && crate::test_utilities::faults::take_instance_write_failure(&tool.name)
    {
        return internal_err!(format!(
            "injected failure after writing an instance of `{}`",
            tool.name
        ));
    }
    // move the tool's latest to the new instance
    let meta = tool_meta(tool)?;
    meta.latest = Some(instance.id);
    meta.latest_version = Some(version);
    meta.latest_spec_hash = Some(spec_hash);
    meta.modified = now;
    // write the tool, which makes the instance its latest
    match new_tool_tags {
        Some(tool_tags) => db::entities::insert(user, tool, tool_tags, shared).await?,
        None => db::entities::update(user, tool.clone(), &[], &[], shared).await?,
    }
    // link the instance to its tool in every group, which lists it as one of the tool's instances
    let source = AssociationTarget::Entity {
        id: instance.id,
        name: instance.name.clone(),
    };
    let target = AssociationTarget::Entity {
        id: tool.id,
        name: tool.name.clone(),
    };
    db::associations::create(
        user,
        tool.groups.len(),
        AssociationKind::PublishedFrom,
        source,
        &vec![(target, tool.groups.clone())],
        None,
        Directionality::To,
        shared,
    )
    .await?;
    // the tool has a new latest instance
    let change = WorkflowGraphChange::ToolUpdated {
        tool: tool.id,
        tool_instance: instance.id,
    };
    Event::queue_workflow_graph(user, &tool.groups, change, shared).await?;
    Ok(instance)
}

/// Write the `Provides` edges from a new harness instance to its native tools in every group
///
/// These edges queue no event: the harness instance's own `ToolUpdated` covers them.
///
/// # Arguments
///
/// * `user` - The user publishing the instance
/// * `instance` - The new harness instance
/// * `provides` - The checked native tool instances
/// * `groups` - Every group of the harness tool
/// * `shared` - Shared Thorium objects
async fn write_provides(
    user: &User,
    instance: &Entity,
    provides: &[Entity],
    groups: &[String],
    shared: &Shared,
) -> Result<(), ApiError> {
    // a harness without native tools has no edges to write
    if provides.is_empty() {
        return Ok(());
    }
    // link the instance to each native tool in every group of the harness
    let source = AssociationTarget::Entity {
        id: instance.id,
        name: instance.name.clone(),
    };
    let targets = provides
        .iter()
        .map(|native| {
            let target = AssociationTarget::Entity {
                id: native.id,
                name: native.name.clone(),
            };
            (target, groups.to_vec())
        })
        .collect::<Vec<(AssociationTarget, Vec<String>)>>();
    db::associations::create(
        user,
        targets.len() * groups.len(),
        AssociationKind::Provides,
        source,
        &targets,
        None,
        Directionality::To,
        shared,
    )
    .await
}

/// Get the stored spec hash of a pinned instance
///
/// # Arguments
///
/// * `pinned` - The id of the pinned instance
/// * `groups` - The groups to read the instance in
/// * `shared` - Shared Thorium objects
async fn pinned_hash(pinned: Uuid, groups: &[String], shared: &Shared) -> Result<String, ApiError> {
    // read the pinned instance
    let entity = db::entities::get(groups, pinned, shared).await?;
    // get its stored spec hash
    match entity.metadata {
        EntityMetadata::ToolInstance(instance) => Ok(instance.spec_hash),
        _ => internal_err!(format!("pinned entity {pinned} is not a tool instance")),
    }
}

/// Check the name and groups every created tool needs
///
/// # Arguments
///
/// * `name` - The requested name
/// * `groups` - The requested groups
pub(crate) fn check_name_and_groups(
    name: Option<&String>,
    groups: &[String],
) -> Result<String, ApiError> {
    // a tool needs a non-empty name
    let Some(name) = name.filter(|name| !name.is_empty()) else {
        return bad!("Entity must have a name!".to_owned());
    };
    // a tool needs at least one group
    if groups.is_empty() {
        return bad!("Entity must be in at least 1 group!".to_owned());
    }
    // group names can't be empty
    if groups.iter().any(String::is_empty) {
        return bad!(format!(
            "Entity cannot have any groups with empty names: {groups:?}"
        ));
    }
    Ok(name.clone())
}

/// Get the portability report of a tool instance's spec, if it's an agentic spec
///
/// # Arguments
///
/// * `instance` - The tool instance entity
fn instance_portability(instance: &Entity) -> Option<ToolPortabilityReport> {
    // only agentic tool instances have a report
    match &instance.metadata {
        EntityMetadata::ToolInstance(meta) => meta.spec.portability(),
        _ => None,
    }
}

/// Create a tool and its first instance from a submitted form
///
/// # Arguments
///
/// * `user` - The user creating the tool
/// * `id` - The id of the new tool
/// * `name` - The requested name
/// * `description` - The requested description
/// * `image` - The tool's uploaded image, if one was uploaded
/// * `groups` - The requested groups
/// * `metadata` - The submitted metadata form
/// * `tags` - The requested user tags
/// * `shared` - Shared Thorium objects
#[allow(clippy::too_many_arguments)]
#[instrument(name = "tools::create_tool", skip_all, err(Debug))]
pub(crate) async fn create_tool(
    user: &User,
    id: Uuid,
    name: Option<&String>,
    description: Option<String>,
    image: Option<String>,
    groups: Vec<String>,
    mut metadata: EntityMetadataForm,
    tags: HashMap<String, HashSet<String>>,
    shared: &Shared,
) -> Result<EntityResponse, ApiError> {
    // check the name and groups
    let name = check_name_and_groups(name, &groups)?;
    // a new name can't be the word for an entity kind
    crate::utils::bounder::entity_name(&name)?;
    // take the tool's fields from the form
    let req = ToolEntityRequest::from_form(&mut metadata)?;
    // build the tool, which has no latest instance until its first is published
    let now = Utc::now();
    let mut meta = ToolEntity::new(req.kind, now);
    // a harness gets its env and disabled models on the root
    if req.harness_env.is_some() || !req.disabled_models.is_empty() {
        // only a harness has a pod env or resolves models
        check_harness_kind(req.kind)?;
        meta.harness_env = req
            .harness_env
            .as_deref()
            .map(HarnessEnv::parse_request)
            .transpose()
            .map_err(|msg| bad_internal!(msg))?;
        meta.disabled_models = checked_model_ids(req.disabled_models)?;
    }
    let mut tool = Entity {
        id,
        name,
        kind: EntityKinds::Tool,
        metadata: EntityMetadata::Tool(meta),
        description,
        submitter: user.username.clone(),
        groups,
        tags: HashMap::default(),
        image,
        created: now,
    };
    // check the first instance before anything is written
    let prepared = prepare(
        user,
        req.kind,
        &tool.groups,
        &req.version,
        &req.spec,
        &req.provides,
        shared,
    )
    .await?;
    // publish the first instance, which also writes the tool with the requested tags
    let instance = Box::pin(publish(
        user,
        &mut tool,
        prepared,
        &[],
        HashMap::default(),
        Some(tags),
        shared,
    ))
    .await?;
    // respond with both ids and the new instance's portability report
    let mut resp = EntityResponse::new(tool.id, tool.name);
    resp.tool_instance = Some(instance.id);
    resp.portability = instance_portability(&instance);
    Ok(resp)
}

/// Create a tool instance directly for an existing tool from a submitted form
///
/// The instance goes through the same publish path as a tool update with a spec. Its groups are
/// always its tool's groups.
///
/// # Arguments
///
/// * `user` - The user creating the instance
/// * `metadata` - The submitted metadata form
/// * `tags` - The requested user tags
/// * `shared` - Shared Thorium objects
#[instrument(name = "tools::create_instance", skip_all, err(Debug))]
pub(crate) async fn create_instance(
    user: &User,
    mut metadata: EntityMetadataForm,
    tags: HashMap<String, HashSet<String>>,
    shared: &Shared,
) -> Result<EntityResponse, ApiError> {
    // take the instance's fields from the form
    let req = ToolInstanceEntityRequest::from_form(&mut metadata)?;
    // the tool must exist and be visible
    let mut tool = Entity::get(user, req.tool, shared).await?;
    // the tool must actually be a tool
    let kind = tool_meta(&mut tool)?.kind;
    // refuse a user who can't edit the tool in all of its groups before waiting on its lock
    tool.groups = db::entities::all_groups(tool.id, shared).await?;
    authorize_publish(user, &tool, shared).await?;
    // hold the tool's lock while it's read, checked, and published to
    let token = lock(tool.id, &tool.name, shared).await?;
    let result = Box::pin(locked_create_instance(
        user,
        tool.id,
        kind,
        Requested {
            version: &req.version,
            spec: &req.spec,
            provides: &req.provides,
        },
        tags,
        shared,
    ))
    .await;
    // release the lock whether or not the publish worked
    unlock_after_write(tool.id, &token, shared).await;
    // respond with the new instance and its portability report
    let instance = result?;
    let mut resp = EntityResponse::new(instance.id, instance.name.clone());
    resp.portability = instance_portability(&instance);
    Ok(resp)
}

/// Make sure a user can publish to a tool by editing it in every one of its groups
///
/// # Arguments
///
/// * `user` - The user publishing to the tool
/// * `tool` - The tool, in all of its groups
/// * `shared` - Shared Thorium objects
async fn authorize_publish(user: &User, tool: &Entity, shared: &Shared) -> Result<(), ApiError> {
    // the user must be able to edit the tool in every one of its groups
    Group::authorize_graph(
        user,
        &tool.groups,
        Group::editable,
        Some(GroupAllowAction::Entities),
        "edit",
        &tool.name,
        shared,
    )
    .await?;
    Ok(())
}

/// The version, spec, and native tools a request asks to publish
#[derive(Debug, Clone, Copy)]
struct Requested<'a> {
    /// The requested version label
    version: &'a str,
    /// The requested JSON encoded spec
    spec: &'a str,
    /// The native tool instances a harness instance provides
    provides: &'a [Uuid],
}

/// Read a tool in all of its groups and publish an instance of it while holding its lock
///
/// # Arguments
///
/// * `user` - The user creating the instance
/// * `id` - The id of the tool
/// * `kind` - The tool's kind
/// * `requested` - The requested version, spec, and native tools
/// * `tags` - The user tags to give the new instance
/// * `shared` - Shared Thorium objects
async fn locked_create_instance(
    user: &User,
    id: Uuid,
    kind: ToolKind,
    requested: Requested<'_>,
    tags: HashMap<String, HashSet<String>>,
    shared: &Shared,
) -> Result<Entity, ApiError> {
    // read the tool as the writes that finished before the lock left it
    let groups = db::entities::all_groups(id, shared).await?;
    let mut tool = db::entities::get(&groups, id, shared).await?;
    tool.groups = groups;
    // the user must still be able to edit the tool in every one of its groups
    authorize_publish(user, &tool, shared).await?;
    // check and publish the instance
    Box::pin(locked_publish(
        user, &mut tool, kind, requested, tags, shared,
    ))
    .await
}

/// Check and publish a new instance of an existing tool
///
/// The caller holds the tool's lock and read the tool after taking it.
///
/// # Arguments
///
/// * `user` - The user publishing the instance
/// * `tool` - The tool, in all of its groups
/// * `kind` - The tool's kind
/// * `requested` - The requested version, spec, and native tools
/// * `tags` - The user tags to give the new instance
/// * `shared` - Shared Thorium objects
async fn locked_publish(
    user: &User,
    tool: &mut Entity,
    kind: ToolKind,
    requested: Requested<'_>,
    tags: HashMap<String, HashSet<String>>,
    shared: &Shared,
) -> Result<Entity, ApiError> {
    // check the requested version, spec, and native tools
    let prepared = prepare(
        user,
        kind,
        &tool.groups,
        requested.version,
        requested.spec,
        requested.provides,
        shared,
    )
    .await?;
    // get the tool's existing instances
    let existing = instances(user, tool.id, &tool.groups, shared).await?;
    // publish the instance
    Box::pin(publish(user, tool, prepared, &existing, tags, None, shared)).await
}

/// Publish a new instance of a tool from a tool update
///
/// The caller holds the tool's lock and read the tool after taking it.
///
/// # Arguments
///
/// * `user` - The user updating the tool
/// * `tool` - The tool, in all of its groups
/// * `version` - The requested version label
/// * `spec` - The requested JSON encoded spec
/// * `provides` - The native tool instances a new harness instance provides
/// * `shared` - Shared Thorium objects
#[instrument(name = "tools::publish_update", skip_all, err(Debug))]
pub(crate) async fn publish_update(
    user: &User,
    tool: &mut Entity,
    version: Option<String>,
    spec: Option<String>,
    provides: Vec<Uuid>,
    shared: &Shared,
) -> Result<(), ApiError> {
    // a version and a spec are only published together, and native tools only with them
    let (version, spec) = match (version, spec) {
        (Some(version), Some(spec)) => (version, spec),
        (None, Some(_)) => {
            return bad!("tool_version is required to publish a tool_spec".to_owned());
        }
        (Some(_), None) => {
            return bad!("tool_spec is required to publish a tool_version".to_owned());
        }
        (None, None) if !provides.is_empty() => {
            return bad!(
                "tool_provides can only be set with the tool_version and tool_spec of a new harness version"
                    .to_owned()
            );
        }
        (None, None) => return Ok(()),
    };
    // get the tool's kind
    let kind = tool_meta(tool)?.kind;
    // check and publish the instance
    let requested = Requested {
        version: &version,
        spec: &spec,
        provides: &provides,
    };
    Box::pin(locked_publish(
        user,
        tool,
        kind,
        requested,
        HashMap::default(),
        shared,
    ))
    .await
    .map(|_| ())
}

/// Refuse a harness setting on a tool that isn't a harness
///
/// # Arguments
///
/// * `kind` - The tool's kind
fn check_harness_kind(kind: ToolKind) -> Result<(), ApiError> {
    // only harnesses have a pod env and resolve models
    if kind == ToolKind::Harness {
        Ok(())
    } else {
        bad!(format!(
            "tool_harness_env and the disabled models are only valid for Harness tools, not {kind} tools"
        ))
    }
}

/// Refuse an empty model id
///
/// # Arguments
///
/// * `ids` - The model ids to check
fn checked_model_ids(ids: BTreeSet<String>) -> Result<BTreeSet<String>, ApiError> {
    // a model id names a model, so it can't be empty
    if ids.iter().any(|id| id.trim().is_empty()) {
        return bad!("a disabled model id can't be empty".to_owned());
    }
    Ok(ids)
}

/// Apply the harness settings of a tool update to the tool's root
///
/// The env and the disabled models are read when each run starts, so changing them publishes no
/// instance and queues no event. The caller holds the tool's lock and writes the root.
///
/// # Arguments
///
/// * `tool` - The tool's root
/// * `form` - The metadata update form
pub(crate) fn update_harness_settings(
    tool: &mut ToolEntity,
    form: &mut EntityMetadataUpdateForm,
) -> Result<(), ApiError> {
    // an update without harness settings changes nothing here
    let sets_env = form.tool_harness_env.is_some() || form.clear_tool_harness_env == Some(true);
    if !sets_env && form.disable_tool_models.is_empty() && form.enable_tool_models.is_empty() {
        return Ok(());
    }
    // only a harness has these settings
    check_harness_kind(tool.kind)?;
    // a cleared env is replaced by any new one in the same update
    if form.clear_tool_harness_env == Some(true) {
        tool.harness_env = None;
    }
    if let Some(raw) = form.tool_harness_env.take() {
        let env = HarnessEnv::parse_request(&raw).map_err(|msg| bad_internal!(msg))?;
        tool.harness_env = Some(env);
    }
    // disable then enable the requested model ids
    let disable = checked_model_ids(std::mem::take(&mut form.disable_tool_models))?;
    tool.disabled_models.extend(disable);
    for model in std::mem::take(&mut form.enable_tool_models) {
        tool.disabled_models.remove(&model);
    }
    // the root was edited
    tool.modified = Utc::now();
    Ok(())
}

/// Queue a change of a tool's user tags, so the toolsets whose selector names a changed pair are
/// rebuilt
///
/// The change is queued only in the groups the tags changed in, so it never names a toolset in
/// a group the user can't see. Nothing is queued when no pair changed.
///
/// # Arguments
///
/// * `user` - The user who changed the tags
/// * `tool` - The id of the tool
/// * `tags` - The tag keys and values that were added or removed
/// * `groups` - The checked groups of the tag request
/// * `shared` - Shared Thorium objects
#[instrument(name = "tools::queue_tag_change", skip(user, tags, shared), err(Debug))]
pub(crate) async fn queue_tag_change(
    user: &User,
    tool: Uuid,
    tags: BTreeMap<String, BTreeSet<String>>,
    groups: &[String],
    shared: &Shared,
) -> Result<(), ApiError> {
    // a request that names no pair or no group changes nothing
    if tags.values().all(BTreeSet::is_empty) || groups.is_empty() {
        return Ok(());
    }
    // queue the change in the groups of the request
    let change = WorkflowGraphChange::ToolTagsChanged { tool, tags };
    Event::queue_workflow_graph(user, groups, change, shared).await
}

/// Move a tool's latest to its newest instance that isn't deprecated
///
/// Called after an instance is deprecated, un-deprecated, or deleted, by a caller that holds the
/// tool's lock. A tool that no longer exists is left alone.
///
/// # Arguments
///
/// * `user` - The user who changed the instance
/// * `tool` - The id of the tool
/// * `shared` - Shared Thorium objects
#[instrument(name = "tools::refresh_latest", skip(user, shared), err(Debug))]
pub(crate) async fn refresh_latest(
    user: &User,
    tool: Uuid,
    shared: &Shared,
) -> Result<(), ApiError> {
    // a tool that was deleted has no latest to move
    let groups = match db::entities::all_groups(tool, shared).await {
        Ok(groups) => groups,
        Err(err) if err.status() == axum::http::StatusCode::NOT_FOUND => return Ok(()),
        Err(err) => return Err(err),
    };
    // read the tool in every group
    let mut entity = db::entities::get(&groups, tool, shared).await?;
    entity.groups = groups;
    // find the newest instance that isn't deprecated
    let existing = instances(user, entity.id, &entity.groups, shared).await?;
    let latest = find_latest(&existing);
    // get the tool's current latest
    let meta = tool_meta(&mut entity)?;
    let current = (
        meta.latest,
        meta.latest_version.clone(),
        meta.latest_spec_hash.clone(),
    );
    // build the new latest
    let next = match latest {
        Some((id, version, hash)) => (Some(id), Some(version), Some(hash)),
        None => (None, None, None),
    };
    // only write the tool when its latest moves
    if current == next {
        return Ok(());
    }
    // move latest and mark the tool modified
    meta.latest = next.0;
    meta.latest_version = next.1;
    meta.latest_spec_hash = next.2;
    meta.modified = Utc::now();
    // the change names the new latest instance, or the nil id when there is none
    let change = WorkflowGraphChange::ToolUpdated {
        tool,
        tool_instance: next.0.unwrap_or(Uuid::nil()),
    };
    let groups = entity.groups.clone();
    // rewrite the tool in every group, then queue its change
    db::entities::update(user, entity, &[], &[], shared).await?;
    Event::queue_workflow_graph(user, &groups, change, shared).await
}

/// Make sure a tool with instances isn't deleted
///
/// # Arguments
///
/// * `user` - The user deleting the tool
/// * `tool` - The tool, in all of its groups
/// * `shared` - Shared Thorium objects
#[instrument(name = "tools::check_delete", skip_all, fields(id = tool.id.to_string()), err(Debug))]
pub(crate) async fn check_delete(
    user: &User,
    tool: &Entity,
    shared: &Shared,
) -> Result<(), ApiError> {
    // a tool's instances must be deleted before the tool
    let ids = instance_ids(user, tool.id, &tool.groups, shared).await?;
    if !ids.is_empty() {
        return conflict!(format!(
            "tool `{}` still has tool instances; delete them first",
            tool.name
        ));
    }
    Ok(())
}

/// Describe the source of an edge that references a tool for a removal error
///
/// # Arguments
///
/// * `source` - The edge's source
/// * `group` - The group the edge is in
/// * `shared` - Shared Thorium objects
pub(crate) async fn describe_referrer(
    source: &AssociationTargetColumn,
    group: &str,
    shared: &Shared,
) -> String {
    // name each kind of source the way users know it
    match source {
        AssociationTargetColumn::Entity(id) => {
            // look up the entity's name and kind in the group
            match db::entities::get(&[group.to_owned()], *id, shared).await {
                Ok(entity) => format!("{} `{}` ({id})", entity.kind, entity.name),
                Err(_) => format!("entity {id}"),
            }
        }
        AssociationTargetColumn::File(sha256) => format!("file `{sha256}`"),
        AssociationTargetColumn::Repo(url) => format!("repo `{url}`"),
    }
}

/// Make sure no data in the removed groups still references a tool or its instances
///
/// A blocked removal is a `409` listing the referrers per group that the user can see; hidden
/// referrers are neither named nor counted.
///
/// # Arguments
///
/// * `user` - The user removing the groups
/// * `tool` - The tool
/// * `instances` - The tool's instances
/// * `remove` - The groups being removed
/// * `shared` - Shared Thorium objects
pub(crate) async fn check_removal(
    user: &User,
    tool: &Entity,
    instances: &[Entity],
    remove: &[String],
    shared: &Shared,
) -> Result<(), ApiError> {
    // the tool and its instances are the referenced entities
    let mut targets = vec![tool.id];
    targets.extend(instances.iter().map(|entity| entity.id));
    // find the edges in the removed groups that point at any of them
    let referrers = db::associations::referrers_in_groups(user, &targets, remove, shared).await?;
    // collect the visible referrers per group, skipping the tool's own instance edges
    let mut blocked = false;
    let mut visible: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (group, refs) in referrers {
        for referrer in refs {
            // edges between the tool and its own instances never block a removal
            if let AssociationTargetColumn::Entity(id) = &referrer.source
                && targets.contains(id)
            {
                continue;
            }
            // any other referrer blocks the removal
            blocked = true;
            // only name referrers in groups this user can see
            if user.is_admin() || user.groups.contains(&group) {
                let described = describe_referrer(&referrer.source, &group, shared).await;
                visible.entry(group.clone()).or_default().insert(described);
            }
        }
    }
    // pipelines in the removed groups whose stages run the tool block the removal too
    for group in remove {
        let pipelines = db::pipelines::list_all(group, shared).await?;
        for pipeline in pipelines {
            // only pipelines with a stage backed by the tool or one of its instances block it
            if !pipeline.stage_tools.values().any(|id| targets.contains(id)) {
                continue;
            }
            blocked = true;
            // only name referrers in groups this user can see
            if user.is_admin() || user.groups.contains(group) {
                visible
                    .entry(group.clone())
                    .or_default()
                    .insert(format!("pipeline `{}`", pipeline.name));
            }
        }
    }
    // a removal nothing references goes ahead
    if !blocked {
        return Ok(());
    }
    // refuse the removal, naming only the visible referrers
    conflict!(removal_message(&format!("tool `{}`", tool.name), visible))
}

/// Build the message of a group removal that referrers block
///
/// Only the referrers the caller can see are listed, per group; hidden referrers are neither
/// named nor counted, and when every referrer is hidden the message only says the groups are
/// still referenced.
///
/// # Arguments
///
/// * `entity` - The kind and name of the entity losing groups, such as ``tool `scan` ``
/// * `visible` - The described referrers the caller can see, per group
pub(crate) fn removal_message(entity: &str, visible: BTreeMap<String, BTreeSet<String>>) -> String {
    // say only that the groups are referenced when every referrer is hidden
    if visible.is_empty() {
        return format!("{entity} is still referenced in the groups being removed");
    }
    // list the visible referrers per group
    let listed = visible
        .into_iter()
        .map(|(group, refs)| {
            format!(
                "in `{group}`: {}",
                refs.into_iter().collect::<Vec<String>>().join(", ")
            )
        })
        .collect::<Vec<String>>()
        .join("; ");
    format!("{entity} is still referenced in the groups being removed: {listed}")
}

/// Get the network policy names a tool's instances name in their K8s environments
///
/// # Arguments
///
/// * `instances` - The tool's instances
fn named_policies(instances: &[Entity]) -> BTreeSet<String> {
    // collect the policies each K8s Image or Harness instance names
    let mut names = BTreeSet::new();
    for entity in instances {
        if let EntityMetadata::ToolInstance(ToolInstanceEntity {
            spec:
                ToolSpec::Image(WorkloadSpec {
                    environment: WorkloadEnvironment::K8s(env),
                    ..
                })
                | ToolSpec::Harness(HarnessSpec {
                    workload:
                        WorkloadSpec {
                            environment: WorkloadEnvironment::K8s(env),
                            ..
                        },
                    ..
                }),
            ..
        }) = &entity.metadata
        {
            names.extend(env.network_policies.iter().cloned());
        }
    }
    names
}

/// Check whether two network policies apply the same rules
///
/// Rule ids are ignored, since equal rules in two policies always have different ids.
///
/// # Arguments
///
/// * `left` - The first policy
/// * `right` - The second policy
fn same_rules(left: &NetworkPolicy, right: &NetworkPolicy) -> bool {
    // describe a policy's rules without their ids
    let rules = |policy: &NetworkPolicy| {
        // strip the id of each rule in a direction
        let strip = |rules: &Option<Vec<crate::models::NetworkPolicyRule>>| {
            rules.as_ref().map(|rules| {
                rules
                    .iter()
                    .map(|rule| {
                        // serialize the rule and drop its id
                        let mut value = serde_json::to_value(rule).unwrap_or_default();
                        if let Some(map) = value.as_object_mut() {
                            map.remove("id");
                        }
                        value
                    })
                    .collect::<Vec<serde_json::Value>>()
            })
        };
        (
            strip(&policy.ingress),
            strip(&policy.egress),
            policy.forced_policy,
            policy.default_policy,
        )
    };
    rules(left) == rules(right)
}

/// Check that every network policy a widened tool's instances name is available in each added
/// group, returning the policies an admin's request adds to those groups
///
/// # Arguments
///
/// * `user` - The user adding the groups
/// * `tool` - The tool, in all of its groups
/// * `instances` - The tool's instances
/// * `add` - The groups being added
/// * `shared` - Shared Thorium objects
pub(crate) async fn check_policies(
    user: &User,
    tool: &Entity,
    instances: &[Entity],
    add: &[String],
    shared: &Shared,
) -> Result<Vec<(NetworkPolicy, Vec<String>)>, ApiError> {
    // track the groups each policy must be added to
    let mut adds: Vec<(NetworkPolicy, Vec<String>)> = Vec::new();
    for name in named_policies(instances) {
        // resolve the policy in each of the tool's groups
        let mut resolved: Option<NetworkPolicy> = None;
        for group in &tool.groups {
            // get the policy with this name in this group if it exists
            let Some(policy) =
                db::network_policies::get(std::slice::from_ref(group), &name, None, shared).await?
            else {
                continue;
            };
            // the tool's groups must all resolve the name to the same rules
            match &resolved {
                Some(existing) if existing.id != policy.id && !same_rules(existing, &policy) => {
                    return bad!(format!(
                        "network policy {name} has different rules in the groups of tool `{}`; \
                        make the rules match or remove the conflicting group",
                        tool.name
                    ));
                }
                Some(_) => (),
                None => resolved = Some(policy),
            }
        }
        // check the policy in each added group
        for group in add {
            match db::network_policies::get(std::slice::from_ref(group), &name, None, shared)
                .await?
            {
                // a same-named policy must apply the same rules
                Some(policy) => {
                    let matches = resolved.as_ref().is_some_and(|existing| {
                        existing.id == policy.id || same_rules(existing, &policy)
                    });
                    if !matches {
                        return bad!(format!(
                            "network policy {name} in group {group} has different rules than in the \
                            groups of tool `{}`; make the rules match or remove {group}",
                            tool.name
                        ));
                    }
                }
                // only an admin can add a missing policy to the group
                None => match (&resolved, user.is_admin()) {
                    (Some(policy), true) => {
                        // queue this group to be added to the policy
                        match adds.iter_mut().find(|(queued, _)| queued.id == policy.id) {
                            Some((_, groups)) => groups.push(group.clone()),
                            None => adds.push((policy.clone(), vec![group.clone()])),
                        }
                    }
                    _ => {
                        return bad!(format!(
                            "network policy {name} is not available in group {group}; ask an admin to add it, or remove {group}"
                        ));
                    }
                },
            }
        }
    }
    Ok(adds)
}

/// Add the network policies a widened tool needs to its added groups
///
/// # Arguments
///
/// * `user` - The admin adding the groups
/// * `adds` - The policies and the groups to add each to
/// * `shared` - Shared Thorium objects
pub(crate) async fn add_policies(
    user: &User,
    adds: Vec<(NetworkPolicy, Vec<String>)>,
    shared: &Shared,
) -> Result<(), ApiError> {
    // add the groups to each policy with the admin-only policy update
    for (policy, groups) in adds {
        let update = NetworkPolicyUpdate {
            add_groups: groups,
            ..NetworkPolicyUpdate::default()
        };
        policy.update(update, user, shared).await?;
    }
    Ok(())
}

/// Get an existing tool's instances for a group change
///
/// # Arguments
///
/// * `user` - The user changing the tool's groups
/// * `tool` - The tool, in all of its groups
/// * `shared` - Shared Thorium objects
pub(crate) async fn instances_for_change(
    user: &User,
    tool: &Entity,
    shared: &Shared,
) -> Result<Vec<Entity>, ApiError> {
    // read every instance in every group of the tool
    instances(user, tool.id, &tool.groups, shared).await
}

/// Get a tool's instance summaries for a user, newest first
///
/// # Arguments
///
/// * `user` - The user listing the instances
/// * `tool` - The tool to list instances of
/// * `shared` - Shared Thorium objects
pub(crate) async fn list_summaries(
    user: &User,
    tool: &Entity,
    shared: &Shared,
) -> Result<Vec<ToolInstanceSummary>, ApiError> {
    // only tools have instances
    if tool.kind != EntityKinds::Tool {
        return not_found!(format!("Tool {} not found", tool.id));
    }
    // read the instances in the groups the user can see the tool in
    let found = instances(user, tool.id, &tool.groups, shared).await?;
    Ok(summarize(&found))
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::{is_noop, removal_message};
    use crate::models::entities::digests::ThorDigest;
    use crate::models::{AgenticTool, AgenticToolBacking, ToolSpec};

    /// A small agentic spec
    fn spec() -> ToolSpec {
        // build an agentic spec behind an MCP tool
        let mut input_schema = serde_json::Map::new();
        input_schema.insert("type".into(), "object".into());
        ToolSpec::Agentic(AgenticTool {
            function_name: "get_sample".into(),
            model_description: "Get a sample".into(),
            input_schema,
            backing: AgenticToolBacking::ThoriumMcp {
                tool: "get_sample".into(),
            },
            timeout: Some(60),
            ..AgenticTool::default()
        })
    }

    /// A blocked removal names only visible referrers and never counts hidden ones
    #[test]
    fn removal_messages() {
        // with only hidden referrers no referrer or count is given
        let hidden = removal_message("tool `scan`", BTreeMap::new());
        assert_eq!(
            hidden,
            "tool `scan` is still referenced in the groups being removed"
        );
        // visible referrers are listed per group
        let visible = BTreeMap::from([
            (
                "b".to_owned(),
                BTreeSet::from(["Device `router` (id1)".to_owned()]),
            ),
            (
                "c".to_owned(),
                BTreeSet::from(["file `abc`".to_owned(), "repo `x`".to_owned()]),
            ),
        ]);
        let msg = removal_message("tool `scan`", visible);
        assert_eq!(
            msg,
            "tool `scan` is still referenced in the groups being removed: in `b`: Device `router` (id1); in `c`: file `abc`, repo `x`"
        );
    }

    /// A latest instance stored under an older scheme is rehashed before the no-op check
    #[test]
    fn noop_against_older_scheme() {
        // hash the latest instance under the older v1 scheme
        let latest = spec();
        let stored = ThorDigest::tool_spec_with_scheme("v1:", &latest, None).unwrap();
        // hash the same spec under a newer v2 scheme
        let candidate = ThorDigest::tool_spec_with_scheme("v2:", &latest, None).unwrap();
        assert_ne!(stored, candidate);
        // the stored side is rehashed, so publishing the same spec is still a no-op
        assert!(is_noop("v2:", Some((&stored, &latest, None)), &candidate).unwrap());
        // a different spec isn't a no-op
        let ToolSpec::Agentic(mut other) = spec() else {
            unreachable!()
        };
        other.timeout = Some(61);
        let other_hash =
            ThorDigest::tool_spec_with_scheme("v2:", &ToolSpec::Agentic(other), None).unwrap();
        assert!(!is_noop("v2:", Some((&stored, &latest, None)), &other_hash).unwrap());
        // a tool with no latest instance never gets a no-op
        assert!(!is_noop("v2:", None, &candidate).unwrap());
    }
}
