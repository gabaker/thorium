//! Logic for interacting with entities in the databases

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{DateTime, Datelike, Utc};
use futures::stream::{self, StreamExt, TryStreamExt};
use itertools::Itertools;
use scylla::statement::batch::{Batch, BatchType};
use std::time::{Duration, Instant};
use tracing::{Level, event, instrument};
use uuid::Uuid;

use crate::models::backends::TagSupport;
use crate::models::backends::db::ScyllaCursor;
use crate::models::schema::scylla::EntitiesTableRef;
use crate::models::{
    Entity, EntityForm, EntityGroupKindRow, EntityKinds, EntityListLine, EntityListParams,
    EntityListSupplementRow, EntityMetadata, GroupPolicy, KeySupport, TagDeleteRequest, TagRequest,
    User,
};
use crate::utils::{ApiError, Shared, helpers};
use crate::{bad, conflict, conn, deserialize, internal_err, not_found, serialize};

/// How long an instance lock is held before it expires, freeing a root whose holder died
const INSTANCE_LOCK_EXPIRE_MS: u64 = 30_000;

/// How long to wait between attempts to take an instance lock
const INSTANCE_LOCK_RETRY: Duration = Duration::from_millis(50);

/// The most bytes of rows one logged batch of a contained entity's copies carries, well under the
/// 1 MiB batch size Scylla refuses by default
const CONTAINED_BATCH_BYTES: usize = 512 * 1024;

/// The bytes a row takes beyond its entity's data, for its keys and group
const CONTAINED_ROW_OVERHEAD: usize = 512;

/// Releases an instance lock only if the caller still holds it
static UNLOCK_SCRIPT: std::sync::LazyLock<redis::Script> = std::sync::LazyLock::new(|| {
    redis::Script::new(
        r"
        if redis.call('get', KEYS[1]) == ARGV[1] then
            return redis.call('del', KEYS[1])
        end
        return 0
        ",
    )
});

/// Create a `Entity` in Scylla
///
/// # Arguments
///
/// * `user` - The user creating the entity
/// * `form` - The form to cast to an entity and write
/// * `id` - The ID to set for this entity
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::entities::create", skip(shared), err(Debug))]
pub async fn create(
    user: &User,
    mut form: EntityForm,
    id: Uuid,
    shared: &Shared,
) -> Result<Entity, ApiError> {
    // build an association req for this entity
    let assoc_req = form.build_association_req(user, id, shared).await?;
    // take our form tags since we need those to setup tags and this entity object
    // gets abandoned in this function anyways
    let tags = std::mem::take(&mut form.tags);
    // cast our form to an entity
    let entity = Box::pin(form.cast(user, id, shared)).await?;
    // write the entity's rows, census entries, and tags
    insert(user, &entity, tags, shared).await?;
    // create any associations if requested
    if let Some(assoc_req) = assoc_req {
        // create the associations for this entity
        assoc_req.apply(user, shared).await?;
    }
    // mark any event handler rule cache built from this entity kind as stale
    super::events::set_rules_stale(entity.kind, shared).await?;
    Ok(entity)
}

/// Write an already built entity's rows, census entries, and tags to every one of its groups
///
/// The entity's intrinsic tags are added to the given user tags.
///
/// # Arguments
///
/// * `user` - The user creating the entity
/// * `entity` - The entity to write
/// * `tags` - The user tags to give the entity
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::entities::insert", skip_all, fields(id = entity.id.to_string()), err(Debug))]
// tag requests hold the default hasher's maps
#[allow(clippy::implicit_hasher)]
pub async fn insert(
    user: &User,
    entity: &Entity,
    mut tags: HashMap<String, HashSet<String>>,
    shared: &Shared,
) -> Result<(), ApiError> {
    // populate our intrinsic tags
    entity.populate_intrinsic_tags(&mut tags)?;
    // the entity's rows are created when the entity was
    let now = entity.created;
    let year = now.year();
    // get the partition size for repos
    let chunk_size = shared.config.thorium.entities.partition_size;
    // get the partition to write this repo off too
    let bucket = helpers::partition(now, year, chunk_size);
    // concurrently insert the entity in each group
    stream::iter(entity.groups.iter())
        .map(Ok::<&String, ApiError>)
        .try_for_each_concurrent(100, |group| {
            // build this group's row: only `group` differs, and every other field borrows the
            // entity, with its metadata stored as JSON
            let row = EntitiesTableRef {
                kind: &entity.kind,
                group,
                year,
                bucket,
                created: now,
                id: entity.id,
                name: &entity.name,
                submitter: &entity.submitter,
                kind_data: &entity.metadata,
                description: entity.description.as_deref(),
                image: entity.image.as_deref(),
            };
            async move {
                // write this group's row
                shared
                    .scylla
                    .queries
                    .entities
                    .insert(&shared.scylla.session, &row)
                    .await?;
                Ok(())
            }
        })
        .await?;
    // make a vec to store our keys
    let mut keys = Vec::with_capacity(entity.groups.len());
    // calculate the grouping for this form
    let grouping = bucket / 10_000;
    // build the keys for this items census cache
    super::keys::entities::census_keys(&mut keys, &entity.groups, year, bucket, grouping, shared);
    // update this samples census cache info
    super::census::incr_cache(keys, shared).await?;
    // add tags if we have any
    if !tags.is_empty() {
        // build a tag request
        let mut req = TagRequest::<Entity>::default().groups(entity.groups.clone());
        // move our tags to the request
        req.tags = tags;
        // create the earliest map; in this case it's just the created timestamp for all groups
        let earliest: HashMap<String, DateTime<Utc>> = entity
            .groups
            .iter()
            .map(|group| (group.clone(), now))
            .collect();
        // save the tags in scylla
        super::tags::create_owned(
            user,
            Entity::build_key(entity.id.to_string(), &()),
            req,
            &earliest,
            shared,
        )
        .await?;
    }
    Ok(())
}

