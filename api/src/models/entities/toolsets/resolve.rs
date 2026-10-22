//! Resolves a toolset into a toolset instance
//!
//! A toolset's members are its explicit `Includes` edges, with a Tool resolved to its latest
//! instance and a Tool Instance kept as it is, and the agentic tools its selector's tag is on in
//! every one of its groups, at their latest instances. An explicit edge wins over the selector
//! for the same tool. With the read only filter on, members that declare any effect are left
//! out. Every tool left out is recorded with why, and the result is validated and digested.
//! The work is bounded: a toolset with more explicit edges, or a selector tag on more tools in
//! one group, than it may consider is refused without reading them. The graph is read through
//! the same [`GraphReader`] as the workflow resolver.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use uuid::Uuid;

use super::{
    TOOL_SET_ID_TAG, ToolSetExclusion, ToolSetExclusionReason, ToolSetInclusion,
    ToolSetInstanceEntity, ToolSetInstanceStatus, ToolSetInstanceTrigger, ToolSetLimits,
    ToolSetMember, ToolSetSelector,
};
use crate::Error;
use crate::models::entities::digests::ThorDigest;
use crate::models::entities::workflows::resolve::{forward, sorted};
use crate::models::entities::workflows::{DedupeKey, GraphEdge, GraphReader};
use crate::models::{
    AssociationKind, Directionality, Entity, EntityKinds, EntityMetadata, ToolInstanceEntity,
    ToolKind, ToolSpec,
};

#[cfg(test)]
mod tests;

/// The edge kinds read from a toolset
const TOOL_SET_EDGES: [AssociationKind; 1] = [AssociationKind::Includes];

/// A toolset resolved into an instance that has no revision yet
///
/// The instance's revision is 0, it supersedes nothing, and its trigger is `Manual` with no
/// `triggered_by` until it's built ([`build_tool_set_instance`]).
#[derive(Debug, Clone)]
pub struct ResolvedToolSet {
    /// The toolset, as it was read
    pub tool_set: Entity,
    /// The resolved instance
    pub instance: ToolSetInstanceEntity,
    /// The tools the toolset includes at latest, whatever they resolved to, and the tools its
    /// selected members follow at latest
    pub follows_tools: BTreeSet<Uuid>,
}

impl ResolvedToolSet {
    /// Get the sorted ids of the entities this instance uses
    #[must_use]
    pub fn referenced_ids(&self) -> BTreeSet<Uuid> {
        // the tools followed at latest and every member's tool instance
        referenced_ids(&self.instance, self.follows_tools.iter().copied())
    }

    /// Get this instance's dedupe key
    #[must_use]
    pub fn dedupe_key(&self) -> DedupeKey {
        // the digest, referenced ids, and status of the instance
        DedupeKey {
            digest: self.instance.digest.clone(),
            referenced: self.referenced_ids(),
            entry_agent: None,
            delegations: BTreeSet::new(),
            errors: status_errors(&self.instance.status),
        }
    }
}

/// Get the sorted reasons a toolset instance is invalid, or none for a ready one
///
/// # Arguments
///
/// * `status` - The instance's status
pub(crate) fn status_errors(status: &ToolSetInstanceStatus) -> Vec<String> {
    // a ready instance has no errors
    let mut errors = match status {
        ToolSetInstanceStatus::Ready => Vec::new(),
        ToolSetInstanceStatus::Invalid { errors } => errors.clone(),
    };
    // sort them, so the order they were found in doesn't matter
    errors.sort_unstable();
    errors
}

/// Collect the ids of the entities a toolset instance uses
///
/// These are the tools its members follow at latest and its members' tool instances.
///
/// # Arguments
///
/// * `instance` - The toolset instance
/// * `follows_tools` - The tools its members follow at latest
pub(crate) fn referenced_ids(
    instance: &ToolSetInstanceEntity,
    follows_tools: impl Iterator<Item = Uuid>,
) -> BTreeSet<Uuid> {
    // the followed tools and each member's tool instance
    let mut ids = follows_tools.collect::<BTreeSet<Uuid>>();
    ids.extend(instance.members.iter().map(|member| member.tool_instance));
    ids
}

