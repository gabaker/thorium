//! Hunt through data using sigma rules

use futures::lock::Mutex;
use futures::stream::StreamExt;
use gxhash::GxHasher;
use linearize::StaticMap;
use papaya::HashMap as PapayaMap;
use sigma_rust::Rule;
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::ops::Deref;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use thorium::models::entities::rules::jq::CompiledJq;
use thorium::models::entities::rules::{SigmaActionToTake, SigmaAutoFlag, SigmaSpawnReaction};
use thorium::models::{
    AssociationKind, AssociationRequest, AssociationTarget, Entity, EntityKinds, EntityListOpts,
    EntityMetadata, EntityMetadataRequest, EntityRequest, Event, EventData, EventType,
    GenericJobArgs, OutputKey, ReactionRequest, RepoDependencyRequest, ScrubbedUser, SigmaRule,
    SigmaScannableResultsEvent, TreeNode, TreeOpts, TreeQuery, UserRole,
};
use thorium::{Error, Thorium};
use tracing::{Level, event, instrument};
use uuid::Uuid;

use crate::libs::EventWorkerCache;
use crate::libs::workers::{
    EventWorkerSupport, EventWorkerSupportCore, WorkerCacheUpdateKinds, WorkerCacheUpdates,
};

/// How long a spawn actions jq filters may run against a single entity
///
/// Rendering runs on a blocking thread that cannot be cancelled, so a timed
/// out render leaks a thread that spins until the process restarts. Leaks are
/// tracked so a timed out action is never rerendered and rendering shuts off
/// entirely at [`MAX_LEAKED_JQ_THREADS`], keeping leaks from piling up and
/// starving the shared blocking pool the rest of this daemon runs on.
const JQ_RENDER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The max number of blocking threads that timed out jq renders may leak
///
/// Each leaked thread spins at full cpu until the process restarts, so once
/// this many have been leaked all spawn arg rendering is refused instead of
/// letting leaks exhaust tokios blocking pool (which yara scans and dns
/// lookups also run on).
const MAX_LEAKED_JQ_THREADS: usize = 32;

/// How many blocking threads timed out jq renders have leaked
static LEAKED_JQ_THREADS: AtomicUsize = AtomicUsize::new(0);

/// The hashes of spawn actions whose jq renders have timed out
///
/// Timed out actions are refused instead of rerendered on later events since
/// every rerender of a non terminating filter would leak another thread. This
/// is process wide because the blocking pool the leaks land on is too.
static TIMED_OUT_SPAWNS: StdMutex<BTreeSet<u64>> = StdMutex::new(BTreeSet::new());

/// Hash a spawn action so timed out renders can be tracked across events
///
/// # Arguments
///
/// * `spawn` - The spawn reaction action to hash
fn hash_spawn_action(spawn: &SigmaSpawnReaction) -> u64 {
    // get the hasher to hash this spawn action with
    let mut hasher = GxHasher::default();
    // hash this spawn action
    spawn.hash(&mut hasher);
    // get this spawn actions hash
    hasher.finish()
}

/// Check whether a spawn actions jq filters may be rendered
///
/// Rendering is refused for actions whose filters already timed out and for
/// all actions once too many blocking threads have been leaked, since leaked
/// threads spin until the process restarts and would otherwise pile up until
/// they peg this pods cpu and starve tokios shared blocking pool.
///
/// # Arguments
///
/// * `timed_out` - The hashes of spawn actions whose renders have timed out
/// * `leaked` - How many blocking threads timed out renders have leaked
/// * `action_hash` - The hash of the spawn action to check
fn check_render_budget(
    timed_out: &StdMutex<BTreeSet<u64>>,
    leaked: &AtomicUsize,
    action_hash: u64,
) -> Result<(), Error> {
    // get the set of timed out spawn actions even if a prior lock holder panicked
    let timed_out = timed_out
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // refuse to rerender actions whose filters already timed out
    if timed_out.contains(&action_hash) {
        return Err(Error::new(
            "Refusing to render a spawn action whose jq filters previously timed out; \
            fix the rule and restart the event handler",
        ));
    }
    // refuse all renders once too many blocking threads have been leaked
    if leaked.load(Ordering::Relaxed) >= MAX_LEAKED_JQ_THREADS {
        return Err(Error::new(format!(
            "Refusing to render spawn args since timed out jq renders have leaked \
            {MAX_LEAKED_JQ_THREADS} blocking threads; restart the event handler"
        )));
    }
    Ok(())
}

/// Record that a spawn actions jq render timed out and leaked its thread
///
/// # Arguments
///
/// * `timed_out` - The hashes of spawn actions whose renders have timed out
/// * `leaked` - How many blocking threads timed out renders have leaked
/// * `action_hash` - The hash of the spawn action that timed out
fn record_render_timeout(
    timed_out: &StdMutex<BTreeSet<u64>>,
    leaked: &AtomicUsize,
    action_hash: u64,
) {
    // count the blocking thread this timed out render leaked
    leaked.fetch_add(1, Ordering::Relaxed);
    // get the set of timed out spawn actions even if a prior lock holder panicked
    let mut timed_out = timed_out
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // track this action so it is not rerendered on later events
    timed_out.insert(action_hash);
}

/// The stats for each entity
#[derive(Default, Debug, Clone, Eq, PartialEq)]
pub struct SigmaRuleSpecificStats {
    /// The total number of entities scanned
    pub scanned: usize,
    /// The total number of entities that have been created in Thorium
    pub created: usize,
    /// The total number of flags created
    pub flagged: usize,
    /// The number of reactions spawned
    ///
    /// Per entity kind this counts the spawn requests that rule hits collected
    /// while the total counts the reactions that were actually created.
    pub spawned: usize,
    /// The number of reaction spawns that failed or were refused
    ///
    /// Per entity kind this counts hits whose spawn was refused or failed to
    /// render while the total counts reactions that failed to be created.
    pub spawn_errors: usize,
}

impl std::ops::AddAssign for SigmaRuleSpecificStats {
    fn add_assign(&mut self, rhs: Self) {
        self.scanned = self.scanned.saturating_add(rhs.scanned);
        self.created = self.created.saturating_add(rhs.created);
        self.flagged = self.flagged.saturating_add(rhs.flagged);
        self.spawned = self.spawned.saturating_add(rhs.spawned);
        self.spawn_errors = self.spawn_errors.saturating_add(rhs.spawn_errors);
    }
}

/// The stats for a sigma rule worker
#[derive(Default, Debug, Clone, Eq, PartialEq)]
pub struct SigmaRuleStats {
    /// The total across all entities regardless of kind
    pub total: SigmaRuleSpecificStats,
    /// The stats on a per entity basis
    pub by_entity: StaticMap<EntityKinds, SigmaRuleSpecificStats>,
}

impl std::ops::AddAssign for SigmaRuleStats {
    fn add_assign(&mut self, rhs: Self) {
        // add our total
        self.total += rhs.total;
        // add each entity kind
        for (kind, stats) in rhs.by_entity {
            // get an entry to this entity kinds stats
            self.by_entity[kind] += stats;
        }
    }
}
/// A compiled sigma rule
#[derive(Debug, Clone)]
pub struct CompiledSigmaRule {
    /// The original sigma rule in Thorium
    pub original: Entity,
    /// The compiled sigma rule
    pub compiled: Rule,
}

impl CompiledSigmaRule {
    /// Get the original sigma rule metadata
    pub fn get_original_rule(&self) -> Result<&SigmaRule, Error> {
        // make sure we have a sigma rule
        match &self.original.metadata {
            EntityMetadata::SigmaRule(rule) => Ok(rule),
            _ => {
                // build an error message saying we don't have a sigma rule
                let msg = format!("Compiled sigma rule does not contain a sigma rule?: {self:?}");
                // build and return an error
                Err(Error::new(msg))
            }
        }
    }
}

/// The context needed for this sigma rule worker
#[derive(Debug, Clone)]
pub struct SigmaRuleContext {
    /// The different rules to scan with
    pub combined: Vec<CompiledSigmaRule>,
    /// A map of entity kinds to rules
    pub applies_to: StaticMap<EntityKinds, Vec<usize>>,
}