/// Take the lock held while a root entity or one of its instances is written
///
/// Returns the token that releases the lock. A lock that's held by someone else is retried
/// until `wait` has passed, after which the request is refused with a `409`.
///
/// # Arguments
///
/// * `root` - The id of the root entity being written
/// * `name` - The name of the entity being written for the error message
/// * `wait` - How long to wait for a lock someone else holds
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::entities::lock_instances", skip(shared), err(Debug))]
pub async fn lock_instances(
    root: Uuid,
    name: &str,
    wait: Duration,
    shared: &Shared,
) -> Result<String, ApiError> {
    // build the lock's key and this request's token
    let key = super::keys::entities::instance_lock(&root, shared);
    let token = Uuid::new_v4().to_string();
    // track when we started waiting
    let start = Instant::now();
    // keep trying to take the lock until we hold it or give up
    loop {
        // try to take the lock if nobody holds it
        let acquired: Option<String> = redis::cmd("set")
            .arg(&key)
            .arg(&token)
            .arg("NX")
            .arg("PX")
            .arg(INSTANCE_LOCK_EXPIRE_MS)
            .query_async(conn!(shared))
            .await?;
        // we hold the lock once it was set
        if acquired.is_some() {
            return Ok(token);
        }
        // give up once we've waited long enough
        if start.elapsed() >= wait {
            return conflict!(format!(
                "`{name}` is being changed by another request; retry"
            ));
        }
        // wait before trying again
        tokio::time::sleep(INSTANCE_LOCK_RETRY).await;
    }
}

/// Release an instance lock if it's still held with this token
///
/// A lock that expired and was taken by another request is left alone.
///
/// # Arguments
///
/// * `root` - The id of the root entity whose lock to release
/// * `token` - The token the lock was taken with
/// * `shared` - Shared Thorium objects
#[instrument(
    name = "db::entities::unlock_instances",
    skip(token, shared),
    err(Debug)
)]
pub async fn unlock_instances(root: Uuid, token: &str, shared: &Shared) -> Result<(), ApiError> {
    // build the lock's key
    let key = super::keys::entities::instance_lock(&root, shared);
    // delete the lock only if it still holds our token
    let _: i64 = UNLOCK_SCRIPT
        .key(&key)
        .arg(token)
        .invoke_async(conn!(shared))
        .await?;
    // fail here when a test asked for a release whose reply is lost
    #[cfg(feature = "test-utilities")]
    if crate::test_utilities::faults::take_unlock_failure(root) {
        return internal_err!(format!(
            "injected failure after releasing the lock of {root}"
        ));
    }
    Ok(())
}

/// Record the highest revision any instance of a toolset or workflow was created at on every
/// row of that root
///
/// The caller holds the root's lock, so the revision only grows.
///
/// # Arguments
///
/// * `root` - The toolset or workflow as stored, in every one of its groups
/// * `revision` - The revision of the instance being created
/// * `shared` - Shared Thorium objects
#[instrument(
    name = "db::entities::set_highest_revision",
    skip(root, shared),
    fields(id = root.id.to_string()),
    err(Debug)
)]
pub async fn set_highest_revision(
    mut root: Entity,
    revision: u64,
    shared: &Shared,
) -> Result<(), ApiError> {
    // set the revision on the root's metadata
    match &mut root.metadata {
        EntityMetadata::ToolSet(meta) => meta.highest_revision = revision,
        EntityMetadata::WorkflowCatalog(meta) => meta.highest_revision = revision,
        _ => {
            return internal_err!(format!("{} is not a toolset or workflow catalog", root.id));
        }
    }
    // drop the fields filled in when it's read
    root.drop_associated_data();
    // find the partition the root's rows are in
    let year = root.created.year();
    let chunk_size = shared.config.thorium.entities.partition_size;
    let bucket = helpers::partition(root.created, year, chunk_size);
    // rewrite every row of the root with its new metadata
    write_rows(&root, year, bucket, shared).await
}

/// Rewrite a tool instance with its bans, as it's stored
///
/// The instance must be read in every one of its groups, while its tool's lock is held.
///
/// # Arguments
///
/// * `instance` - The tool instance with its new bans, in every one of its groups
/// * `shared` - Shared Thorium objects
#[instrument(
    name = "db::entities::set_tool_instance_bans",
    skip_all,
    fields(id = instance.id.to_string()),
    err(Debug)
)]
pub async fn set_tool_instance_bans(mut instance: Entity, shared: &Shared) -> Result<(), ApiError> {
    // only a tool instance carries bans
    if !matches!(instance.metadata, EntityMetadata::ToolInstance(_)) {
        return internal_err!(format!("{} is not a tool instance", instance.id));
    }
    // drop the fields filled in when it's read
    instance.drop_associated_data();
    // find the partition the instance's rows are in
    let year = instance.created.year();
    let chunk_size = shared.config.thorium.entities.partition_size;
    let bucket = helpers::partition(instance.created, year, chunk_size);
    // rewrite every row of the instance with its new metadata
    write_rows(&instance, year, bucket, shared).await
}

/// Mark an agent, toolset, or workflow modified now, as it's stored
///
/// The root must be read in every one of its groups, while its lock is held.
///
/// # Arguments
///
/// * `root` - The agent, toolset, or workflow, in every one of its groups
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::entities::mark_modified", skip_all, fields(id = root.id.to_string()), err(Debug))]
pub async fn mark_modified(mut root: Entity, shared: &Shared) -> Result<(), ApiError> {
    // set the modified time on the root's metadata
    let now = Utc::now();
    match &mut root.metadata {
        EntityMetadata::Agent(meta) => meta.modified = now,
        EntityMetadata::ToolSet(meta) => meta.modified = now,
        EntityMetadata::WorkflowCatalog(meta) => meta.modified = now,
        _ => return internal_err!(format!("{} has no modified time", root.id)),
    }
    // drop the fields filled in when it's read
    root.drop_associated_data();
    // find the partition the root's rows are in
    let year = root.created.year();
    let chunk_size = shared.config.thorium.entities.partition_size;
    let bucket = helpers::partition(root.created, year, chunk_size);
    // rewrite every row of the root with its new metadata
    write_rows(&root, year, bucket, shared).await
}