/// The outcome of building a toolset's next instance
#[derive(Debug, Clone)]
pub enum ToolSetBuild {
    /// The newest instance already has the resolved instance's dedupe key
    Current {
        /// The newest instance
        newest: Uuid,
        /// The toolset as it resolved
        resolved: Box<ResolvedToolSet>,
    },
    /// A new instance to submit, with its revision, previous instance, and trigger set
    New(Box<ResolvedToolSet>),
}

/// A tool instance considered as a member
struct Candidate {
    /// The tool instance, in all of its groups
    entity: Entity,
    /// The tool instance's metadata
    meta: ToolInstanceEntity,
    /// How the toolset included it
    inclusion: ToolSetInclusion,
}

/// The state of one toolset's resolution
struct Resolver<'a, R: GraphReader + ?Sized> {
    /// Reads the graph
    reader: &'a R,
    /// The toolset limits
    limits: ToolSetLimits,
    /// The toolset, in all of its groups
    tool_set: Entity,
    /// Why the instance is invalid
    errors: Vec<String>,
    /// Non fatal issues
    warnings: Vec<String>,
    /// Each tool considered as a member, by its tool
    candidates: BTreeMap<Uuid, Candidate>,
    /// The tools the toolset includes explicitly, which the selector skips
    explicit: BTreeSet<Uuid>,
    /// The tools the toolset includes at latest through its own edges, whatever they resolve to
    follows: BTreeSet<Uuid>,
    /// Every tool left out, and why
    excluded: Vec<ToolSetExclusion>,
}

impl<'a, R: GraphReader + ?Sized> Resolver<'a, R> {
    /// Start resolving a toolset
    ///
    /// # Arguments
    ///
    /// * `reader` - Reads the graph
    /// * `limits` - The toolset limits
    /// * `tool_set` - The toolset, in all of its groups
    fn new(reader: &'a R, limits: ToolSetLimits, tool_set: Entity) -> Self {
        Resolver {
            reader,
            limits,
            tool_set,
            errors: Vec::new(),
            warnings: Vec::new(),
            candidates: BTreeMap::new(),
            explicit: BTreeSet::new(),
            follows: BTreeSet::new(),
            excluded: Vec::new(),
        }
    }

    /// Read an entity without any edges
    ///
    /// # Arguments
    ///
    /// * `id` - The entity to read
    async fn read(&self, id: Uuid) -> Result<Option<Entity>, Error> {
        // read just the entity
        Ok(self
            .reader
            .node(id, &[], Directionality::To)
            .await?
            .map(|node| node.entity))
    }

    /// Resolve the toolset's explicit `Includes` edges
    ///
    /// # Arguments
    ///
    /// * `edges` - The toolset's outgoing `Includes` edges, sorted
    async fn explicit(&mut self, edges: &[GraphEdge]) -> Result<(), Error> {
        // the toolset's name, for errors
        let name = self.tool_set.name.clone();
        // more edges than the toolset may consider are refused without reading them
        let cap = self.limits.max_candidates();
        if edges.len() > cap {
            self.errors.push(format!(
                "toolset `{name}` has {} Includes edges, over the {cap} a toolset can consider (4 x max_tools_per_toolset)",
                edges.len()
            ));
            return Ok(());
        }
        for edge in edges {
            match edge.other_kind {
                // a tool resolves to its latest instance, and is followed whatever it resolves to
                EntityKinds::Tool => {
                    self.explicit.insert(edge.other);
                    self.follows.insert(edge.other);
                    let Some(root) = self.read(edge.other).await? else {
                        self.errors.push(format!(
                            "toolset `{name}` includes tool `{}`, which doesn't exist",
                            edge.other
                        ));
                        continue;
                    };
                    let EntityMetadata::Tool(meta) = &root.metadata else {
                        self.errors
                            .push(format!("`{}` is a {}, not a tool", root.name, root.kind));
                        continue;
                    };
                    let Some(latest) = meta.latest else {
                        self.errors.push(format!(
                            "toolset `{name}` includes tool `{}` at latest, but it has no latest instance (every instance is deprecated or it has none)",
                            root.name
                        ));
                        continue;
                    };
                    self.take(latest, ToolSetInclusion::ExplicitLatest).await?;
                }
                // a tool instance is pinned as it is
                EntityKinds::ToolInstance => {
                    self.take(edge.other, ToolSetInclusion::ExplicitPinned)
                        .await?;
                }
                // nothing else can be a member
                other => self.errors.push(format!(
                    "toolset `{name}` has an Includes edge to `{}`, which is a {other}, not a tool",
                    edge.other
                )),
            }
        }
        Ok(())
    }