impl SigmaRuleContext {
    /// Build a [`SigmaRuleContext`] from info in the Thorium API
    ///
    /// # Arguments
    ///
    /// * `thorium` - A Thorium client
    pub async fn new(thorium: &Thorium) -> Result<Self, Error> {
        // build the options for listing entities
        let opts = EntityListOpts::default()
            .kind(EntityKinds::SigmaRule)
            // use a large page size to reduce round trips
            .page_size(2000);
        // get cursor over all sigma rule entities
        let mut cursor = thorium.entities.list_details(&opts).await?;
        // pre allocate a list for our rules
        let mut combined_list = Vec::with_capacity(cursor.data.len());
        // pre allocate a map for our different entity kinds and their rule indexes
        let mut applies_to_map = StaticMap::<EntityKinds, Vec<usize>>::default();
        // iterate over all the rules in Thorium and add them to our context
        loop {
            for original in cursor.data.drain(..) {
                // compile this sigma rule
                let compiled = match &original.metadata {
                    EntityMetadata::SigmaRule(sigma) => {
                        // compile this sigma rule; a stored rule can go stale (e.g. across
                        // sigma-rust upgrades) and one bad rule must never crash the daemon
                        // now that this context is periodically rebuilt
                        let compiled = match sigma_rust::rule_from_yaml(&sigma.rule) {
                            Ok(compiled) => compiled,
                            Err(error) => {
                                // log that this stored sigma rule no longer compiles
                                event!(
                                    Level::ERROR,
                                    msg = "Skipping sigma rule that failed to compile",
                                    rule = original.name,
                                    id = original.id.to_string(),
                                    error = error.to_string(),
                                );
                                // skip this rule and keep the rest of the context
                                continue;
                            }
                        };
                        // get the next available index that this new rule will go into
                        let index = combined_list.len();
                        // add this rules index to our rule map for all of its entity kinds
                        for applies_to in &sigma.applies_to {
                            // get the entity kind this is sigma applies too converts too
                            let kind = EntityKinds::from(applies_to);
                            // add this index
                            applies_to_map[kind].push(index);
                        }
                        // reutrn this compiled sigma rule
                        compiled
                    }
                    // we can't compile a non sigma rule entity
                    _ => {
                        // log that we trying to compile an entity thats not a sigma rule
                        event!(
                            Level::ERROR,
                            msg = "Can't compile a non sigma rule entity",
                            kind = original.kind.as_str()
                        );
                        // continue on and just ignore this bad entity
                        continue;
                    }
                };
                // combine our original and compiled sigma rule
                let combined = CompiledSigmaRule { original, compiled };
                // add our combined rule to our context
                combined_list.push(combined);
            }
            // if this cursor is finished then stop listing sigma rules
            if cursor.exhausted() {
                break;
            }
            // this cursor has more data so get the next page
            cursor.refill().await?;
        }
        // build a sigma rule context
        let context = SigmaRuleContext {
            combined: combined_list,
            applies_to: applies_to_map,
        };
        Ok(context)
    }
}

/// Either an existing assocation target or one that will be created once we create the target entity
#[derive(Debug)]
enum MaybeEntity {
    /// An entity that is going to be created
    New(usize),
    /// An existing entity
    Existing(AssociationTarget),
}

impl MaybeEntity {
    /// Get an association target from a list of entity responses if needed
    ///
    /// # Arguments
    ///
    /// * `resps` - The list of entity creation responses to get our id and names from
    pub fn get_target(self, resps: &[Option<AssociationTarget>]) -> Option<AssociationTarget> {
        match self {
            Self::New(index) => resps.get(index).cloned().flatten(),
            Self::Existing(target) => Some(target.clone()),
        }
    }

    /// Create a `MaybeEntity` for an existing entity
    ///
    /// # Arguments
    ///
    /// * `id` - The id of the existing entity
    /// * `name` - The name of the existing entity
    pub fn new_existing(id: Uuid, name: impl Into<String>) -> Self {
        // build an association target for this entity
        let target = AssociationTarget::Entity {
            id,
            name: name.into(),
        };
        // wrap this target
        MaybeEntity::Existing(target)
    }
}

impl From<usize> for MaybeEntity {
    /// Wrap the index to an entity that will be created in a `MaybeEntity`
    ///
    /// # Arguments
    ///
    /// * `index` - The index to the entity that will be created
    fn from(index: usize) -> Self {
        MaybeEntity::New(index)
    }
}

impl From<Entity> for MaybeEntity {
    /// Wrap an existing entity in a `MaybeEntity`
    ///
    /// # Arguments
    ///
    /// * `entity` - An existing entity
    fn from(entity: Entity) -> Self {
        // build an association target for this entity
        let target = AssociationTarget::Entity {
            id: entity.id,
            name: entity.name,
        };
        // wrap this target
        MaybeEntity::Existing(target)
    }
}

impl From<AssociationTarget> for MaybeEntity {
    /// Wrap an `AssociationTarget` to an existing entity in a `MaybeEntity`
    ///
    /// # Arguments
    ///
    /// * `target` - The target to wrap
    fn from(target: AssociationTarget) -> Self {
        // wrap this target in an existing wrapper
        MaybeEntity::Existing(target)
    }
}

/// Create an association between things in Thorium
///
/// This function is called instead of just directly calling the client method to ensure our return type
/// is Result<(), Error>.
///
/// # Arguments
///
/// * `thorium` - A Thorium client
/// * `assoc_req` - The association to create
async fn create_association(thorium: &Thorium, assoc_req: AssociationRequest) -> Result<(), Error> {
    thorium.associations.create(&assoc_req).await?;
    Ok(())
}

/// Track whether this entity should be created, updated, or already exists
#[allow(
    clippy::large_enum_variant,
    reason = "boxing a variant changes how entities are built and matched throughout the sigma hunt"
)]
pub enum MaybeEntityCreate {
    /// This entity needs to be created
    Create(EntityRequest),
    /// This entity already exists
    Exists(AssociationTarget),
}

impl MaybeEntityCreate {
    /// Create an entity if it does not yet exist
    ///
    /// # Arguments
    ///
    /// * `thorium` - A Thorium client
    pub async fn apply(self, thorium: &Thorium) -> Result<AssociationTarget, Error> {
        match self {
            MaybeEntityCreate::Create(req) => {
                // create this entity
                let resp = thorium.entities.create(req).await?;
                // build an association target for this entity
                let target = AssociationTarget::Entity {
                    id: resp.id,
                    name: resp.name,
                };
                Ok(target)
            }
            MaybeEntityCreate::Exists(target) => Ok(target),
        }
    }
}

/// A cache of actions to take after scanning a single sigma result
#[derive(Debug)]
struct SigmaActionCache {
    /// The original data that all scanned entities originate from
    key: OutputKey,
    /// The hashes for entities that we are creating
    pending_entities: HashMap<u64, usize>,
    /// The entities that we are creating
    entities: Vec<EntityRequest>,
    /// Any associations to create between two nodes in this cache (source, target, groups)
    #[allow(
        clippy::type_complexity,
        reason = "the tuple follows the (source, target, groups) layout documented on the field"
    )]
    associations: Vec<(AssociationKind, (MaybeEntity, MaybeEntity, Vec<String>))>,
    /// The hashes for reactions that we are spawning and their indexes
    pending_reactions: HashMap<u64, usize>,
    /// The reactions that we are spawning
    reactions: Vec<ReactionRequest>,
}

impl SigmaActionCache {
    /// Create a new action cache
    ///
    /// # Arguments
    ///
    /// * `key` - The key to the original data are scanning sigma events from
    pub fn new(key: OutputKey) -> Self {
        SigmaActionCache {
            key,
            pending_entities: HashMap::default(),
            entities: Vec::default(),
            associations: Vec::default(),
            pending_reactions: HashMap::default(),
            reactions: Vec::default(),
        }
    }

    /// Add a reaction to spawn or merge its tags into an identical request
    ///
    /// This returns true if this reaction was newly collected and false if an
    /// identical request was already collected and only its tags were merged.
    ///
    /// # Arguments
    ///
    /// * `hash` - The dedupe hash for this reaction request
    /// * `req` - The reaction request to add
    pub fn add_reaction(&mut self, hash: u64, req: ReactionRequest) -> bool {
        // check if we have already collected an identical request
        match self.pending_reactions.entry(hash) {
            // another rule already collected this request so just merge our tags
            Entry::Occupied(entry) => {
                // get the already collected request
                if let Some(existing) = self.reactions.get_mut(*entry.get()) {
                    // add any of our tags that are not already set
                    for tag in req.tags {
                        // only add new tags
                        if !existing.tags.contains(&tag) {
                            existing.tags.push(tag);
                        }
                    }
                }
                false
            }
            // this is a new request so add it
            Entry::Vacant(entry) => {
                // track the index this request will be added at
                entry.insert(self.reactions.len());
                // add this reaction request
                self.reactions.push(req);
                true
            }
        }
    }

    /// Add a new entity to create to our cache and return the index this for this new entity
    ///
    /// # Arguments
    ///
    /// * `rule` - The compiled that is causing us to create this entity
    /// * `req` - The entity to add
    /// * `stats` - The stats to use for tracking what entities have been created
    pub fn add_entity(
        &mut self,
        rule: &CompiledSigmaRule,
        mut req: EntityRequest,
        stats: &mut SigmaRuleStats,
    ) -> usize {
        // get the hash for this entity request
        let mut hasher = GxHasher::default();
        // hash this entity req
        req.metadata.hash(&mut hasher);
        // get this entities hash
        let hash = hasher.finish();
        // check if we already have added this entity
        match self.pending_entities.entry(hash) {
            // we already have added this entity
            Entry::Occupied(entry) => {
                // get the entities index in our entity vec
                let index = *entry.get();
                // get our existing entities groups that we may need to update
                let groups = &mut self.entities[index].groups;
                // get only the groups that are not already  in our group set
                for new_group in &rule.original.groups {
                    // only add the new groups
                    if !groups.contains(new_group) {
                        // add this group
                        groups.push(new_group.clone());
                    }
                }
                index
            }
            // we do not yet have this entity
            Entry::Vacant(entry) => {
                // get this entities stats
                let entity_stats = &mut stats.by_entity[req.kind()];
                // track that we are creating one of these entities
                entity_stats.created = entity_stats.created.saturating_add(1);
                // set the groups this entity should be created in
                req.groups.clone_from(&rule.original.groups);
                // get the index our next entity will be added at
                let index = self.entities.len();
                // add this entity request
                self.entities.push(req);
                // insert this entities new index
                entry.insert(index);
                index
            }
        }
    }