/// Get an entity from Scylla
///
/// # Arguments
///
/// * `groups` - The list of groups to check for the entity
/// * `name` - The name of the entity
/// * `id` - An optional network policy ID required if one or more distinct entities
///          share the same name
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::entities::get", skip(shared), err(Debug))]
pub async fn get(groups: &[String], id: Uuid, shared: &Shared) -> Result<Entity, ApiError> {
    // read the entity's rows in these groups
    let mut entity = get_untagged(groups, id, shared).await?;
    // get the tags for this entity
    entity.get_tags(groups, shared).await?;
    Ok(entity)
}

/// Get an entity from Scylla without its tags
///
/// An entity whose stored data can't be read is logged and returns an error naming it as
/// unreadable.
///
/// # Arguments
///
/// * `groups` - The list of groups to check for the entity
/// * `id` - The id of the entity
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::entities::get_untagged", skip(shared), err(Debug))]
pub async fn get_untagged(
    groups: &[String],
    id: Uuid,
    shared: &Shared,
) -> Result<Entity, ApiError> {
    // a user with no groups cannot see any entity, so report it as missing just
    // like an entity that doesn't exist in any of their groups
    if groups.is_empty() {
        return not_found!(format!("Entity {id} not found"));
    }
    // count this read so tests can check how often a request reads an entity
    #[cfg(feature = "test-utilities")]
    crate::test_utilities::faults::count_entity_read(id);
    let mut entity: Option<Entity> = None;
    // break our groups into chunks of 100
    for groups_chunk in groups.chunks(100) {
        // count this query so tests can check how many queries a request sends
        #[cfg(feature = "test-utilities")]
        crate::test_utilities::faults::count_entity_query();
        let rows = shared
            .scylla
            .queries
            .entities_by_id
            .get_chunk(&shared.scylla.session, id, groups_chunk)
            .await?;
        // cast rows to entity rows and save the info
        for row in rows {
            // refuse an entity whose stored data can't be read, naming it in the error
            let row = match row {
                Ok(row) => row,
                Err(error) => {
                    // log this entity's unreadable rows so an admin can find and repair them
                    warn_unreadable(&[id], groups_chunk, &HashMap::new(), &[error], shared).await;
                    return internal_err!(format!(
                        "Entity {id} is unreadable: its stored data could not be parsed"
                    ));
                }
            };
            // get the entity we are building or cast this row to one
            match entity.as_mut() {
                Some(handle) => handle.groups.push(row.group),
                None => {
                    entity = Some(Entity::try_from(row)?);
                }
            }
        }
    }
    // if we didn't build an entity then return a 404
    match entity {
        Some(entity) => Ok(entity),
        None => not_found!(format!("Entity {id} not found")),
    }
}

/// Get every group an entity is in, including groups the caller can't see
///
/// Used to authorize writes to `Contained` entities, which need rights in all of their groups.
///
/// # Arguments
///
/// * `id` - The id of the entity
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::entities::all_groups", skip(shared), err(Debug))]
pub async fn all_groups(id: Uuid, shared: &Shared) -> Result<Vec<String>, ApiError> {
    // read the groups from the entity's id partition
    let rows = shared
        .scylla
        .queries
        .entities_by_id
        .all_groups(&shared.scylla.session, id)
        .await?;
    // collect each group once, as an entity may have several rows in a group
    let mut groups: Vec<String> = Vec::new();
    for row in rows {
        // get the group from this row
        let group = row?.group;
        // add this group if we haven't seen it yet
        if !groups.contains(&group) {
            groups.push(group);
        }
    }
    // an entity with no rows doesn't exist
    if groups.is_empty() {
        return not_found!(format!("Entity {id} not found"));
    }
    Ok(groups)
}