    /// Take a tool instance as a candidate member
    ///
    /// # Arguments
    ///
    /// * `id` - The tool instance
    /// * `inclusion` - How the toolset includes it
    async fn take(&mut self, id: Uuid, inclusion: ToolSetInclusion) -> Result<(), Error> {
        // the toolset's name, for errors
        let name = self.tool_set.name.clone();
        // the instance must exist and be an agentic tool instance
        let Some(entity) = self.read(id).await? else {
            self.errors.push(format!(
                "toolset `{name}` includes tool instance `{id}`, which doesn't exist"
            ));
            return Ok(());
        };
        let EntityMetadata::ToolInstance(meta) = &entity.metadata else {
            self.errors.push(format!(
                "`{}` is a {}, not a tool instance",
                entity.name, entity.kind
            ));
            return Ok(());
        };
        let meta = meta.clone();
        if meta.spec.kind() != ToolKind::Agentic {
            self.errors.push(format!(
                "toolset `{name}` includes `{}`, which isn't an agentic tool",
                entity.name
            ));
            return Ok(());
        }
        // a pinned instance's tool is explicit too, so the selector skips it
        if inclusion == ToolSetInclusion::ExplicitPinned {
            self.explicit.insert(meta.tool);
        }
        // one tool can be a member at one instance only
        if let Some(first) = self.candidates.get(&meta.tool) {
            if first.entity.id != entity.id {
                self.errors.push(format!(
                    "toolset `{name}` includes tool `{}` at two instances: `{}` and `{}`",
                    meta.tool, first.entity.name, entity.name
                ));
            }
            return Ok(());
        }
        self.candidates.insert(
            meta.tool,
            Candidate {
                entity,
                meta,
                inclusion,
            },
        );
        Ok(())
    }

    /// Take in the tools the selector's tag is on in every one of the toolset's groups
    ///
    /// Each group is searched on its own, for at most the tools a toolset may consider, so a
    /// tag on too many tools is refused without reading them. Tools the toolset includes
    /// explicitly are skipped.
    ///
    /// # Arguments
    ///
    /// * `selector` - The toolset's tag selector
    async fn select(&mut self, selector: &ToolSetSelector) -> Result<(), Error> {
        // the toolset's name and groups, and how many tools it may consider
        let name = self.tool_set.name.clone();
        let groups = sorted(&self.tool_set.groups);
        let cap = self.limits.max_candidates();
        // find the tools tagged in each group, and the groups each is tagged in
        let mut tagged: BTreeMap<Uuid, BTreeSet<String>> = BTreeMap::new();
        let mut roots: HashMap<Uuid, Entity> = HashMap::new();
        for group in &groups {
            let found = self
                .reader
                .tagged_up_to(
                    EntityKinds::Tool,
                    &selector.key,
                    &selector.value,
                    std::slice::from_ref(group),
                    cap + 1,
                )
                .await?;
            if found.len() > cap {
                self.errors.push(format!(
                    "selector `{selector}` of toolset `{name}` matches more than {cap} tools in group `{group}`, over the {cap} a toolset can consider (4 x max_tools_per_toolset)"
                ));
                return Ok(());
            }
            for entity in found {
                tagged.entry(entity.id).or_default().insert(group.clone());
                roots.entry(entity.id).or_insert(entity);
            }
        }
        // split the tools by whether they're tagged in every group
        let matches = ToolSetSelector::partition(&groups, &tagged);
        // a tool tagged in only some groups is left out, naming the rest
        for (tool, missing) in matches.partial {
            if self.explicit.contains(&tool) {
                continue;
            }
            let name = roots
                .get(&tool)
                .map_or_else(String::new, |root| root.name.clone());
            self.excluded.push(ToolSetExclusion {
                tool,
                tool_instance: None,
                name,
                inclusion: ToolSetInclusion::Selector,
                reason: ToolSetExclusionReason::NotTaggedInAllGroups,
                missing_groups: missing,
            });
        }
        // a tool tagged in every group is a member at its latest, if it's agentic and has one
        for tool in matches.matched {
            if self.explicit.contains(&tool) {
                continue;
            }
            let Some(root) = roots.get(&tool) else {
                continue;
            };
            let EntityMetadata::Tool(meta) = &root.metadata else {
                continue;
            };
            let reason = match (meta.kind, meta.latest) {
                (ToolKind::Agentic, Some(latest)) => {
                    self.take(latest, ToolSetInclusion::Selector).await?;
                    continue;
                }
                (ToolKind::Agentic, None) => ToolSetExclusionReason::NoLatest,
                _ => ToolSetExclusionReason::NotAgentic,
            };
            self.excluded.push(ToolSetExclusion {
                tool,
                tool_instance: meta.latest,
                name: root.name.clone(),
                inclusion: ToolSetInclusion::Selector,
                reason,
                missing_groups: Vec::new(),
            });
        }
        Ok(())
    }