    /// Add a new association to create
    ///
    /// # Arguments
    ///
    /// * `rule` - The compiled that is causing us to create this association
    /// * `kind` - The kind of association to create
    /// * `source` - The source side of this association
    /// * `target` - The target side of this association
    pub fn add_association<S: Into<MaybeEntity>, T: Into<MaybeEntity>>(
        &mut self,
        rule: &CompiledSigmaRule,
        kind: AssociationKind,
        source: S,
        target: T,
    ) {
        // add this new association to create
        self.associations.push((
            kind,
            (source.into(), target.into(), rule.original.groups.clone()),
        ));
    }

    /// Gather all parent entities
    ///
    /// # Arguments
    ///
    /// * `thorium` - A Thorium client
    /// * `rule` - The compiled sigma rule that hit on some entity
    /// * `entity` - The entity to look for parents for
    /// * `entity_map` - The map of entities to look for parents in
    /// * `position` - The position to start looking at
    /// * `stats` - The stats to keep updated
    /// * `parents` - The set of parent entities to add too
    #[async_recursion::async_recursion]
    #[expect(clippy::too_many_arguments)]
    #[instrument(name = "SigmaActionCache::gather_parent_entities", skip_all)]
    async fn gather_parent_entities(
        &mut self,
        thorium: &Thorium,
        rule: &CompiledSigmaRule,
        entity: &EntityRequest,
        entity_map: &EntityMap,
        position: usize,
        stats: &mut SigmaRuleStats,
        parents: &mut Vec<(usize, EntityKinds)>,
    ) -> Result<(), Error> {
        // get the parent info for this entity if it has any parents
        if let Some(parent_info) = entity.parent_info() {
            // get the entities to search for our parent in
            let entities = entity_map.get(parent_info.entity_kind());
            // look for our parent process if it exists above us
            if let Some((index, parent_entity)) = entities[..position]
                .iter()
                .enumerate()
                .rev()
                .find(|(_, req)| parent_info.is_parent(req))
            {
                // we have a parent entity so check if that has its own parent
                // gather any parents for our parents (grandparents)
                let index = self
                    .gather_parents(thorium, rule, parent_entity, entity_map, index, stats)
                    .await?;
                // set our parent index
                parents.push((index, parent_info.entity_kind()));
            }
        }
        Ok(())
    }

    /// Link the end of a chain of entities to our original data
    ///
    /// # Arguments
    ///
    /// * `thorium` - A Thorium client
    /// * `rule` - The compiled sigma rule that hit on some entity
    /// * `root_kind` - The kind of root/end entity that we are linking
    /// * `root_index` - The index to the root/end entity that we are linking
    #[instrument(name = "SigmaActionCache::link_to_original", skip_all)]
    pub async fn link_to_original(
        &mut self,
        thorium: &Thorium,
        rule: &CompiledSigmaRule,
        root_kind: EntityKinds,
        root_index: usize,
    ) -> Result<(), Error> {
        // get the association kind between our original data and our root
        match &self.key {
            // get the association kind back to sample or repo
            OutputKey::Sample(sha256) => {
                // get the association kind between our original data and our root
                let kind = AssociationKind::to_parent_nonentity(root_kind);
                // build the sample association target to associate against
                let parent_target = AssociationTarget::File(sha256.clone());
                // add the association between our root node and our original data
                self.add_association(rule, kind, parent_target, root_index);
            }
            OutputKey::Repo(url) => {
                // get the association kind between our original data and our root
                let kind = AssociationKind::to_parent_nonentity(root_kind);
                // build the sample association target to associate against
                let parent_target = AssociationTarget::Repo(url.clone());
                // add the association between our root node and our original data
                self.add_association(rule, kind, parent_target, root_index);
            }
            // get the association kind back to an entity
            OutputKey::Entity(entity_id) => {
                // get this entity if it exists
                let entity = thorium.entities.get(*entity_id).await?;
                // get this entities kind
                let kind = entity.kind;
                // get the association kind between our original data and our root
                let kind = AssociationKind::from((kind, root_kind));
                // add the association between our root node and our original data
                self.add_association(rule, kind, entity, root_index);
            }
        }
        Ok(())
    }

    /// Gather all parent entities into this cache
    ///
    /// This returns the index to the child entity that we are gathering parents for.
    ///
    /// # Arguments
    ///
    /// * `rule` - The compiled sigma rule that hit on some entity
    /// * `entity` - The entity to look for parents for
    /// * `entity_map` - The map of entities to look for parents in
    /// * `position` - The position to start looking at
    /// * `stats` - The stats to keep updated
    #[async_recursion::async_recursion]
    #[instrument(name = "SigmaActionCache::gather_parents", skip_all)]
    pub async fn gather_parents(
        &mut self,
        thorium: &Thorium,
        rule: &CompiledSigmaRule,
        entity: &EntityRequest,
        entity_map: &EntityMap,
        position: usize,
        stats: &mut SigmaRuleStats,
    ) -> Result<usize, Error> {
        // track if we found any parent's index
        let mut parents = Vec::default();
        // get any parent entities from this entity
        self.gather_parent_entities(
            thorium,
            rule,
            entity,
            entity_map,
            position,
            stats,
            &mut parents,
        )
        .await?;
        // get the kind of entity for this child
        let child_kind = entity.kind();
        // we always add ourselves after trying to find our parent
        let child_index = self.add_entity(rule, entity.clone(), stats);
        // if we have any parents then we don't need to check for a root node kind
        if parents.is_empty() {
            // check if we have a root node to create
            if let Some(root_kind) = entity.root_kind() {
                // get our root nodes if we have any
                let entities = entity_map.get(root_kind);
                // for now just assume the first one
                match entities.first() {
                    // we have a root so link our chain through it
                    Some(root) => {
                        // add our root entity and get its index
                        let root_index = self.add_entity(rule, root.clone(), stats);
                        // get the association kind between these entities
                        let kind = AssociationKind::from((root_kind, child_kind));
                        // add the association to create
                        self.add_association(rule, kind, root_index, child_index);
                        // link our root node back to our original data
                        self.link_to_original(thorium, rule, root_kind, root_index)
                            .await?;
                    }
                    // results are not required to include root entities (e.g.
                    // processes can be uploaded without a process tree) so link
                    // this chain top back to our original data instead
                    None => {
                        self.link_to_original(thorium, rule, child_kind, child_index)
                            .await?;
                    }
                }
            } else {
                // just link the final child back to our original data
                self.link_to_original(thorium, rule, child_kind, child_index)
                    .await?;
            }
        } else {
            // add associations to all of our our parents
            for (parent_index, parent_kind) in parents {
                // get the association kind between these entities
                let kind = AssociationKind::from((parent_kind, child_kind));
                // add the association to create
                self.add_association(rule, kind, parent_index, child_index);
            }
        }
        Ok(child_index)
    }

    /// Determine which entities we need to create, skip, or update
    ///
    /// # Arguments
    ///
    /// * `thorium` - A Thorium client
    async fn filter_reqs(&mut self, thorium: &Thorium) -> Result<Vec<MaybeEntityCreate>, Error> {
        // build the query for getting the existing tree of entities
        let query = TreeQuery::from(self.key.clone());
        // we don't need to gather parents
        // TODO set better defaults to help limit what we crawl and speed up tree
        // growth
        let opts = TreeOpts::default().limit(6);
        // get a tree starting at our events source
        let tree = thorium.trees.start(&opts, &query).await?;
        // keep a map of entities that already exist
        let mut exists = HashMap::with_capacity(tree.data_map.len());
        // turn this tree into a map of entities
        for (_, node) in tree.data_map {
            // we only care about entity nodes
            if let TreeNode::Entity(entity) = node {
                // get this entities identifying hash
                let hash = entity.hash_identifying();
                // add this entity into our map
                exists.insert(hash, entity);
            }
        }
        // keep a list of what entities we should create
        let mut to_create = Vec::with_capacity(self.entities.len());
        // step over the entities we want to create and find out which should
        // be created, updated, or skipped
        for req in self.entities.drain(..) {
            // get this potential new entities hash
            let hash = req.hash_identifying();
            // get this entity if it already exists
            match exists.get(&hash) {
                Some(entity) => {
                    // build an association target for this entity
                    let target = AssociationTarget::Entity {
                        id: entity.id,
                        name: entity.name.clone(),
                    };
                    // this is an existing entity so we don't need to create it
                    to_create.push(MaybeEntityCreate::Exists(target));
                }
                // this is a new entity so create it
                None => to_create.push(MaybeEntityCreate::Create(req)),
            }
        }
        Ok(to_create)
    }