/// Get all entities of some kinds sharing an exact name from Scylla
///
/// Entity names are not unique so this reads the `entities_by_name` view and
/// dedupes the rows by entity id, merging group rows for the same entity. The
/// kind slice rides in the CQL because other kinds legally share a rule's name
/// (every yara rule hit is named after its rule), so filtering in Rust would
/// crawl one row per hit when resolving a productive rule's name. More than
/// 500 distinct entities of the requested kinds sharing one name is rejected
/// as too ambiguous, and a group chunk that fills its 501 row statement limit
/// is rejected as truncated, so a pathological shared-name partition can never
/// force an unbounded read or a silently incomplete entity. Matches come back
/// ordered oldest-created first and an
/// unknown name is just an empty list since callers distinguish the zero, one,
/// and many match cases themselves. A match whose stored data can't be read
/// is logged and returns an error naming it as unreadable.
///
/// # Arguments
///
/// * `groups` - The list of groups to check for entities
/// * `name` - The exact name of the entities to get
/// * `kinds` - The entity kinds to include
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::entities::get_by_name", skip(shared), err(Debug))]
pub async fn get_by_name(
    groups: &[String],
    name: &str,
    kinds: &[EntityKinds],
    shared: &Shared,
) -> Result<Vec<Entity>, ApiError> {
    // a caller with no visible groups can match nothing; return the documented
    // zero-match empty list instead of surfacing an internal guard as a 400
    if groups.is_empty() {
        return Ok(Vec::new());
    }
    // dedupe the requested kinds so duplicate params cannot inflate the
    // clustering-key IN product past what the chunk sizing below accounts for
    let mut seen_kinds = HashSet::with_capacity(kinds.len());
    let kinds: Vec<EntityKinds> = kinds
        .iter()
        .filter(|kind| seen_kinds.insert(**kind))
        .copied()
        .collect();
    // create a btreemap to keep the entities sorted by creation timestamp;
    // the value is a list of entity ids in case some were created at the exact
    // same time, where repeats are fine since we take from the data map below
    let mut sorted: BTreeMap<DateTime<Utc>, Vec<Uuid>> = BTreeMap::new();
    // make a map of the entity's id to its actual data
    let mut data_map: HashMap<Uuid, Entity> = HashMap::new();
    // scylla caps the clustering-key IN cartesian product (groups x kinds) at
    // 100 per query, so size the group chunks to stay at or under that cap
    let group_chunk_size = std::cmp::max(1, 100 / kinds.len().max(1));
    // break our groups into cap-sized chunks
    for groups_chunk in groups.chunks(group_chunk_size) {
        // query for this chunk's rows of the requested kinds sharing our name
        let rows = shared
            .scylla
            .queries
            .entities_by_name()?
            .get_chunk(&shared.scylla.session, name, groups_chunk, &kinds)
            .await?;
        // count this chunks rows so a truncated read can never pass silently
        let mut chunk_rows: usize = 0;
        // cast rows to entity rows and save the info
        for cast in rows {
            // refuse a match whose stored data can't be read, naming it in the error
            let row = match cast {
                Ok(row) => row,
                Err(error) => {
                    // log the unreadable match so an admin can find and repair it
                    event!(
                        Level::WARN,
                        msg = "Entity row is unreadable",
                        name = name,
                        error = error.to_string(),
                    );
                    return internal_err!(format!(
                        "An entity named '{name}' is unreadable: its stored data could not be parsed"
                    ));
                }
            };
            // count this row towards this chunks truncation guard
            chunk_rows += 1;
            // get an entry to the timestamp map
            let timestamp_list = sorted.entry(row.created).or_default();
            // add our entity's id to it
            timestamp_list.push(row.id);
            // bail out if far too many distinct entities share this name; the
            // view holds one row per group so the guard must count entities,
            // not group rows, or a single entity shared into many groups
            // would trip it
            if !data_map.contains_key(&row.id) && data_map.len() >= 500 {
                return bad!(format!(
                    "More than 500 entities share the name '{name}'; address this entity by its id"
                ));
            }
            // set this entity's details
            match data_map.entry(row.id) {
                Entry::Vacant(empty) => {
                    // make a new entity from the row
                    let entity = Entity::try_from(row)?;
                    // insert it into the entry
                    empty.insert(entity);
                }
                // insert the row's group if we already have this entity
                Entry::Occupied(occupied) => occupied.into_mut().add_row(row),
            }
        }
        // the statement caps each chunk at 501 rows; a full pull means the
        // read may have been truncated, so refuse to return a silently
        // incomplete entity rather than dropping group memberships
        if chunk_rows > 500 {
            return bad!(format!(
                "Too many rows share the name '{name}' in the requested groups; address this entity by its id"
            ));
        }
    }
    // get tags for all of the matched entities concurrently
    stream::iter(data_map.values_mut())
        .map(Ok::<_, ApiError>)
        .try_for_each_concurrent(10, |entity| async {
            entity.get_tags(groups, shared).await?;
            Ok(())
        })
        .await?;
    // compile our matches into a final list
    let mut matches = Vec::with_capacity(data_map.len());
    // keep oldest-created first order by crawling the sorted map in order,
    // where ties keep their insertion order
    for (_, ids) in sorted {
        for id in ids {
            // move this entity into our list if we haven't already
            if let Some(entity) = data_map.remove(&id) {
                matches.push(entity);
            }
        }
    }
    Ok(matches)
}

/// Get an entity's name, kind, and group rows by its id across all groups
///
/// The `entities_by_id` view partitions by id alone so this is a single
/// partition read that needs no group or time bucket knowledge. This backs the
/// deterministic create-as-merge path for yara rule hits where a creator in
/// group B must find a hit that only exists in group A, and the server side
/// hit route where a rules authoritative groups gate the whole write.
///
/// # Arguments
///
/// * `id` - The id of the entity to look for
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::entities::exists_by_id", skip(shared), err(Debug))]
pub async fn exists_by_id(
    id: Uuid,
    shared: &Shared,
) -> Result<Option<(String, EntityKinds, Vec<String>)>, ApiError> {
    // read this entitys group and kind rows across all groups by its id
    let rows = shared
        .scylla
        .queries
        .entities_by_id
        .get_groups_kinds_by_ids(&shared.scylla.session, &[id])
        .await?;
    // track the kind and groups this entity exists in
    let mut kind: Option<EntityKinds> = None;
    let mut groups: Vec<String> = Vec::default();
    // crawl this entitys rows and collect its groups
    for row in rows {
        // make sure the row is valid
        let row = row?;
        // track this entitys kind
        kind.get_or_insert(row.kind);
        // keep each group once, as an entity may have several rows in a group
        if !groups.contains(&row.group) {
            groups.push(row.group);
        }
    }
    // an entity with no rows doesn't exist
    let Some(kind) = kind else {
        return Ok(None);
    };
    // read this entitys name from the same single id partition
    let names = shared
        .scylla
        .queries
        .entities_by_id
        .get_names_kinds_by_ids(&shared.scylla.session, &[id])
        .await?;
    // take the name from this entitys first row
    let mut name: Option<String> = None;
    for row in names {
        // make sure the row is valid
        let row = row?;
        // track this entitys name
        name.get_or_insert(row.name);
    }
    // a group row without a name row should be impossible on one partition
    let Some(name) = name else {
        return Ok(None);
    };
    // return the name, kind, and groups for this entity
    Ok(Some((name, kind, groups)))
}

/// List entities in specific groups and with tags
///
/// When the list is filtered by tags, only entities of the requested kinds are kept, because tag
/// rows aren't partitioned by kind.
///
/// # Arguments
///
/// * `user` - The user that is listing entities
/// * `params` - The query params to use when listing files
/// * `dedupe` - Whether to dedupe submissions for the same sha256
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::entities::list", skip(user, shared), err(Debug))]
pub async fn list(
    user: &User,
    params: EntityListParams,
    dedupe: bool,
    shared: &Shared,
) -> Result<ScyllaCursor<EntityListLine>, ApiError> {
    // build and read this cursors first page
    let cursor = list_inner(user, params, dedupe, shared).await?;
    // save this cursor so callers can present its id again
    cursor.save(shared).await?;
    Ok(cursor)
}