    /// Leave out every candidate that declares an effect, with a warning
    fn filter_read_only(&mut self) {
        // the toolset's name, for warnings
        let name = self.tool_set.name.clone();
        // find the candidates whose spec declares any effect
        let declaring = self
            .candidates
            .iter()
            .filter_map(|(tool, candidate)| match &candidate.meta.spec {
                ToolSpec::Agentic(agentic) if !agentic.capabilities.effects.is_empty() => {
                    Some(*tool)
                }
                _ => None,
            })
            .collect::<Vec<Uuid>>();
        // move each of them to the exclusions
        for tool in declaring {
            let Some(candidate) = self.candidates.remove(&tool) else {
                continue;
            };
            let effects = match &candidate.meta.spec {
                ToolSpec::Agentic(agentic) => agentic
                    .capabilities
                    .effects
                    .iter()
                    .map(|effect| format!("{effect:?}"))
                    .collect::<Vec<String>>()
                    .join(", "),
                ToolSpec::Image(_) | ToolSpec::Harness(_) => String::new(),
            };
            self.warnings.push(format!(
                "`{}` declares effects ({effects}), so read only toolset `{name}` leaves it out",
                candidate.entity.name
            ));
            self.excluded.push(ToolSetExclusion {
                tool,
                tool_instance: Some(candidate.entity.id),
                name: candidate.entity.name,
                inclusion: candidate.inclusion,
                reason: ToolSetExclusionReason::NotReadOnly,
                missing_groups: Vec::new(),
            });
        }
    }

    /// Validate the remaining candidates and describe them as members
    ///
    /// A member whose groups don't cover the toolset's is left out with an error, two members a
    /// model would call by one function name are an error, and so are more members than the
    /// limit; no members at all is a warning.
    fn members(&mut self) -> Vec<ToolSetMember> {
        // the toolset's name and groups, for errors and containment
        let name = self.tool_set.name.clone();
        let groups = self.tool_set.groups.clone();
        let mut members = Vec::with_capacity(self.candidates.len());
        let mut functions: HashMap<String, String> = HashMap::new();
        for candidate in std::mem::take(&mut self.candidates).into_values() {
            // a member must cover every group of the toolset
            let missing = AssociationKind::Includes
                .group_policy(&groups, &candidate.entity.groups)
                .err()
                .unwrap_or_default();
            if !missing.is_empty() {
                self.errors.push(format!(
                    "`{}` is missing groups {missing:?} of `{name}`, which includes it",
                    candidate.entity.name
                ));
                continue;
            }
            // a model calls each member by a different function name
            let function = candidate
                .meta
                .spec
                .function_name()
                .unwrap_or_default()
                .to_owned();
            match functions.get(&function) {
                Some(first) => self.errors.push(format!(
                    "toolset `{name}` gives two tools the function name `{function}`: `{first}` and `{}`",
                    candidate.entity.name
                )),
                None => {
                    functions.insert(function.clone(), candidate.entity.name.clone());
                }
            }
            members.push(ToolSetMember {
                tool: candidate.meta.tool,
                tool_instance: candidate.entity.id,
                name: candidate.entity.name.clone(),
                groups: sorted(&candidate.entity.groups),
                version: candidate.meta.version.to_string(),
                spec_hash: candidate.meta.spec_hash.clone(),
                function_name: function,
                inclusion: candidate.inclusion,
            });
        }
        // the member limit applies after every filter
        let max = self.limits.max_tools_per_toolset;
        if members.len() > max {
            self.errors.push(format!(
                "toolset `{name}` has {} members, over max_tools_per_toolset ({max})",
                members.len()
            ));
        }
        // an empty toolset is allowed but gives agents nothing
        if members.is_empty() {
            self.warnings.push(format!(
                "toolset `{name}` has no members, so it gives agents no tools"
            ));
        }
        members
    }
}