    /// Create all of our entities (including flags) and associations
    ///
    /// Existing entities are looked up as the user, but the creates use the worker's base (admin)
    /// client so sigma's entities and associations keep being attributed to it.
    ///
    /// # Arguments
    ///
    /// * `thorium` - The thorium client masquerading as the user whose event this is
    /// * `creator` - The base (admin) thorium client to create entities and associations with
    pub async fn create_all(mut self, thorium: &Thorium, creator: &Thorium) -> Result<(), Error> {
        // don't just recreate any already existing entities
        let maybe_creates = self.filter_reqs(thorium).await?;
        // create all of our entities 10 at a time
        let targets = futures::stream::iter(maybe_creates)
            .map(|maybe_create| maybe_create.apply(creator))
            .buffered(10)
            .collect::<Vec<Result<AssociationTarget, _>>>()
            .await;
        // pre allocate a vector to store our entity creation responses
        let mut resps = Vec::with_capacity(targets.len());
        // log any failures
        for result in targets {
            // log if this create failed
            match result {
                Ok(resp) => resps.push(Some(resp)),
                Err(error) => {
                    // log that we failed to create an entity
                    event!(Level::ERROR, creating = "Entity", %error);
                    // add a none so that we don't try to create dangling association
                    resps.push(None);
                }
            }
        }
        // preallocate a list to store our association requests
        let mut assoc_reqs = Vec::with_capacity(self.associations.len());
        // create the association requests for our new entities
        for (kind, (src, target, groups)) in self.associations {
            // get our source and target association targets
            // log an error if either of our targets are missing
            let (Some(src), Some(target)) = (src.get_target(&resps), target.get_target(&resps))
            else {
                // log that we failed to create an association
                event!(
                    Level::ERROR,
                    creating = "Association",
                    error = "Missing Source/Destination entity"
                );
                // continue on to the next association to create
                continue;
            };
            // build this association request
            let req = AssociationRequest::new(kind, src)
                .target(target)
                .groups(groups);
            // add this request
            assoc_reqs.push(req);
        }
        // create all of our associations 10 at a time
        let resp_results = futures::stream::iter(assoc_reqs)
            .map(|req| create_association(creator, req))
            .buffer_unordered(10)
            .collect::<Vec<Result<(), _>>>()
            .await;
        // log any failures
        for result in resp_results {
            // log if this create failed
            if let Err(error) = result {
                // log that we failed to create an association
                event!(
                    Level::ERROR,
                    creating = "Association",
                    error = error.to_string()
                );
            }
        }
        Ok(())
    }
}

/// Spawn all of the reactions that any rule hits collected
///
/// This must only be called after all fallible scan steps so that a retried
/// event cannot respawn reactions that were already created. Failures to create
/// single reactions are logged and counted but never bubbled up for the same
/// reason.
///
/// # Arguments
///
/// * `reqs` - The reactions to spawn
/// * `thorium` - A Thorium client
/// * `stats` - The stats to track spawned reactions in
#[instrument(name = "spawn_reactions", skip_all, fields(reqs = reqs.len()))]
async fn spawn_reactions(
    reqs: Vec<ReactionRequest>,
    thorium: &Thorium,
    stats: &mut SigmaRuleStats,
) {
    // skip spawning if no reactions were collected
    if reqs.is_empty() {
        return;
    }
    // create all of our reactions 10 at a time
    let results = futures::stream::iter(reqs)
        .map(|req| async move {
            // create this reaction and keep the request for error logs
            let result = thorium.reactions.create(&req).await;
            (req, result)
        })
        .buffer_unordered(10)
        .collect::<Vec<_>>()
        .await;
    // log any failures and track our stats
    for (req, result) in results {
        // check if this reaction was created
        match result {
            // track that we spawned a reaction
            Ok(_) => stats.total.spawned = stats.total.spawned.saturating_add(1),
            Err(error) => {
                // log that we failed to create a reaction
                event!(
                    Level::ERROR,
                    creating = "Reaction",
                    group = &req.group,
                    pipeline = &req.pipeline,
                    error = error.to_string()
                );
                // track that this spawn failed
                stats.total.spawn_errors = stats.total.spawn_errors.saturating_add(1);
            }
        }
    }
}

/// Render a spawn actions jq filter args against a matched entitys metadata json
///
/// # Arguments
///
/// * `spawn` - The spawn reaction action to render args for
/// * `entity_json` - The matched entitys metadata json
#[instrument(name = "render_spawn_args", skip_all, err(Debug))]
fn render_spawn_args(
    spawn: &SigmaSpawnReaction,
    entity_json: &serde_json::Value,
) -> Result<BTreeMap<String, GenericJobArgs>, Error> {
    // start with an empty map of rendered args
    let mut rendered = BTreeMap::default();
    // render the args for each image in this spawn action
    for (image, args) in &spawn.args {
        // start with room for one output per positional filter
        let mut positionals = Vec::with_capacity(args.positionals.len());
        // run each positional filter against our entity
        for filter in &args.positionals {
            // compile and run this filter against our entity
            let outputs = CompiledJq::new(filter)
                .and_then(|compiled| compiled.run(entity_json))
                .map_err(|error| Error::new(error.to_string()))?;
            // add this filters outputs as positionals in order
            positionals.extend(outputs);
        }
        // start with an empty map of kwargs
        let mut kwargs = BTreeMap::default();
        // run the filters for each kwarg against our entity
        for (key, filters) in &args.kwargs {
            // start with room for one output per kwarg filter
            let mut values = Vec::with_capacity(filters.len());
            // run each of this kwargs filters against our entity
            for filter in filters {
                // compile and run this filter against our entity
                let outputs = CompiledJq::new(filter)
                    .and_then(|compiled| compiled.run(entity_json))
                    .map_err(|error| Error::new(error.to_string()))?;
                // add this filters outputs as values in order
                values.extend(outputs);
            }
            // add this kwargs rendered values
            kwargs.insert(key.clone(), values);
        }
        // build this images job args with our rendered values
        let job_args = GenericJobArgs::default()
            .positionals(positionals)
            .set_kwargs(kwargs)
            .switches(args.switches.clone());
        // add this images rendered args
        rendered.insert(image.clone(), job_args);
    }
    Ok(rendered)
}

/// Hash a spawn request so duplicate spawns can be deduped within a single event
///
/// # Arguments
///
/// * `group` - The group the pipeline to spawn is in
/// * `pipeline` - The pipeline to spawn
/// * `rendered` - The rendered args for this spawn request
#[instrument(name = "hash_spawn_request", skip_all, err(Debug))]
fn hash_spawn_request(
    group: &str,
    pipeline: &str,
    rendered: &BTreeMap<String, GenericJobArgs>,
) -> Result<u64, Error> {
    // serialize our rendered args since job args are not directly hashable
    let serialized = serde_json::to_string(rendered)?;
    // get the hasher to hash this spawn request with
    let mut hasher = GxHasher::default();
    // hash the target pipeline and its rendered args
    group.hash(&mut hasher);
    pipeline.hash(&mut hasher);
    serialized.hash(&mut hasher);
    // get this spawn requests hash
    Ok(hasher.finish())
}

/// A map of entities and events sorted by what kind of entity this is
#[derive(Default)]
struct EntityMap {
    /// The inner map of entities
    pub entities: StaticMap<EntityKinds, Vec<EntityRequest>>,
    /// A map of entities turned into scannable events
    pub events: StaticMap<EntityKinds, Vec<sigma_rust::Event>>,
}

impl EntityMap {
    /// Get a specific kind of entity to add to our map
    ///
    /// # Arguments
    ///
    /// * `event` - The event we are populating entities for
    /// * `result_id` - The id of the results we are scanning entities from
    /// * `kind` - The kind of entities to get
    /// * `thorium` - A thorium client
    /// * `local_stats` - The local stats for this worker
    pub async fn populate(
        &mut self,
        event: &SigmaScannableResultsEvent,
        result_id: Uuid,
        kind: EntityKinds,
        thorium: &Thorium,
        local_stats: &mut SigmaRuleStats,
    ) -> Result<(), Error> {
        // download this kinds entity requests for this result
        let entities = event
            .key
            .get_entities(thorium, &event.tool, result_id, kind)
            .await?;
        // eagerly convert this entities into scannable rules
        for req in entities {
            // this kind should always be equal to the kind we requested
            // however to be defensive we will get the kind for each entity
            let req_kind = req.kind();
            // get this entities stats
            let entity_stats = &mut local_stats.by_entity[req_kind];
            // track that we scanned one of these entities
            entity_stats.scanned = entity_stats.scanned.saturating_add(1);
            // convert this entity to something that sigma can scan if possible
            if let Some(scannable) = req.metadata.to_sigma_scannable()? {
                // wrap this in a sigma event
                let sigma_event = sigma_rust::event_from_json(&scannable)?;
                // keep track of this event
                self.events[req_kind].push(sigma_event);
            }
            // add this to our entity map
            self.entities[req_kind].push(req);
        }
        Ok(())
    }

    /// Populate a new entity map
    ///
    /// # Arguments
    ///
    /// * `event` - The event we are creating an entity map for
    /// * `thorium` - A thorium client
    /// * `local_stats` - The local stats for this worker
    pub async fn new(
        event: &SigmaScannableResultsEvent,
        thorium: &Thorium,
        local_stats: &mut SigmaRuleStats,
    ) -> Result<Self, Error> {
        // get the results for this event
        let mut results = event.key.get_results(thorium, &event.tool).await?;
        // get the latest results
        let result = match results.results.remove(&event.tool) {
            Some(mut results) => results.swap_remove(0),
            // we can't create an entity map
            // this should basically never happen and events should be small enough
            // that we can just log the whole event to make debugging easier
            None => return Err(Error::new(format!("Missing results for {event:?}"))),
        };
        // start with a empty map of the different entities
        let mut entity_map = EntityMap::default();
        //  step over the entity kinds in this event
        for applies_to in &event.applies_to {
            // get the kind of entity this applies to maps too
            let entity_kind = EntityKinds::from(*applies_to);
            // skip kinds this result has no entities for since downloading an
            // absent kinds entities fails; the newest result for this tool is
            // fetched so a kind the event applies to can be absent when a newer
            // result shadowed the one that created this event
            if !result.entities.contains_key(&entity_kind) {
                // warn since this skips scanning entities the event was created for
                event!(
                    Level::WARN,
                    msg = "Newest result is missing entities this event applies to; skipping that kinds scan",
                    tool = &event.tool,
                    kind = entity_kind.as_str(),
                );
                continue;
            }
            // get the entities for this kind
            entity_map
                .populate(event, result.id, entity_kind, thorium, local_stats)
                .await?;
            // also get any root entities for this entity kind
            for root_kind in entity_kind.root_kinds() {
                // results are not required to include root entities
                // (e.g. processes can be uploaded without a process tree)
                if !result.entities.contains_key(root_kind) {
                    continue;
                }
                // get the entities for this kind
                entity_map
                    .populate(event, result.id, *root_kind, thorium, local_stats)
                    .await?;
            }
        }
        Ok(entity_map)
    }