/// List one page of entities without persisting a resumable cursor
///
/// Internal bounded aggregate readers (the hunt quota census and the admin
/// hunt status census) read a single page and throw the cursor id away, so
/// saving the cursor would just orphan an 8 hour TTL'd redis key per call
/// whenever the page was not exhausted (`ScyllaCursor::save` already skips
/// new-and-exhausted cursors on its own).
///
/// # Arguments
///
/// * `user` - The user that is listing entities
/// * `params` - The query params to use when listing files
/// * `dedupe` - Whether to dedupe submissions for the same sha256
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::entities::list_once", skip(user, shared), err(Debug))]
pub async fn list_once(
    user: &User,
    params: EntityListParams,
    dedupe: bool,
    shared: &Shared,
) -> Result<ScyllaCursor<EntityListLine>, ApiError> {
    // build and read this cursors first page without ever saving it
    list_inner(user, params, dedupe, shared).await
}

/// Build an entity list cursor and read its first page
///
/// # Arguments
///
/// * `user` - The user that is listing entities
/// * `params` - The query params to use when listing files
/// * `dedupe` - Whether to dedupe submissions for the same sha256
/// * `shared` - Shared Thorium objects
async fn list_inner(
    user: &User,
    params: EntityListParams,
    dedupe: bool,
    shared: &Shared,
) -> Result<ScyllaCursor<EntityListLine>, ApiError> {
    // get our cursor scoped to the creating user
    let mut cursor = ScyllaCursor::from_params(params, &user.username, dedupe, shared).await?;
    // get the next page of data for this cursor
    cursor.next(shared).await?;
    // if we are searching on tags, we need to supplement the data
    // with names+kinds because tag rows don't have them
    if cursor.retain.tags_retain.is_some() {
        supplement_tag_lines(&mut cursor.data, shared).await?;
        // tag rows aren't partitioned by kind, so keep only the requested kinds
        let kinds = &cursor.retain.extra_filter;
        cursor.data.retain(|line| kinds.contains(&line.kind));
    }
    Ok(cursor)
}

/// List entities for a grep search without saving the list cursor
///
/// Grep cursors embed the retained state of their inner list cursor, so the cursor
/// built here is handed back unsaved for its owner to persist atomically. A resumed
/// cursor lists only the saved groups the params may still list.
///
/// # Arguments
///
/// * `user` - The user that is listing entities
/// * `params` - The query params to use when listing entities
/// * `retain` - The serialized retained state of the list cursor to resume
/// * `shared` - Shared Thorium objects
#[instrument(
    name = "db::entities::list_greppable",
    skip(user, retain, shared),
    err(Debug)
)]
pub async fn list_greppable(
    user: &User,
    params: EntityListParams,
    retain: Option<String>,
    shared: &Shared,
) -> Result<ScyllaCursor<EntityListLine>, ApiError> {
    // resume our cursor from its retained state or build a new one from our params
    let mut cursor = match retain {
        // narrow the resumed cursor to the groups this page was authorized for
        Some(retain) => {
            ScyllaCursor::from_retain(deserialize!(&retain), params, &user.username, true, shared)?
        }
        None => ScyllaCursor::from_params(params, &user.username, true, shared).await?,
    };
    // get the next page of data for this cursor
    cursor.next(shared).await?;
    // if we are searching on tags, we need to supplement the data
    // with names+kinds because tag rows don't have them
    if cursor.retain.tags_retain.is_some() {
        supplement_tag_lines(&mut cursor.data, shared).await?;
    }
    Ok(cursor)
}

/// Get all details for many entities
///
/// Entities whose stored data can't be read are logged and skipped.
///
/// # Arguments
///
/// * `groups` - The groups to get from
/// * `ids` - The ids of the entities to get details on
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::entities::get_many", skip(shared), err(Debug))]
pub async fn get_many(
    groups: &[String],
    ids: &[Uuid],
    shared: &Shared,
) -> Result<Vec<Entity>, ApiError> {
    // count these reads so tests can check how often a request reads an entity
    #[cfg(feature = "test-utilities")]
    for id in ids {
        crate::test_utilities::faults::count_entity_read(*id);
    }
    // read the entities' rows
    let mut details = read_many(groups, ids, shared).await?;
    // get tags for all of the entities concurrently
    stream::iter(details.iter_mut())
        .map(Ok::<_, ApiError>)
        .try_for_each_concurrent(10, |entity| async {
            // count these tag reads so tests can check which requests read tags
            #[cfg(feature = "test-utilities")]
            crate::test_utilities::faults::count_tag_read(entity.id);
            entity.get_tags(groups, shared).await?;
            Ok(())
        })
        .await?;
    Ok(details)
}

/// Get many entities without their tags, newest first
///
/// Entities whose stored data can't be read are logged and skipped.
///
/// # Arguments
///
/// * `groups` - The groups to get from
/// * `ids` - The ids of the entities to get
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::entities::get_many_untagged", skip(shared), err(Debug))]
pub async fn get_many_untagged(
    groups: &[String],
    ids: &[Uuid],
    shared: &Shared,
) -> Result<Vec<Entity>, ApiError> {
    // count these reads so tests can check how often a request reads an entity
    #[cfg(feature = "test-utilities")]
    for id in ids {
        crate::test_utilities::faults::count_entity_read(*id);
    }
    // read the entities' rows
    read_many(groups, ids, shared).await
}