/// Resolve a toolset into an instance
///
/// Problems with the toolset's members make the instance `Invalid` rather than failing; only a
/// read that fails, a toolset that doesn't exist, or an entity that isn't a toolset is an error.
///
/// # Arguments
///
/// * `reader` - Reads the graph
/// * `tool_set` - The toolset to resolve
/// * `limits` - The toolset limits
pub async fn resolve_tool_set<R: GraphReader + ?Sized>(
    reader: &R,
    tool_set: Uuid,
    limits: &ToolSetLimits,
) -> Result<ResolvedToolSet, Error> {
    // read the toolset and its explicit edges
    let Some(node) = reader
        .node(tool_set, &TOOL_SET_EDGES, Directionality::To)
        .await?
    else {
        return Err(Error::new(format!("toolset {tool_set} doesn't exist")));
    };
    let EntityMetadata::ToolSet(meta) = &node.entity.metadata else {
        return Err(Error::new(format!(
            "`{}` is a {}, not a toolset",
            node.entity.name, node.entity.kind
        )));
    };
    let (read_only, selector) = (meta.read_only, meta.selector.clone());
    let edges = forward(node.edges, &TOOL_SET_EDGES);
    let mut resolver = Resolver::new(reader, *limits, node.entity);
    // explicit members first, then the selector's, which skips explicit tools
    resolver.explicit(&edges).await?;
    if let Some(selector) = &selector {
        resolver.select(selector).await?;
    }
    // the read only filter applies to every member
    if read_only {
        resolver.filter_read_only();
    }
    // validate and describe the members; the tools included at latest and the selected members
    // are followed
    let members = resolver.members();
    let mut follows_tools = std::mem::take(&mut resolver.follows);
    follows_tools.extend(
        members
            .iter()
            .filter(|member| member.inclusion.follows_latest())
            .map(|member| member.tool),
    );
    // build the instance, sort its lists, and digest it
    let status = if resolver.errors.is_empty() {
        ToolSetInstanceStatus::Ready
    } else {
        ToolSetInstanceStatus::Invalid {
            errors: std::mem::take(&mut resolver.errors),
        }
    };
    let mut instance = ToolSetInstanceEntity {
        tool_set,
        tool_set_groups: sorted(&resolver.tool_set.groups),
        revision: 0,
        previous: None,
        status,
        warnings: std::mem::take(&mut resolver.warnings),
        trigger: ToolSetInstanceTrigger::Manual,
        triggered_by: String::new(),
        read_only,
        selector,
        members,
        excluded: std::mem::take(&mut resolver.excluded),
        digest: String::new(),
    };
    instance.sort_lists();
    instance.digest = instance
        .compute_digest()
        .map_err(|err| Error::new(format!("failed to hash a toolset instance: {err}")))?;
    Ok(ResolvedToolSet {
        tool_set: resolver.tool_set,
        instance,
        follows_tools,
    })
}