    /// Get the entities for a kind of entity
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of events and entities to get
    pub fn get(&self, kind: EntityKinds) -> &Vec<EntityRequest> {
        &self.entities[kind]
    }
}

/// Check if an error is a 401 caused by masquerading as a disabled user
///
/// # Arguments
///
/// * `error` - The error to inspect
fn is_disabled_error(error: &Error) -> bool {
    // this must be a 401 unauthorized error
    if error.status() != Some(reqwest::StatusCode::UNAUTHORIZED) {
        return false;
    }
    // check if the response body says this user has been disabled
    error
        .msg()
        .is_some_and(|msg| msg.contains("has been disabled"))
}

/// A single sigma rule worker
pub struct SigmaRuleWorker {
    /// The context for this sigma rule worker
    context: SigmaRuleContext,
    /// A map of Thorium clients for users in Thorium
    pub clients: PapayaMap<String, Thorium>,
    /// The users that are currently disabled and whose events should be skipped
    pub disabled: HashSet<String>,
    /// The max trigger depth at which sigma rule hits may still spawn reactions
    max_depth: u8,
    /// The base (admin) client that builds user clients and creates sigma's entities and associations
    thorium: Arc<Thorium>,
}

impl SigmaRuleWorker {
    /// Create a client for all of our users
    ///
    /// Disabled users do not get a client since masquerading as them would
    /// only fail; any stale client for a now disabled user is removed instead.
    ///
    /// # Arguments
    ///
    /// * `users` - The users to create a client for
    pub fn create_user_clients(&mut self, users: HashMap<String, ScrubbedUser>) {
        // get a pin to our client map
        let pin = self.clients.pin();
        for (username, user) in users {
            // skip disabled users since masquerading as them would only fail
            if user.role == UserRole::Disabled {
                // remove any stale client for this now disabled user
                pin.remove(&username);
                // track that this user is disabled so we can skip their events
                self.disabled.insert(username);
                // don't build a client for this user
                continue;
            }
            // this user is enabled so make sure they are not tracked as disabled
            self.disabled.remove(&username);
            // build a client for each user
            let mut user_client = self.thorium.deref().clone();
            // set this client to masquerade as this user
            user_client.masquerade(&user);
            // add a client for this user
            pin.insert(username, user_client);
        }
    }

    /// Check if spawning a reaction must be refused because an event is at or past the max depth
    ///
    /// This mirrors the trigger cache max depth guard so sigma spawned reactions
    /// cannot recurse past the configured max trigger depth.
    ///
    /// # Arguments
    ///
    /// * `depth` - The depth of the event being scanned
    /// * `max_depth` - The max depth an event chain may reach
    fn spawn_depth_capped(depth: u8, max_depth: u8) -> bool {
        // refuse to spawn once an events depth reaches the max trigger depth
        depth >= max_depth
    }

    /// Build and collect a reaction to spawn for a sigma rule hit
    ///
    /// Any failures are logged and counted instead of returned so that one bad
    /// action cannot poison the rest of this events scan.
    ///
    /// # Arguments
    ///
    /// * `action_cache` - A cache of actions that this worker will eventually take
    /// * `rule` - The rule that hit and wants to spawn a reaction
    /// * `spawn` - The spawn reaction action to take
    /// * `hit_on` - The entity that this rule hit on
    /// * `event_depth` - The trigger depth of the event we are scanning
    /// * `stats` - The local stats for this worker
    #[instrument(name = "SigmaRuleWorker::collect_spawn", skip_all)]
    async fn collect_spawn(
        &self,
        action_cache: &mut SigmaActionCache,
        rule: &CompiledSigmaRule,
        spawn: &SigmaSpawnReaction,
        hit_on: &EntityRequest,
        event_depth: u8,
        stats: &mut SigmaRuleStats,
    ) {
        // try to collect a reaction to spawn for this rule hit
        if let Err(error) = self
            .try_collect_spawn(action_cache, rule, spawn, hit_on, event_depth, stats)
            .await
        {
            // log that we skipped spawning a reaction for this hit
            event!(
                Level::WARN,
                rule = &rule.original.name,
                error = error.to_string(),
                msg = "Skipping reaction spawn"
            );
            // get this entity kinds stats
            let entity_stats = &mut stats.by_entity[hit_on.kind()];
            // track that spawning for this entity kind failed or was refused
            entity_stats.spawn_errors = entity_stats.spawn_errors.saturating_add(1);
        }
    }

    /// Try to build and collect a reaction to spawn for a sigma rule hit
    ///
    /// Spawn requests are deduped within a single event; a duplicate request is
    /// not an error and instead just adds this rules tag to the already
    /// collected request.
    ///
    /// # Arguments
    ///
    /// * `action_cache` - A cache of actions that this worker will eventually take
    /// * `rule` - The rule that hit and wants to spawn a reaction
    /// * `spawn` - The spawn reaction action to take
    /// * `hit_on` - The entity that this rule hit on
    /// * `event_depth` - The trigger depth of the event we are scanning
    /// * `stats` - The local stats for this worker
    #[instrument(name = "SigmaRuleWorker::try_collect_spawn", skip_all, err(Debug))]
    async fn try_collect_spawn(
        &self,
        action_cache: &mut SigmaActionCache,
        rule: &CompiledSigmaRule,
        spawn: &SigmaSpawnReaction,
        hit_on: &EntityRequest,
        event_depth: u8,
        stats: &mut SigmaRuleStats,
    ) -> Result<(), Error> {
        // refuse to spawn reactions at or past the max trigger depth
        if Self::spawn_depth_capped(event_depth, self.max_depth) {
            return Err(Error::new(format!(
                "Refusing to spawn a reaction at max trigger depth: {event_depth} >= {}",
                self.max_depth
            )));
        }
        // reactions can only be spawned on sample or repo backed results
        if let OutputKey::Entity(_) = &action_cache.key {
            return Err(Error::new(
                "Spawning reactions is not supported for entity results",
            ));
        }
        // hash this spawn action so timed out renders can be tracked across events
        let action_hash = hash_spawn_action(spawn);
        // make sure this actions filters may still be rendered
        check_render_budget(&TIMED_OUT_SPAWNS, &LEAKED_JQ_THREADS, action_hash)?;
        // get the metadata json for the entity this rule hit on
        // a scanned entity should always be scannable but be defensive anyways
        let scannable = hit_on
            .metadata
            .to_sigma_scannable()?
            .ok_or_else(|| Error::new("Matched entity is not sigma scannable"))?;
        // parse this entities metadata json so we can run jq filters against it
        let entity_json = serde_json::from_str::<serde_json::Value>(&scannable)?;
        // clone this spawn action so it can be rendered on a blocking thread
        let to_render = spawn.clone();
        // render this spawn actions jq filters against our entity on a blocking
        // thread with a timeout so pathological filters that loop forever cannot
        // wedge this worker
        let render =
            tokio::task::spawn_blocking(move || render_spawn_args(&to_render, &entity_json));
        // wait for the render to finish or fail this spawn once it times out
        let rendered = match tokio::time::timeout(JQ_RENDER_TIMEOUT, render).await {
            // rendering completed so bubble up any jq failures
            Ok(joined) => joined.map_err(|error| Error::new(error.to_string()))??,
            // fail closed on filters that did not finish in time; the leaked
            // thread spins until this process restarts so record it and never
            // rerender this action
            Err(_) => {
                // record this timed out render and the thread it leaked
                record_render_timeout(&TIMED_OUT_SPAWNS, &LEAKED_JQ_THREADS, action_hash);
                return Err(Error::new(format!(
                    "jq arg extraction timed out after {}s; this action is skipped \
                    until the event handler restarts",
                    JQ_RENDER_TIMEOUT.as_secs()
                )));
            }
        };
        // hash this spawn request so we can dedupe duplicate spawns
        let hash = hash_spawn_request(&spawn.group, &spawn.pipeline, &rendered)?;
        // build the base reaction request for this spawn action
        // spawned reactions are one trigger deeper than the event that caused them
        let req = ReactionRequest::new(spawn.group.clone(), spawn.pipeline.clone())
            .trigger_depth(event_depth.saturating_add(1))
            // tag this reaction with the rule that spawned it
            .tag(format!("sigma-rule:{}", rule.original.name));
        // target the data that the originating reaction ran on; repo results
        // only record the repo url so repo spawns run against the repos
        // default checkout rather than the originating commit
        let mut req = match &action_cache.key {
            OutputKey::Sample(sha256) => req.sample(sha256.clone()),
            OutputKey::Repo(url) => req.repo(RepoDependencyRequest::new(url.clone())),
            // entity keyed results were already rejected above
            OutputKey::Entity(_) => return Ok(()),
        };
        // overlay the rendered args for each image
        for (image, args) in rendered {
            // add this images rendered args
            req = req.args(image, args);
        }
        // add this reaction or merge its tags into an identical collected one
        if action_cache.add_reaction(hash, req) {
            // get this entity kinds stats
            let entity_stats = &mut stats.by_entity[hit_on.kind()];
            // track that we are spawning a reaction for this entity kind
            entity_stats.spawned = entity_stats.spawned.saturating_add(1);
        }
        Ok(())
    }