/// Get details for many entities without hydrating their tags
///
/// Bounded aggregate readers like the admin hunt census project entities into
/// tag free status lines, so loading tags for every entity across every group
/// would just multiply the census read cost for data nothing reads. Entities
/// whose stored data can't be read are logged and skipped.
///
/// # Arguments
///
/// * `groups` - The groups to get from
/// * `ids` - The ids of the entities to get details on
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::entities::get_many_bare", skip(shared), err(Debug))]
pub async fn get_many_bare(
    groups: &[String],
    ids: &[Uuid],
    shared: &Shared,
) -> Result<Vec<Entity>, ApiError> {
    // hydrate these entities without their tags
    get_many_untagged(groups, ids, shared).await
}

/// Read the rows of many entities without their tags, newest first
///
/// Rows whose stored data can't be read are logged and skipped, so one bad row can't fail a
/// whole listing.
///
/// # Arguments
///
/// * `groups` - The groups to get from
/// * `ids` - The ids of the entities to get
/// * `shared` - Shared Thorium objects
async fn read_many(
    groups: &[String],
    ids: &[Uuid],
    shared: &Shared,
) -> Result<Vec<Entity>, ApiError> {
    // Create a btreemap to keep the entities sorted by creation timestamp.
    // Make the value a list of entity ids in case they were uploaded at the exact same time.
    // We're okay to have a list with possibly repeated ids because we'll take from a the data
    // map to guarantee we don't return repeats. This also maintains insert order.
    let mut sorted: BTreeMap<DateTime<Utc>, Vec<Uuid>> = BTreeMap::new();
    // make a map of the entity's id to its actual data
    let mut data_map: HashMap<Uuid, Entity> = HashMap::new();
    for (ids_chunk, groups_chunk) in ids.chunks(50).cartesian_product(groups.chunks(50)) {
        // count this query so tests can check how many queries a request sends
        #[cfg(feature = "test-utilities")]
        crate::test_utilities::faults::count_entity_query();
        // query for this chunk combo's data
        let rows = shared
            .scylla
            .queries
            .entities_by_id
            .get_many_chunk(&shared.scylla.session, ids_chunk, groups_chunk)
            .await?;
        // track the errors of any rows in this chunk that can't be read
        let mut unreadable = Vec::new();
        // try to cast rows to entiry rows
        for cast in rows {
            // skip a row whose stored data can't be read so one bad row can't fail the listing
            let row = match cast {
                Ok(row) => row,
                Err(error) => {
                    // keep the error so this chunk's skipped rows can be logged together
                    unreadable.push(error);
                    continue;
                }
            };
            // get an entry to the timestamp map
            let timestamp_list = sorted.entry(row.created).or_default();
            // add our entity's id to it
            timestamp_list.push(row.id);
            // set this entity's details
            match data_map.entry(row.id) {
                Entry::Vacant(empty) => {
                    // make a new entity from the row
                    let entity = Entity::try_from(row)?;
                    // insert it into the entry
                    empty.insert(entity);
                }
                // insert the row's group if we already have this entity
                Entry::Occupied(occupied) => occupied.into_mut().add_row(row),
            }
        }
        // log the rows this chunk skipped so an admin can find and repair them
        if !unreadable.is_empty() {
            warn_unreadable(ids_chunk, groups_chunk, &data_map, &unreadable, shared).await;
        }
    }
    // compile our details into a final list
    let mut details = Vec::with_capacity(data_map.len());
    // keep creation order by using the sorted map
    for (_, ids) in sorted.into_iter().rev() {
        for id in ids {
            // try to get the entity's details if we haven't already
            if let Some(entity) = data_map.remove(&id) {
                details.push(entity);
            }
        }
    }
    Ok(details)
}

/// Log the entity rows that failed to deserialize, naming each by its id, kind, and group
///
/// The deserialization errors don't say which row failed, so the rows are found by reading the
/// ids, kinds, and groups of the requested entities and keeping the ones that weren't read. No
/// other row contents are logged.
///
/// # Arguments
///
/// * `ids` - The ids of the entities that were requested
/// * `groups` - The groups the entities were requested in
/// * `read` - The entities that were read, with the groups they were read in
/// * `errors` - The errors from the rows that failed to deserialize
/// * `shared` - Shared Thorium objects
async fn warn_unreadable(
    ids: &[Uuid],
    groups: &[String],
    read: &HashMap<Uuid, Entity>,
    errors: &[scylla::deserialize::DeserializationError],
    shared: &Shared,
) {
    // describe the errors, which are usually all the same
    let error = errors
        .iter()
        .map(ToString::to_string)
        .unique()
        .collect::<Vec<_>>()
        .join("; ");
    // read the ids, kinds, and groups of the requested entities' rows
    let rows = match shared
        .scylla
        .queries
        .entities_by_id
        .get_groups_kinds_by_ids(&shared.scylla.session, ids)
        .await
    {
        Ok(rows) => rows,
        Err(lookup) => {
            // log the skipped rows without naming them since they couldn't be found
            event!(
                Level::WARN,
                msg = "Skipped unreadable entity rows",
                count = errors.len(),
                error = error,
                lookup_error = lookup.to_string(),
            );
            return;
        }
    };
    // log each requested row of an entity that wasn't read in that group, once
    let mut logged = HashSet::new();
    for row in rows.into_iter().flatten() {
        // only rows in the requested groups were read
        if !groups.contains(&row.group) {
            continue;
        }
        // skip rows whose entity was read in this group
        if read
            .get(&row.id)
            .is_some_and(|entity| entity.groups.contains(&row.group))
        {
            continue;
        }
        // log this unreadable row once per entity and group
        if logged.insert((row.id, row.group.clone())) {
            event!(
                Level::WARN,
                msg = "Skipped unreadable entity row",
                id = row.id.to_string(),
                kind = row.kind.as_str(),
                group = row.group,
                error = error,
            );
        }
    }
}