/// Find a toolset's newest instance, whatever its status
///
/// The instances are found by their `ToolSetId` tag in the toolset's groups.
///
/// # Arguments
///
/// * `reader` - Reads the graph
/// * `tool_set` - The toolset, in all of its groups
pub async fn newest_tool_set_instance<R: GraphReader + ?Sized>(
    reader: &R,
    tool_set: &Entity,
) -> Result<Option<Entity>, Error> {
    // look up the toolset's instances by tag
    let found = reader
        .tagged(
            EntityKinds::ToolSetInstance,
            TOOL_SET_ID_TAG,
            &tool_set.id.to_string(),
            &tool_set.groups,
        )
        .await?;
    // keep the instance of this toolset with the highest revision
    let newest = found
        .into_iter()
        .filter_map(|entity| match &entity.metadata {
            EntityMetadata::ToolSetInstance(instance) if instance.tool_set == tool_set.id => {
                Some((instance.revision, entity))
            }
            _ => None,
        })
        .max_by_key(|(revision, _)| *revision)
        .map(|(_, entity)| entity);
    Ok(newest)
}

/// Check whether a stored toolset instance has a candidate's dedupe key under a scheme
///
/// A stored digest under an older scheme is recomputed from its stored members, never
/// rewritten.
///
/// # Arguments
///
/// * `scheme` - The active digest scheme
/// * `candidate` - The candidate's dedupe key, digested under the active scheme
/// * `stored` - The stored toolset instance, with its tags
pub(crate) fn is_current_with_scheme(
    scheme: &str,
    candidate: &DedupeKey,
    stored: &Entity,
) -> Result<bool, Error> {
    // only a toolset instance with the same referenced ids and status can be current
    let Some(key) = DedupeKey::of_stored(stored) else {
        return Ok(false);
    };
    let EntityMetadata::ToolSetInstance(instance) = &stored.metadata else {
        return Ok(false);
    };
    if !key.same_except_digest(candidate) {
        return Ok(false);
    }
    // compare the digests, recomputing an older stored one under the active scheme
    ThorDigest::same_definition_with_scheme(
        scheme,
        &key.digest,
        || {
            instance
                .compute_digest_with_scheme(scheme)
                .map_err(|err| Error::new(format!("failed to hash a toolset instance: {err}")))
        },
        &candidate.digest,
    )
}

/// Build a toolset's next instance, or find that its newest instance is current
///
/// The newest instance, of any status, is current when it has the resolved instance's digest,
/// referenced ids, and status. Otherwise the new instance takes the revision after both the
/// newest instance's and the highest revision the toolset ever gave out, and supersedes the newest
/// instance. Nothing is written.
///
/// # Arguments
///
/// * `reader` - Reads the graph
/// * `tool_set` - The toolset to build
/// * `limits` - The toolset limits
/// * `trigger` - What caused the build
/// * `triggered_by` - The user whose change or request caused the build
pub async fn build_tool_set_instance<R: GraphReader + ?Sized>(
    reader: &R,
    tool_set: Uuid,
    limits: &ToolSetLimits,
    trigger: ToolSetInstanceTrigger,
    triggered_by: &str,
) -> Result<ToolSetBuild, Error> {
    // resolve the toolset and find its newest instance
    let mut resolved = resolve_tool_set(reader, tool_set, limits).await?;
    let newest = newest_tool_set_instance(reader, &resolved.tool_set).await?;
    // an unchanged toolset builds nothing
    if let Some(newest) = &newest
        && is_current_with_scheme(ThorDigest::SCHEME, &resolved.dedupe_key(), newest)?
    {
        return Ok(ToolSetBuild::Current {
            newest: newest.id,
            resolved: Box::new(resolved),
        });
    }
    // the next revision follows every revision given out so far
    let newest_revision = newest
        .as_ref()
        .and_then(|entity| match &entity.metadata {
            EntityMetadata::ToolSetInstance(instance) => Some(instance.revision),
            _ => None,
        })
        .unwrap_or(0);
    let highest = match &resolved.tool_set.metadata {
        EntityMetadata::ToolSet(meta) => meta.highest_revision,
        _ => 0,
    };
    resolved.instance.revision = newest_revision.max(highest) + 1;
    resolved.instance.previous = newest.map(|entity| entity.id);
    resolved.instance.trigger = trigger;
    resolved.instance.triggered_by = triggered_by.to_owned();
    Ok(ToolSetBuild::New(Box::new(resolved)))
}