    /// Create a flag for this rule hit
    ///
    /// # Arguments
    ///
    /// * `thorium` - A thorium client
    /// * `action_cache` - A cache of actions that this worker will eventually take
    /// * `rule` - The rule we are creating a flag for
    /// * `auto_flag` - The flag to create
    /// * `flagged_on` - The entity that we are flagging
    /// * `position` - The index of the entity we are flagging
    /// * `stats` - The local stats for this worker
    #[expect(clippy::too_many_arguments)]
    #[instrument(name = "SigmaRuleWorker::create_flag", skip_all)]
    async fn create_flag(
        &self,
        thorium: &Thorium,
        action_cache: &mut SigmaActionCache,
        rule: &CompiledSigmaRule,
        auto_flag: &SigmaAutoFlag,
        flagged_on: &EntityRequest,
        entity_map: &EntityMap,
        position: usize,
        stats: &mut SigmaRuleStats,
    ) -> Result<usize, Error> {
        // get the original rules metadata
        let rule_meta = rule.get_original_rule()?;
        // build the flag for this sigma rule
        let flag = auto_flag.to_flag(rule_meta);
        // build the metadata request for this flag
        let metadata = EntityMetadataRequest::Flag(flag);
        // use the same groups as our rule
        let groups = rule.original.groups.clone();
        // build the entity request for this flag
        let entity_req = EntityRequest::new(&rule.original.name, metadata, groups);
        // add the entity we flagged on and any parents we need to create
        let index = action_cache
            .gather_parents(thorium, rule, flagged_on, entity_map, position, stats)
            .await?;
        // add our flag entity to our action cache
        let flag_index = action_cache.add_entity(rule, entity_req, stats);
        // add the info to associate our flag with our entity
        action_cache.add_association(rule, AssociationKind::FlagFor, flag_index, index);
        // our original rule entity already exists
        let original = MaybeEntity::new_existing(rule.original.id, &rule.original.name);
        // add the info to associate this flag with its source sigma rule
        action_cache.add_association(rule, AssociationKind::CreatedBy, original, flag_index);
        // get this entities stats
        let entity_stats = &mut stats.by_entity[flagged_on.kind()];
        // track that we are creating a flag for this entity kind
        entity_stats.flagged = entity_stats.flagged.saturating_add(1);
        Ok(index)
    }

    /// Scan a single entity
    ///
    /// # Arguments
    ///
    /// * `event` - The event to scan
    #[instrument(name = "SigmaRuleWorker::scan_event", skip_all)]
    pub async fn scan_event(&self, event: Event) -> Result<SigmaRuleStats, Error> {
        // skip events from disabled users since masquerading as them would only fail
        if self.disabled.contains(&event.user) {
            // log that we are skipping this disabled users event
            event!(Level::INFO, disabled_user = &event.user, skipped = true);
            // return empty stats since we did no work; this mirrors a successful scan
            return Ok(SigmaRuleStats::default());
        }
        // get a pin to our papaya map
        let pin = self.clients.pin_owned();
        // get this users thorium client
        let Some(thorium) = pin.get(&event.user) else {
            // without a client for this user we can't scan this event, so error and let it be
            // retried later
            return Err(Error::new(format!(
                "Missing client for user {}",
                event.user
            )));
        };
        // start with an empty stats block then we will lock and add everything all at once later
        let mut local_stats = SigmaRuleStats::default();
        // keep this events trigger depth so spawned reactions can go one deeper
        let depth = event.depth;
        // get our innner event
        // this should never hit the wrong event kind branch but its better to be defensive
        let EventData::SigmaScannableResults(event_data) = event.data else {
            // we got an event that is not meant for this worker somehow
            // build a helpful error message
            let msg = format!("SigmaRuleWorker got the wrong kind of event: {}", event.id);
            // return an error since we can't handle this event
            return Err(Error::new(msg));
        };
        // A cache of actions to take
        let mut action_cache = SigmaActionCache::new(event_data.key.clone());
        // increment the total number of scanned events
        local_stats.total.scanned = local_stats.total.scanned.saturating_add(1);
        // Get all of the entities for this event
        let entity_map = EntityMap::new(&event_data, thorium, &mut local_stats).await?;
        // scan using any relevant rules
        for applies_to in &event_data.applies_to {
            // get the entity kind for this applies to
            let entity_kind = EntityKinds::from(applies_to);
            // scan using these rules
            for index in &self.context.applies_to[entity_kind] {
                // get this rule
                let Some(rule) = self.context.combined.get(*index) else {
                    // we are somehow missing a rule so log it
                    event!(
                        Level::ERROR,
                        "Missing rule for {entity_kind} at index {index}"
                    );
                    // just continue on to the next rule
                    continue;
                };
                // get a reference to this entity kinds events
                let events = &entity_map.events[entity_kind];
                // scan the entities this rule applies too
                for (index, entity_req) in entity_map.get(entity_kind).iter().enumerate() {
                    // get this entities event
                    let sigma_event = &events[index];
                    // scan this event
                    if rule.compiled.is_match(sigma_event) {
                        // get this rules metadata
                        let rule_meta = rule.get_original_rule()?;
                        // get all of the actions to take when this rule hits
                        for action in &rule_meta.actions {
                            // perform the correct action
                            match action {
                                // Create a flag on the data we hit on
                                SigmaActionToTake::Flag(auto_flag) => {
                                    self.create_flag(
                                        thorium,
                                        &mut action_cache,
                                        rule,
                                        auto_flag,
                                        entity_req,
                                        &entity_map,
                                        index,
                                        &mut local_stats,
                                    )
                                    .await?;
                                }
                                // spawn a reaction on the data these results came from
                                SigmaActionToTake::SpawnReaction(spawn) => {
                                    self.collect_spawn(
                                        &mut action_cache,
                                        rule,
                                        spawn,
                                        entity_req,
                                        depth,
                                        &mut local_stats,
                                    )
                                    .await;
                                }
                            }
                        }
                    }
                }
            }
        }
        // pull any reactions collected by rule hits out before the cache is consumed
        let spawn_reqs = std::mem::take(&mut action_cache.reactions);
        // create any entities that any rules hit/flagged on with our base (admin) client
        // this must happen before spawning reactions since its errors retry the
        // event and a retried event would respawn already spawned reactions
        action_cache.create_all(thorium, &self.thorium).await?;
        // spawn any reactions that any rule hits collected as this events user
        spawn_reactions(spawn_reqs, thorium, &mut local_stats).await;
        Ok(local_stats)
    }
}

impl EventWorkerSupportCore for SigmaRuleWorker {
    /// The stats for all workers of this kind
    type Stats = SigmaRuleStats;

    /// Build this specific kind of worker
    ///
    /// # Arguments
    ///
    /// * `cache` - A cache of all info a worker may need to be created
    /// * `thorium` - A Thorium client
    /// * `_worker` - The identity of this event handler process (unused since
    ///   the wrapper performs this workers queue operations)
    async fn new(cache: &EventWorkerCache, thorium: &Arc<Thorium>, _worker: &str) -> Self {
        // build this worker
        let mut worker = SigmaRuleWorker {
            context: cache.sigma.clone(),
            thorium: thorium.clone(),
            clients: PapayaMap::with_capacity(100),
            disabled: HashSet::default(),
            max_depth: cache.conf.thorium.events.max_depth,
        };
        // populate our user clients
        worker.create_user_clients(cache.users.clone());
        worker
    }

    /// The types of events this worker handles
    #[inline]
    fn event_kinds() -> Vec<EventType> {
        vec![EventType::SigmaScannableResults]
    }

    /// The types of cache updates this worker subscribes too
    #[inline]
    fn cache_subscriptions() -> Vec<WorkerCacheUpdateKinds> {
        vec![
            WorkerCacheUpdateKinds::SigmaRulesContext,
            WorkerCacheUpdateKinds::Users,
        ]
    }

    /// Apply an update to our local worker cache
    ///
    /// # Arguments
    ///
    /// * `update` - The update to apply to this workers cache
    fn apply_cache_update(&mut self, update: WorkerCacheUpdates) {
        // Apply an updates
        match update {
            WorkerCacheUpdates::ReactionTriggers(_) => (),
            WorkerCacheUpdates::SigmaRulesContext(context) => self.context = *context,
            WorkerCacheUpdates::Users(users) => self.create_user_clients(users),
            // the sigma worker does not care about yara cache updates
            WorkerCacheUpdates::YaraRulesContext(_) | WorkerCacheUpdates::GroupAllowances(_) => (),
        }
    }