/// Supplement entity list lines from tag rows with the names and kinds of
/// their entities; tag rows only have the entities' ids
///
/// # Arguments
///
/// * `lines` - The lines to supplement
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::entities::supplement_tag_lines", skip_all, err(Debug))]
async fn supplement_tag_lines(
    lines: &mut [EntityListLine],
    shared: &Shared,
) -> Result<(), ApiError> {
    // make a map of ids to their index in the mutable vec
    let line_map = lines
        .iter()
        .enumerate()
        .map(|(index, line)| (line.id, index))
        .collect::<HashMap<_, _>>();
    // collect all our ids to a list to chunk them
    let ids = lines.iter().map(|line| line.id).collect::<Vec<_>>();
    // get all of the supplement rows concurrently
    let supplement_rows = helpers::assert_send_stream(
        stream::iter(ids.chunks(100))
            .map(|ids_chunk| async move {
                // get the names/kinds from these ids
                let found = shared
                    .scylla
                    .queries
                    .entities_by_id
                    .get_names_kinds_by_ids_chunk(&shared.scylla.session, ids_chunk)
                    .await?;
                let mut rows = Vec::new();
                for row in found {
                    rows.push(row?);
                }
                Ok::<Vec<EntityListSupplementRow>, ApiError>(rows)
            })
            .buffer_unordered(10),
    )
    .collect::<Vec<Result<_, ApiError>>>()
    .await
    .into_iter()
    .collect::<Result<Vec<_>, _>>()?;
    // add the supplemental row info to our lines;
    // we'll have duplicate rows due to groups, but that's okay
    for row in supplement_rows.into_iter().flatten() {
        let index = line_map.get(&row.id).copied().unwrap_or_default();
        let line = &mut lines[index];
        line.name = row.name;
        line.kind = row.kind;
    }
    Ok(())
}

/// Get the group and kind of every row of some entities
///
/// Reads every entity's id partition in one query per 100 ids, without reading their data.
///
/// # Arguments
///
/// * `ids` - The ids of the entities to read
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::entities::groups_and_kinds", skip(shared), err(Debug))]
pub async fn groups_and_kinds(
    ids: &[Uuid],
    shared: &Shared,
) -> Result<Vec<EntityGroupKindRow>, ApiError> {
    // collect the rows of every entity
    let mut rows = Vec::new();
    // read the entities 100 at a time
    for ids_chunk in ids.chunks(100) {
        // count this query so tests can check how many queries a request sends
        #[cfg(feature = "test-utilities")]
        crate::test_utilities::faults::count_entity_query();
        // read the group and kind of each row of these entities
        let found = shared
            .scylla
            .queries
            .entities_by_id
            .get_groups_kinds_by_ids_chunk(&shared.scylla.session, ids_chunk)
            .await?;
        // keep each row
        for row in found {
            rows.push(row?);
        }
    }
    Ok(rows)
}

/// Update an entity with its new data, removing rows for removed groups
/// and old names if needed
///
/// # Arguments
///
/// * `user` - The user that is updating this entity
/// * `entity` - The entity that's being updated
/// * `add_groups` - The groups that were added to the entity
/// * `remove_groups` - The groups that were removed from the entity
/// * `shared` - Shared Thorium objects
pub async fn update(
    user: &User,
    mut entity: Entity,
    add_groups: &[String],
    remove_groups: &[String],
    shared: &Shared,
) -> Result<(), ApiError> {
    // instance a set of tags that we want to ensure exist
    let mut tags = HashMap::default();
    // populate our intrinsic tags
    entity.populate_intrinsic_tags(&mut tags)?;
    // drop any association specific data
    entity.drop_associated_data();
    // ge the year this entity was created
    let year = entity.created.year();
    // get the partition size for repos
    let chunk_size = shared.config.thorium.entities.partition_size;
    // get the partition to write this repo off too
    let bucket = helpers::partition(entity.created, year, chunk_size);
    // rewrite the entity's row in each of its groups
    write_rows(&entity, year, bucket, shared).await?;
    // break up the groups delete into chunks to avoid cartesian product errors
    let delete_groups_chunks = remove_groups.chunks(100);
    // concurrently delete the entity's old data for each group chunk
    stream::iter(delete_groups_chunks)
        .map(Ok::<_, ApiError>)
        .try_for_each_concurrent(100, |groups_chunk| async move {
            shared
                .scylla
                .queries
                .entities
                .delete_chunk(
                    &shared.scylla.session,
                    &entity.kind,
                    groups_chunk,
                    year,
                    bucket,
                    entity.created,
                    entity.id,
                )
                .await?;
            Ok(())
        })
        .await?;
    // update census info for any new groups
    if !add_groups.is_empty() {
        // increment the census where we've added groups
        let mut keys = Vec::with_capacity(entity.groups.len());
        // get the bucket grouping for this census info
        let grouping = bucket / 10_000;
        super::keys::entities::census_keys(&mut keys, add_groups, year, bucket, grouping, shared);
        super::census::incr_cache(keys, shared).await?;
    }
    // build the key for this entity
    let entity_key = Entity::build_key(entity.id.to_string(), &());
    // add tags if we have any
    if !tags.is_empty() {
        // build a tag request
        let mut req = TagRequest::<Entity>::default().groups(entity.groups.clone());
        // move our tags to the request
        req.tags = tags;
        // get the earliest time each group can see this entity
        let earliest = entity.earliest();
        // save the tags in scylla
        super::tags::create(user, entity_key.clone(), req, &earliest, shared).await?;
    }
    // delete tag rows for the deleted groups
    let mut tag_delete_req = TagDeleteRequest::<Entity>::default().groups(remove_groups);
    // delete all tags from these groups
    tag_delete_req.tags = entity
        .tags
        .iter()
        .map(|(key, values_with_groups)| {
            (key.clone(), values_with_groups.keys().cloned().collect())
        })
        .collect();
    super::tags::delete(&entity_key, &tag_delete_req, shared).await?;
    // mark any event handler rule cache built from this entity kind as stale
    super::events::set_rules_stale(entity.kind, shared).await?;
    Ok(())
}