    /// Report our latest stats in an event
    ///
    /// This will only report new/changed stats.
    ///
    /// # Arguments
    ///
    /// * `stats` - The new stats to log
    /// * `prior` - The previously reported stats
    #[instrument(name = "EventWorkerSupportCore::stats", skip_all)]
    fn stats(stats: &Self::Stats, prior: &Self::Stats) {
        // first report our total stats
        event!(
            Level::INFO,
            worker = "SigmaRuleworker",
            total = true,
            scanned = stats.total.scanned,
            created = stats.total.created,
            flagged = stats.total.flagged,
            spawned = stats.total.spawned,
            spawn_errors = stats.total.spawn_errors,
        );
        // now report stats on a per entity basis
        for (entity, entity_stats) in &stats.by_entity {
            // We have prior stats check if they changed
            if entity_stats == &prior.by_entity[entity] {
                // these stats didn't change so no reason to reemit the same numbers
                continue;
            }
            // report this entity kinds stats
            event!(
                Level::INFO,
                worker = "SigmaRuleworker",
                total = false,
                entity = entity.as_str(),
                scanned = entity_stats.scanned,
                created = entity_stats.created,
                flagged = entity_stats.flagged,
                spawned = entity_stats.spawned,
                spawn_errors = entity_stats.spawn_errors,
            );
        }
    }

    /// Flatten this worker kinds stats into cumulative counters for publishing
    ///
    /// The totals land under the `total.` prefix while the per entity kind
    /// breakdown flattens to `by_entity.{kind}.` dotted paths; every kind is
    /// emitted (zeros included) so the key set stays stable per worker.
    ///
    /// # Arguments
    ///
    /// * `stats` - The stats to flatten
    fn counter_map(stats: &Self::Stats) -> BTreeMap<String, u64> {
        // start with our totals under the total prefix
        let mut map = BTreeMap::from([
            ("total.scanned".to_owned(), stats.total.scanned as u64),
            ("total.created".to_owned(), stats.total.created as u64),
            ("total.flagged".to_owned(), stats.total.flagged as u64),
            ("total.spawned".to_owned(), stats.total.spawned as u64),
            (
                "total.spawn_errors".to_owned(),
                stats.total.spawn_errors as u64,
            ),
        ]);
        // flatten each entity kinds stats to dotted paths
        for (kind, entity_stats) in &stats.by_entity {
            // key this kinds counters under the by_entity prefix
            map.insert(
                format!("by_entity.{}.scanned", kind.as_str()),
                entity_stats.scanned as u64,
            );
            map.insert(
                format!("by_entity.{}.created", kind.as_str()),
                entity_stats.created as u64,
            );
            map.insert(
                format!("by_entity.{}.flagged", kind.as_str()),
                entity_stats.flagged as u64,
            );
            map.insert(
                format!("by_entity.{}.spawned", kind.as_str()),
                entity_stats.spawned as u64,
            );
            map.insert(
                format!("by_entity.{}.spawn_errors", kind.as_str()),
                entity_stats.spawn_errors as u64,
            );
        }
        map
    }
}

impl EventWorkerSupport for SigmaRuleWorker {
    /// The method to call to handle or process a single event
    async fn process(&self, event: Event, stats: &Arc<Mutex<SigmaRuleStats>>) -> Result<(), Error> {
        // keep this events user so we can log skips after the event is consumed
        let username = event.user.clone();
        // scan this event
        let local_stats = match self.scan_event(event).await {
            // this event was scanned successfully
            Ok(local_stats) => local_stats,
            // this user was disabled after our user cache was built so their
            // masqueraded calls 401; skip instead of poisoning the retry loop
            Err(error) if is_disabled_error(&error) => {
                // log that we skipped a stale disabled users event
                event!(
                    Level::INFO,
                    disabled_user = &username,
                    skipped = true,
                    stale = true,
                );
                // skip this event with empty stats like a successful no-op scan
                SigmaRuleStats::default()
            }
            // all other errors still bubble up so the event is retried later
            Err(error) => return Err(error),
        };
        // get a lock to our shared stats so we can update them
        let mut lock = stats.lock().await;
        // add our total
        lock.total += local_stats.total;
        // add each entity kind
        for (kind, stats) in local_stats.by_entity {
            // get an entry to this entity kinds stats
            lock.by_entity[kind] += stats;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    use super::*;

    /// A request the recording server received: (method, path, authorization header)
    type Recorded = (String, String, String);

    /// The token the base (admin) client authenticates with
    const ADMIN_TOKEN: &str = "admin-token";

    /// The token the masqueraded user authenticates with
    const USER_TOKEN: &str = "user-token";

    /// Find where the header block of a raw HTTP request ends
    ///
    /// # Arguments
    ///
    /// * `buf` - The bytes read so far
    fn header_end(buf: &[u8]) -> Option<usize> {
        // the header block ends at the first blank line
        buf.windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|pos| pos + 4)
    }

    /// Read one HTTP request, body included, from a connection and return its header block
    ///
    /// # Arguments
    ///
    /// * `stream` - The connection to read from
    async fn read_request(stream: &mut TcpStream) -> String {
        // read until the whole header block has arrived
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let end = loop {
            let read = stream.read(&mut chunk).await.expect("read request");
            buf.extend_from_slice(&chunk[..read]);
            if let Some(end) = header_end(&buf) {
                break end;
            }
            assert!(
                read > 0,
                "connection closed before the request headers ended"
            );
        };
        // parse the header block
        let head = String::from_utf8_lossy(&buf[..end]).to_string();
        let lower = head.to_lowercase();
        // find how the body is framed
        let length = lower
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .map(|value| value.trim().parse::<usize>().expect("content length"));
        let chunked = lower.contains("transfer-encoding: chunked");
        // read the rest of the body so the client never sees a reset
        loop {
            let body = &buf[end..];
            let complete = match (length, chunked) {
                (Some(length), _) => body.len() >= length,
                (None, true) => body.ends_with(b"0\r\n\r\n"),
                (None, false) => true,
            };
            if complete {
                break;
            }
            let read = stream.read(&mut chunk).await.expect("read body");
            assert!(read > 0, "connection closed before the request body ended");
            buf.extend_from_slice(&chunk[..read]);
        }
        head
    }

    /// Start a server that records every request and answers the sigma create path's calls
    ///
    /// Returns the server's host and the requests it has received
    async fn recording_server() -> (String, Arc<StdMutex<Vec<Recorded>>>) {
        // bind to any free local port
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let host = format!("http://{}", listener.local_addr().expect("addr"));
        let recorded = Arc::new(StdMutex::new(Vec::new()));
        let log = recorded.clone();
        // answer each connection with a canned response for its route
        tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let log = log.clone();
                tokio::spawn(async move {
                    // read and record the request
                    let head = read_request(&mut stream).await;
                    let mut parts = head.lines().next().unwrap_or_default().split(' ');
                    let method = parts.next().unwrap_or_default().to_owned();
                    let path = parts.next().unwrap_or_default().to_owned();
                    let auth = head
                        .lines()
                        .find_map(|line| {
                            line.to_lowercase()
                                .starts_with("authorization:")
                                .then(|| line["authorization:".len()..].trim().to_owned())
                        })
                        .unwrap_or_default();
                    log.lock().expect("lock").push((method, path.clone(), auth));
                    // pick the body this route answers with
                    let body = if path.starts_with("/api/trees/") {
                        format!(
                            r#"{{"id":"{}","initial":[],"growable":[],"data_map":{{}},"branches":{{}},"hint_branches":{{}}}}"#,
                            Uuid::new_v4()
                        )
                    } else if path.starts_with("/api/entities/") {
                        format!(r#"{{"id":"{}","name":"created"}}"#, Uuid::new_v4())
                    } else {
                        String::new()
                    };
                    // send the response and close the connection
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    stream.write_all(resp.as_bytes()).await.expect("write");
                    stream.shutdown().await.ok();
                });
            }
        });
        (host, recorded)
    }

    /// Build the auth string the client sends for a token
    ///
    /// # Arguments
    ///
    /// * `token` - The token to build an auth string for
    fn auth_for(token: &str) -> String {
        // the client sends base64 encoded tokens behind a "token" prefix
        format!("token {}", base64_encode(token.as_bytes()))
    }

    /// Base64 encode bytes with the standard alphabet
    ///
    /// # Arguments
    ///
    /// * `bytes` - The bytes to encode
    fn base64_encode(bytes: &[u8]) -> String {
        /// The standard base64 alphabet
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        // encode each group of three bytes as four characters
        for chunk in bytes.chunks(3) {
            // pad a short final group with zero bytes
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            // emit a character per six bits, or padding past the end of the input
            for i in 0..4 {
                if i <= chunk.len() {
                    out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    /// Sigma creates its entities and associations with its base (admin) client
    #[tokio::test]
    async fn sigma_creates_entities_and_associations_as_admin() {
        // start the recording server and build an admin client against it
        let (host, recorded) = recording_server().await;
        let admin = Thorium::build(&host)
            .token(ADMIN_TOKEN)
            .build()
            .await
            .expect("admin client");
        // masquerade a copy of the admin client as a user, as sigma's user clients do
        let user = ScrubbedUser {
            username: "hunter".to_owned(),
            role: UserRole::User,
            email: "hunter@example.com".to_owned(),
            groups: vec!["rules".to_owned()],
            actual_groups: Vec::default(),
            token: USER_TOKEN.to_owned(),
            token_expiration: chrono::Utc::now(),
            unix: None,
            settings: thorium::models::UserSettings::default(),
            local: true,
            verified: true,
            has_image: false,
        };
        let mut user_client = admin.clone();
        user_client.masquerade(&user);
        // queue one new entity and one association from it to an existing rule
        let mut cache = SigmaActionCache::new(OutputKey::Entity(Uuid::new_v4()));
        let groups = vec!["rules".to_owned()];
        cache.entities.push(EntityRequest::new(
            "flagged",
            EntityMetadataRequest::Other,
            groups.clone(),
        ));
        cache.associations.push((
            AssociationKind::CreatedBy,
            (
                MaybeEntity::new_existing(Uuid::new_v4(), "rule"),
                MaybeEntity::New(0),
                groups,
            ),
        ));
        // create everything the way scan_event does
        cache
            .create_all(&user_client, &admin)
            .await
            .expect("create all");
        // check who each request went out as
        let recorded = recorded.lock().expect("lock").clone();
        let auth_of = |prefix: &str| {
            recorded
                .iter()
                .filter(|(method, path, _)| method == "POST" && path.starts_with(prefix))
                .map(|(_, _, auth)| auth.clone())
                .collect::<Vec<_>>()
        };
        // the existing-entity lookup reads as the user
        assert_eq!(
            auth_of("/api/trees/"),
            vec![auth_for(USER_TOKEN)],
            "tree lookup"
        );
        // the entity and association creates go out as admin
        assert_eq!(
            auth_of("/api/entities/"),
            vec![auth_for(ADMIN_TOKEN)],
            "entity create"
        );
        assert_eq!(
            auth_of("/api/associations/"),
            vec![auth_for(ADMIN_TOKEN)],
            "association create"
        );
    }

    /// The counter map flattens [`SigmaRuleStats`] to dotted paths: the totals land
    /// under the `total.` prefix and the per entity kind breakdown under
    /// `by_entity.{kind}.`, with every kind emitted (zeros included) so the published
    /// key set stays stable per worker.
    #[test]
    fn sigma_stats_counter_map() {
        // build stats with distinct totals and one active entity kind
        let mut stats = SigmaRuleStats::default();
        stats.total = SigmaRuleSpecificStats {
            scanned: 1,
            created: 2,
            flagged: 3,
            spawned: 4,
            spawn_errors: 5,
        };
        stats.by_entity[EntityKinds::SigmaRule].scanned = 6;
        // flatten these stats into a counter map
        let map = SigmaRuleWorker::counter_map(&stats);
        // the totals must land under the total prefix
        assert_eq!(
            map.get("total.scanned"),
            Some(&1),
            "total.scanned must carry the total"
        );
        assert_eq!(
            map.get("total.created"),
            Some(&2),
            "total.created must carry the total"
        );
        assert_eq!(
            map.get("total.flagged"),
            Some(&3),
            "total.flagged must carry the total"
        );
        assert_eq!(
            map.get("total.spawned"),
            Some(&4),
            "total.spawned must carry the total"
        );
        assert_eq!(
            map.get("total.spawn_errors"),
            Some(&5),
            "total.spawn_errors must carry the total"
        );
        // an active kinds counters must land under its dotted path
        assert_eq!(
            map.get("by_entity.SigmaRule.scanned"),
            Some(&6),
            "an active kind must flatten to its dotted path"
        );
        // untouched kinds must still be emitted as zeros so the key set is stable
        assert_eq!(
            map.get("by_entity.Flag.scanned"),
            Some(&0),
            "untouched kinds must be emitted as zeros"
        );
        // the map must hold the totals plus five counters per entity kind
        assert_eq!(
            map.len(),
            5 + 5 * (&stats.by_entity).into_iter().count(),
            "the counter map must hold the totals plus five counters per entity kind"
        );
    }

    /// Build a volatility like spawn reaction action for tests
    fn volatility_spawn() -> SigmaSpawnReaction {
        // extract the pid of the matched process and pass a literal mode
        SigmaSpawnReaction::new("forensics", "volatility-dump")
            .kwarg("volatility", "--pid", vec![".pid"])
            .positional("volatility", "\"memdump\"")
            .switch("volatility", "--verbose")
    }

    /// Build a windows process like metadata json blob for tests
    fn process_json() -> serde_json::Value {
        serde_json::json!({"pid": 4321, "parent_pid": null, "name": "evil.exe"})
    }

    /// Rendering a spawn action extracts values from the matched entity
    #[test]
    fn render_spawn_args_extracts_values() {
        // render our volatility spawn action against a matched process
        let rendered = render_spawn_args(&volatility_spawn(), &process_json()).unwrap();
        // get the rendered args for the volatility image
        let args = rendered.get("volatility").expect("missing volatility args");
        // the pid should be extracted from the matched process
        assert_eq!(args.kwargs["--pid"], vec!["4321".to_owned()]);
        // the literal positional should be passed through without quotes
        assert_eq!(args.positionals, vec!["memdump".to_owned()]);
        // switches should be passed through unchanged
        assert_eq!(args.switches, vec!["--verbose".to_owned()]);
    }

    /// Rendering fails closed when a filter yields null or hits a missing field
    #[test]
    fn render_spawn_args_fails_closed() {
        // build a spawn action that extracts a null field
        let null_spawn = SigmaSpawnReaction::new("forensics", "volatility-dump").kwarg(
            "volatility",
            "--ppid",
            vec![".parent_pid"],
        );
        // rendering a null field must fail so this spawn is skipped
        assert!(render_spawn_args(&null_spawn, &process_json()).is_err());
        // build a spawn action that extracts a missing field
        let missing_spawn = SigmaSpawnReaction::new("forensics", "volatility-dump").kwarg(
            "volatility",
            "--pid",
            vec![".not_a_field"],
        );
        // rendering a missing field must also fail
        assert!(render_spawn_args(&missing_spawn, &process_json()).is_err());
    }

    /// Filters with multiple outputs render multiple values in order
    #[test]
    fn render_spawn_args_multiple_outputs() {
        // build a spawn action whose filter yields multiple outputs
        let spawn = SigmaSpawnReaction::new("forensics", "volatility-dump").kwarg(
            "volatility",
            "--info",
            vec![".pid, .name"],
        );
        // render this spawn action against a matched process
        let rendered = render_spawn_args(&spawn, &process_json()).unwrap();
        // both outputs should become values for this kwarg in order
        assert_eq!(
            rendered["volatility"].kwargs["--info"],
            vec!["4321".to_owned(), "evil.exe".to_owned()]
        );
    }

    /// Spawning is refused at or past the max trigger depth
    #[test]
    fn spawn_depth_capped_at_max_depth() {
        // below the max depth spawning is allowed
        assert!(!SigmaRuleWorker::spawn_depth_capped(4, 5));
        // at the max depth spawning is refused
        assert!(SigmaRuleWorker::spawn_depth_capped(5, 5));
        // past the max depth spawning is refused
        assert!(SigmaRuleWorker::spawn_depth_capped(6, 5));
    }

    /// Duplicate spawn requests merge their rule tags instead of duplicating
    #[test]
    fn add_reaction_merges_tags() {
        // build an action cache for a sample backed event
        let mut cache = SigmaActionCache::new(OutputKey::Sample("sha256".to_owned()));
        // build two identical requests from two different rules
        let first = ReactionRequest::new("forensics", "volatility-dump").tag("sigma-rule:ransom");
        let second = ReactionRequest::new("forensics", "volatility-dump").tag("sigma-rule:shadow");
        // the first request should be newly collected
        assert!(cache.add_reaction(42, first));
        // the second identical request should only merge its tag
        assert!(!cache.add_reaction(42, second));
        // only one reaction should be collected with both rules tags
        assert_eq!(cache.reactions.len(), 1);
        assert_eq!(
            cache.reactions[0].tags,
            vec![
                "sigma-rule:ransom".to_owned(),
                "sigma-rule:shadow".to_owned()
            ]
        );
        // a request with a different hash should be newly collected
        let other = ReactionRequest::new("forensics", "other-pipeline").tag("sigma-rule:ransom");
        assert!(cache.add_reaction(43, other));
        assert_eq!(cache.reactions.len(), 2);
    }

    /// Identical spawn requests hash identically and distinct ones do not
    #[test]
    fn hash_spawn_request_dedupes() {
        // render the same spawn action twice
        let first = render_spawn_args(&volatility_spawn(), &process_json()).unwrap();
        let second = render_spawn_args(&volatility_spawn(), &process_json()).unwrap();
        // identical requests must hash identically so they dedupe
        assert_eq!(
            hash_spawn_request("forensics", "volatility-dump", &first).unwrap(),
            hash_spawn_request("forensics", "volatility-dump", &second).unwrap(),
        );
        // a different pipeline must hash differently
        assert_ne!(
            hash_spawn_request("forensics", "volatility-dump", &first).unwrap(),
            hash_spawn_request("forensics", "other-pipeline", &second).unwrap(),
        );
    }

    /// Timed out spawn renders are tracked and refused instead of rerendered
    #[test]
    fn render_budget_blocks_timed_out_actions() {
        // build fresh budget state for this test instead of using the statics
        let timed_out = StdMutex::new(BTreeSet::new());
        let leaked = AtomicUsize::new(0);
        // an action that never timed out may render
        assert!(check_render_budget(&timed_out, &leaked, 42).is_ok());
        // record a timed out render for this action
        record_render_timeout(&timed_out, &leaked, 42);
        // the timed out action must be refused instead of leaking another thread
        assert!(check_render_budget(&timed_out, &leaked, 42).is_err());
        // other actions may still render while the leak budget remains
        assert!(check_render_budget(&timed_out, &leaked, 43).is_ok());
        // exhaust the leak budget
        leaked.store(MAX_LEAKED_JQ_THREADS, Ordering::Relaxed);
        // all renders must be refused once the leak budget is exhausted
        assert!(check_render_budget(&timed_out, &leaked, 43).is_err());
    }
}