/// Write an entity's row in each of its groups
///
/// # Arguments
///
/// * `entity` - The entity whose rows to write
/// * `year` - The year the entity was created
/// * `bucket` - The partition the entity's rows are in
/// * `shared` - Shared Thorium objects
async fn write_rows(
    entity: &Entity,
    year: i32,
    bucket: i32,
    shared: &Shared,
) -> Result<(), ApiError> {
    // rewrite every copy of a contained entity in logged batches small enough for Scylla to
    // accept; each batch is atomic, so a failure between batches leaves the copies differing
    // until the next write rewrites them all
    if entity.kind.group_policy() == GroupPolicy::Contained {
        // measure the metadata as each row stores it, as JSON
        let meta_bytes = serialize!(&entity.metadata).len();
        // get how many rows fit in one batch
        let row_bytes = meta_bytes
            + entity.name.len()
            + entity.submitter.len()
            + entity.description.as_ref().map_or(0, String::len)
            + entity.image.as_ref().map_or(0, String::len)
            + CONTAINED_ROW_OVERHEAD;
        let per_batch = (CONTAINED_BATCH_BYTES / row_bytes).max(1);
        for groups in entity.groups.chunks(per_batch) {
            // add an insert for each of these groups' rows
            let mut batch = Batch::new(BatchType::Logged);
            let mut values = Vec::with_capacity(groups.len());
            for group in groups {
                // add this group's insert and its row, which binds by column name
                batch.append_statement(shared.scylla.queries.entities.insert.clone());
                values.push(EntitiesTableRef {
                    kind: &entity.kind,
                    group,
                    year,
                    bucket,
                    created: entity.created,
                    id: entity.id,
                    name: &entity.name,
                    submitter: &entity.submitter,
                    kind_data: &entity.metadata,
                    description: entity.description.as_deref(),
                    image: entity.image.as_deref(),
                });
            }
            // write these rows at once
            shared.scylla.session.batch(&batch, values).await?;
        }
    } else {
        // concurrently update the entity in each group
        stream::iter(entity.groups.iter())
            .map(Ok::<&String, ApiError>)
            .try_for_each_concurrent(100, |group| {
                // build this group's row: only `group` differs, and every other field borrows
                // the entity, with its metadata stored as JSON
                let row = EntitiesTableRef {
                    kind: &entity.kind,
                    group,
                    year,
                    bucket,
                    created: entity.created,
                    id: entity.id,
                    name: &entity.name,
                    submitter: &entity.submitter,
                    kind_data: &entity.metadata,
                    description: entity.description.as_deref(),
                    image: entity.image.as_deref(),
                };
                async move {
                    // write this group's row
                    shared
                        .scylla
                        .queries
                        .entities
                        .insert(&shared.scylla.session, &row)
                        .await?;
                    Ok(())
                }
            })
            .await?;
    }
    Ok(())
}

/// Delete all tags from an entity
///
/// # Arguments
///
/// * `user` - The user that is pruning tags from an entity
/// * `entity` - The entity that is having its tags pruned
/// * `shared` - Shared Thorium objects
async fn prune_tags(user: &User, entity: &Entity, shared: &Shared) -> Result<(), ApiError> {
    // cast this entities tags into a tag deletion request
    let mut tag_del = TagDeleteRequest::<Entity>::default();
    // build a set of groups to delete tags from
    let mut group_set = HashSet::with_capacity(entity.groups.len());
    // iterate and all all of our entities tags
    for (key, value_map) in &entity.tags {
        // add each tag value and its grousp to our tag delete request
        for (value, groups) in value_map {
            // add this group to the set of groups to delete tags from
            group_set.extend(groups.iter().map(ToOwned::to_owned));
            // add this value to our tags to delete
            tag_del.add_ref(key, value);
        }
    }
    // set the groups to delete tags form
    tag_del.groups = group_set.into_iter().collect();
    // delete these tags if our tag delete request isn't empty
    if !tag_del.tags.is_empty() {
        // a contained entity's delete was authorized in all of its groups already
        if entity.kind.group_policy() == GroupPolicy::Contained {
            // delete its tags from every group directly
            let key = Entity::build_key(entity.id.to_string(), &());
            super::tags::delete(&key, &tag_del, shared).await?;
        } else {
            // delete the requested tags
            entity.delete_tags(user, tag_del, shared).await?;
        }
    }
    // mark any event handler rule cache built from this entity kind as stale
    super::events::set_rules_stale(entity.kind, shared).await?;
    Ok(())
}

/// Delete an entity completely from all of its groups
///
/// The entity's census entries are left alone: entity census sets mark which buckets hold data
/// rather than counting rows, so there's nothing to decrement.
///
/// # Arguments
///
/// * `user` - The user that is deleting this entity
/// * `entity` - The entity that's being updated
/// * `shared` - Shared Thorium objects
pub async fn delete(user: &User, entity: &Entity, shared: &Shared) -> Result<(), ApiError> {
    // we delete tags and associations this first so if any failures occur we don't leave
    // dangling references
    // prune this entities tags first
    prune_tags(user, entity, shared).await?;
    // get the year this entity was created
    let year = entity.created.year();
    // get the partition size for entities
    let chunk_size = shared.config.thorium.entities.partition_size;
    // get the bucket for this entity
    let bucket = helpers::partition(entity.created, year, chunk_size);
    // concurrently delete the entity's rows
    stream::iter(entity.groups.chunks(100))
        .map(Ok::<_, ApiError>)
        .try_for_each_concurrent(100, |groups_chunk| async move {
            shared
                .scylla
                .queries
                .entities
                .delete_chunk(
                    &shared.scylla.session,
                    &entity.kind,
                    groups_chunk,
                    year,
                    bucket,
                    entity.created,
                    entity.id,
                )
                .await?;
            Ok(())
        })
        .await?;
    // mark any event handler rule cache built from this entity kind as stale
    super::events::set_rules_stale(entity.kind, shared).await?;
    Ok(())
}
