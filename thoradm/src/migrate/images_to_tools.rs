//! The `images-to-tools` migration
//!
//! Every image in Redis is converted into the Tool request it becomes, using the same pure
//! conversion in check and run, and classified: convertible, already migrated, or refused with a
//! reason code. A check writes nothing. A run creates each convertible image's tool and first
//! instance through the API's own tool creation, on a `Shared` built for an offline writer, so
//! the rows, tags, `PublishedFrom` edge, workload runtime, and `ToolUpdated` event are exactly the
//! ones the API writes. The tool's id, its first instance's id, and the time everything is
//! created at derive from the image, so a run that crashes partway and repeats an image writes
//! the same rows again. An image counts as migrated only when its tool, its first instance, and
//! the edge between them all exist. Notes and warnings the conversion produces (who publishes
//! each tool, dropped dependency names, env variable names that look like secrets, bans) are
//! collected while the pass runs and added to the applied report when it is finished. The
//! `entities` table is read once per pass for the names of its tools, so an image whose name
//! another tool holds is refused instead of colliding with it. The groups and each group's image
//! names are listed once per pass and every batch resumes from its position in those listings.

use async_trait::async_trait;
use bb8_redis::{RedisConnectionManager, bb8::Pool};
use chrono::Datelike;
use scylla::value::CqlValue;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};
use thorium::client::migrations::{
    ImageContext, MIGRATED_FROM_IMAGE_TAG, MigrationCategory, MigrationNote, MigrationReason,
    ToolIdMap, convert_image, migrated_at,
};
use thorium::models::AssociationTargetColumn;
use thorium::models::backends::entities::{PublishStamp, create_migrated_tool};
use thorium::models::schema::{
    AppliedReport, Batch, IssueReport, MAX_REPORT_NOTES, MigrateError, MigrationRunner,
    ObjectClass, Outcome, ReportNote, RunContext, RunMode, Slice, backend_error, scan_table,
};
use thorium::models::upgrades::{IssueCategory, IssueReason, Record};
use thorium::models::{Image, WorkloadSpec};
use thorium::utils::{ApiError, Shared, helpers};
use uuid::Uuid;

use super::registry::{Backend, MigrationDef, Support};

/// The step id issues of this migration are recorded under
const TRANSFORM: &str = "images-to-tools";

/// The roles whose members can publish a tool in a group
const PUBLISHER_ROLES: [&str; 3] = ["owners", "managers", "users"];

/// The images-to-tools migration
pub const IMAGES_TO_TOOLS: MigrationDef = MigrationDef {
    id: "images-to-tools",
    backend: Backend::Scylla,
    support: Support::Yes,
    runner: &ImagesToTools,
    pass: "images-to-tools",
    chain: &["images-to-tools"],
    group_max: 5,
    families: &["images", "entities"],
    has_views: true,
};

/// The runner that reads images from Redis and classifies what converting them would do
pub struct ImagesToTools;

/// What converting one group's images needs, built once per group and shared by its images
struct GroupPlan {
    /// The ids of the tools every convertible image of the group migrates to
    map: ToolIdMap,
    /// The images that can't convert because of their own fields or dependency lists
    skipped: BTreeMap<String, MigrationReason>,
    /// The images of the group by name
    images: HashMap<String, Image>,
    /// The creators that still exist and can publish in the group
    publishers: HashSet<String>,
}

/// The group and name of an item a note is about
type ItemKey = (String, String);

/// What a check collected for the report, by namespace
#[derive(Default)]
struct Collected {
    /// The notes of each item, up to the report's bound of items; classifying an item again
    /// replaces its notes
    notes: BTreeMap<ItemKey, Vec<ReportNote>>,
    /// How many warnings each item raised, including the items whose notes were dropped
    warnings: BTreeMap<ItemKey, u64>,
}

/// The tools of each group by name, as the `entities` table holds them
type ToolNames = BTreeMap<ItemKey, Vec<Uuid>>;

/// The plans built so far, by namespace and group
type Plans = HashMap<(String, String), Arc<GroupPlan>>;

/// The plans built so far
static PLANS: LazyLock<Mutex<Plans>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// The notes and warnings collected so far, by namespace
static COLLECTED: LazyLock<Mutex<HashMap<String, Collected>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The tool names read at the start of a pass, by namespace
static TOOL_NAMES: LazyLock<Mutex<HashMap<String, Arc<ToolNames>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The sorted members of the Redis sets a pass walks, read once per pass, by namespace and set key
///
/// The sorted groups and each group's sorted image names are listed the first time a batch needs
/// them and every later batch resumes from its position in the kept listing.
type Listings = HashMap<(String, String), Arc<Vec<String>>>;

/// The listings read so far in each namespace's pass; the lock is held while listing, so slices
/// that start together share one read
static LISTINGS: LazyLock<tokio::sync::Mutex<Listings>> =
    LazyLock::new(|| tokio::sync::Mutex::new(HashMap::new()));

/// The offline `Shared` each namespace's writes go through, built when the first write needs it
static WRITERS: LazyLock<tokio::sync::Mutex<HashMap<String, Arc<Shared>>>> =
    LazyLock::new(|| tokio::sync::Mutex::new(HashMap::new()));

/// Map a conversion reason to the issue reason the runner records
///
/// # Arguments
///
/// * `reason` - The conversion's reason
fn issue_reason(reason: &MigrationReason) -> IssueReason {
    // each conversion category has one runner category
    let category = match reason.category {
        MigrationCategory::MissingField => IssueCategory::MissingField,
        MigrationCategory::InvalidValue => IssueCategory::InvalidValue,
        MigrationCategory::MalformedJson => IssueCategory::MalformedJson,
        MigrationCategory::WriteFailed => IssueCategory::WriteFailed,
    };
    IssueReason::new(category, reason.field.clone())
}

/// Build the issue of an image that isn't migrated
///
/// # Arguments
///
/// * `group` - The image's group
/// * `name` - The image's name
/// * `reason` - Why the image isn't migrated
fn issue(group: &str, name: &str, reason: &IssueReason) -> Outcome {
    // an image that can't convert is one the runner doesn't recognize
    Outcome::Issue(IssueReport {
        key: format!("{group}/{name}"),
        transform: TRANSFORM.to_owned(),
        class: ObjectClass::Unrecognized,
        reason: reason.clone(),
    })
}

/// Run a read against Redis, naming what it was for when it fails
///
/// # Arguments
///
/// * `pool` - The Redis pool
/// * `context` - What the read is for
/// * `cmd` - The command to run
async fn query<T: redis::FromRedisValue>(
    pool: &Pool<RedisConnectionManager>,
    context: &str,
    cmd: &mut redis::Cmd,
) -> Result<T, MigrateError> {
    // take a connection and run the command on it
    let mut conn = pool
        .get()
        .await
        .map_err(|err| backend_error(context, &err, false))?;
    cmd.query_async(&mut *conn)
        .await
        .map_err(|err| backend_error(context, &err, false))
}

/// The members of a Redis set, sorted
///
/// # Arguments
///
/// * `pool` - The Redis pool
/// * `key` - The set's key
async fn members(
    pool: &Pool<RedisConnectionManager>,
    key: &str,
) -> Result<Vec<String>, MigrateError> {
    // read the set and order it so positions are stable
    let mut found: Vec<String> = query(pool, "read a set", redis::cmd("SMEMBERS").arg(key)).await?;
    found.sort_unstable();
    Ok(found)
}

/// The sorted members of a Redis set, read once per pass and kept for its other batches
///
/// # Arguments
///
/// * `ctx` - The run's context
/// * `key` - The set's key
async fn listing(ctx: &RunContext, key: &str) -> Result<Arc<Vec<String>>, MigrateError> {
    // reuse the listing this pass already read, holding the lock so only one slice reads it
    let id = (ctx.backends.namespace.clone(), key.to_owned());
    let mut listings = LISTINGS.lock().await;
    if let Some(found) = listings.get(&id) {
        return Ok(found.clone());
    }
    // read the set once and keep it for the pass
    let found = Arc::new(members(&ctx.backends.redis, key).await?);
    listings.insert(id, found.clone());
    Ok(found)
}

/// Parse an image from its Redis hash with the API's own deserialization
///
/// # Arguments
///
/// * `fields` - The hash's fields
fn parse_image(fields: &BTreeMap<String, String>) -> Option<Image> {
    // the image's used-by list isn't needed to convert it
    let fields = fields
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<HashMap<String, String>>();
    Image::try_from((fields, Vec::new())).ok()
}

/// Split an image data key into its group and name
///
/// # Arguments
///
/// * `ns` - The namespace
/// * `key` - The image data key
fn split_key<'a>(ns: &str, key: &'a str) -> (&'a str, &'a str) {
    // the key is `<ns>:image_data:<group>:<name>`
    let rest = key
        .strip_prefix(ns)
        .and_then(|rest| rest.strip_prefix(":image_data:"))
        .unwrap_or(key);
    rest.split_once(':').unwrap_or((rest, ""))
}

/// Sort a failed tool creation into what the runner does with it
///
/// A refusal of the tool itself (a spec the publish checks reject, a name or version conflict) is
/// recorded as a `write_failed` issue for the image. Anything else is the backend failing, which
/// the runner retries and then resumes from its checkpoint.
///
/// # Arguments
///
/// * `err` - The failed creation's error
fn write_error(err: &ApiError) -> MigrateError {
    // a 404 means something the publish needs, such as the system settings, is missing from the
    // cluster, which no image's record can fix, so it stops the pass
    if err.status().as_u16() == 404 {
        MigrateError::Systematic(format!(
            "the tool writer's environment is incomplete: {err}"
        ))
    } else if err.status().is_client_error() {
        // any other 4xx is the API refusing this tool, and retrying can't change that
        MigrateError::Rejected(format!("the tool was refused: {err}"))
    } else {
        MigrateError::Backend(format!("the tool didn't write: {err}"))
    }
}

/// Get the offline `Shared` a namespace's writes go through, building it on first use
///
/// # Arguments
///
/// * `ctx` - The run's context
async fn writer(ctx: &RunContext) -> Result<Arc<Shared>, MigrateError> {
    // reuse the one built for this namespace, holding the lock so only one is ever built
    let mut writers = WRITERS.lock().await;
    let ns = &ctx.backends.namespace;
    if let Some(shared) = writers.get(ns) {
        return Ok(shared.clone());
    }
    // build it from the run's config and Redis pool; an unreachable Scylla is worth retrying
    let shared = Box::pin(Shared::for_offline_writer(
        (*ctx.backends.conf).clone(),
        ctx.backends.redis.clone(),
    ))
    .await
    .map_err(|err| MigrateError::Backend(format!("the tool writer didn't start: {err}")))?;
    let shared = Arc::new(shared);
    writers.insert(ns.clone(), shared.clone());
    Ok(shared)
}

impl ImagesToTools {
    /// Whether the image conversion refuses an image for its own fields or dependency lists
    ///
    /// A stage that runs a refused image can't resolve to a tool, so a pipeline check doesn't
    /// plan it as convertible.
    ///
    /// # Arguments
    ///
    /// * `group` - The image's group
    /// * `name` - The image's name
    /// * `ctx` - The run's context
    pub(super) async fn refuses(
        &self,
        group: &str,
        name: &str,
        ctx: &RunContext,
    ) -> Result<bool, MigrateError> {
        // the group's plan lists every image its conversion skips
        let plan = self.plan(group, ctx).await?;
        Ok(plan.skipped.contains_key(name))
    }

    /// Get the plan for converting a group's images, building it on first use
    ///
    /// # Arguments
    ///
    /// * `group` - The group
    /// * `ctx` - The run's context
    async fn plan(&self, group: &str, ctx: &RunContext) -> Result<Arc<GroupPlan>, MigrateError> {
        // reuse a plan already built for this group
        let ns = &ctx.backends.namespace;
        let id = (ns.clone(), group.to_owned());
        if let Some(plan) = PLANS.lock().expect("plans lock").get(&id) {
            return Ok(plan.clone());
        }
        // read and parse every image of the group; the ones that don't parse can't be named by
        // another image's dependencies
        let pool = &ctx.backends.redis;
        let mut images = HashMap::new();
        for name in members(pool, &format!("{ns}:images:{group}")).await? {
            let fields: HashMap<String, String> = query(
                pool,
                "read an image",
                redis::cmd("HGETALL").arg(format!("{ns}:image_data:{group}:{name}")),
            )
            .await?;
            if let Some(image) = parse_image(&fields.into_iter().collect()) {
                images.insert(name, image);
            }
        }
        // skip the images the conversion refuses, then those whose dependency lists would empty
        let mut map = ToolIdMap::from_images(images.values());
        let mut skipped = BTreeMap::new();
        for image in images.values() {
            if WorkloadSpec::from_image(image).is_err() {
                skipped.insert(
                    (image.group.clone(), image.name.clone()),
                    MigrationReason::missing_field("image"),
                );
            }
        }
        map.settle(images.values(), &mut skipped);
        let skipped = skipped
            .into_iter()
            .map(|((_, name), reason)| (name, reason))
            .collect();
        // find the creators that still exist and hold a role that can publish in the group
        let mut allowed = HashSet::new();
        for role in PUBLISHER_ROLES {
            allowed.extend(members(pool, &format!("{ns}:groups_combined:{group}:{role}")).await?);
        }
        let mut publishers = HashSet::new();
        for creator in images.values().map(|image| image.creator.clone()) {
            let exists: bool = query(
                pool,
                "check a user",
                redis::cmd("EXISTS").arg(format!("{ns}:user_data:{creator}")),
            )
            .await?;
            if exists && allowed.contains(&creator) {
                publishers.insert(creator);
            }
        }
        // share the plan with the group's other images
        let plan = Arc::new(GroupPlan {
            map,
            skipped,
            images,
            publishers,
        });
        PLANS.lock().expect("plans lock").insert(id, plan.clone());
        Ok(plan)
    }

    /// Keep the notes and warnings of one converted image for the report
    ///
    /// # Arguments
    ///
    /// * `ns` - The namespace
    /// * `item` - The image's group and name
    /// * `notes` - The image's notes
    /// * `warnings` - The image's warnings
    fn collect(ns: &str, item: ItemKey, notes: &[MigrationNote], warnings: &[MigrationNote]) {
        // replace what an earlier classification of this image collected
        let mut all = COLLECTED.lock().expect("collected lock");
        let collected = all.entry(ns.to_owned()).or_default();
        if warnings.is_empty() {
            collected.warnings.remove(&item);
        } else {
            collected
                .warnings
                .insert(item.clone(), warnings.len() as u64);
        }
        collected.notes.insert(
            item,
            warnings
                .iter()
                .map(|note| note.report_note(true))
                .chain(notes.iter().map(|note| note.report_note(false)))
                .collect(),
        );
        // keep no more items than the report can hold, dropping the last without warnings first
        while collected.notes.len() > MAX_REPORT_NOTES {
            let drop = collected
                .notes
                .keys()
                .rev()
                .find(|key| !collected.warnings.contains_key(*key))
                .or_else(|| collected.notes.keys().next_back())
                .cloned();
            match drop {
                Some(key) => collected.notes.remove(&key),
                None => break,
            };
        }
    }

    /// Read the names of every tool in the namespace once, for the pass
    ///
    /// # Arguments
    ///
    /// * `ctx` - The run's context
    async fn read_tool_names(ctx: &RunContext) -> Result<ToolNames, MigrateError> {
        // scan the entities table reading only its key columns and name, so no entity's data is
        // read
        let mut names = ToolNames::new();
        scan_table(
            ctx,
            "entities",
            &["kind", "group", "year", "bucket", "created", "id", "name"],
            |records| {
                for record in records {
                    // only tools hold names an image's tool can collide with
                    let Record::Scylla { columns, .. } = record else {
                        continue;
                    };
                    let kind = record.text("kind").unwrap_or_default();
                    if kind.trim_matches('"') != "Tool" {
                        continue;
                    }
                    if let (Some(group), Some(name), Some(Some(CqlValue::Uuid(id)))) =
                        (record.text("group"), record.text("name"), columns.get("id"))
                    {
                        names.entry((group, name)).or_default().push(*id);
                    }
                }
                Ok(())
            },
        )
        .await?;
        Ok(names)
    }

    /// Whether an entity exists in a group
    ///
    /// # Arguments
    ///
    /// * `group` - The entity's group
    /// * `id` - The entity's id
    /// * `ctx` - The run's context
    async fn entity_exists(group: &str, id: Uuid, ctx: &RunContext) -> Result<bool, MigrateError> {
        // look the entity up by id in the group through the entities view
        let ns = &ctx.backends.namespace;
        let rows = ctx
            .backends
            .scylla
            .query_unpaged(
                format!("SELECT id FROM {ns}.entities_by_id WHERE id = {id} AND group = ?"),
                (group,),
            )
            .await
            .map_err(|err| backend_error("read an entity", &err, false))?
            .into_rows_result()
            .map_err(|err| backend_error("read an entity", &err, false))?;
        Ok(rows.rows_num() > 0)
    }

    /// Whether the `PublishedFrom` edge from a migrated instance to its tool exists
    ///
    /// The edge is what lists an instance under its tool, and it is the last row a tool's
    /// creation writes before its event, so a tool whose edge is missing was cut short.
    ///
    /// # Arguments
    ///
    /// * `group` - The tool's group
    /// * `name` - The image's name
    /// * `ctx` - The run's context
    async fn edge_exists(group: &str, name: &str, ctx: &RunContext) -> Result<bool, MigrateError> {
        // the edge's rows are keyed by the migration's time and the instance, serialized as the
        // API does
        let at = migrated_at();
        let year = at.year();
        let size = ctx.backends.conf.thorium.associations.partition_size;
        let bucket = helpers::partition(at, year, size);
        let column = |id| serde_json::to_string(&AssociationTargetColumn::Entity(id));
        let (Ok(source), Ok(target)) = (
            column(ToolIdMap::first_instance_id(group, name)),
            column(ToolIdMap::tool_id(group, name)),
        ) else {
            return Err(MigrateError::Systematic(
                "an association column didn't serialize".to_owned(),
            ));
        };
        // read the targets listed under the instance in the migration's partition
        let ns = &ctx.backends.namespace;
        let rows = ctx
            .backends
            .scylla
            .query_unpaged(
                format!(
                    "SELECT target FROM {ns}.associations WHERE group = ? AND year = ? \
                     AND bucket = ? AND source = ?"
                ),
                (group, year, bucket, source),
            )
            .await
            .map_err(|err| backend_error("read an association", &err, false))?
            .into_rows_result()
            .map_err(|err| backend_error("read an association", &err, false))?;
        // the edge exists when one of the instance's rows lists the tool
        let found = rows
            .rows::<(String,)>()
            .map_err(|err| backend_error("read an association", &err, false))?
            .filter_map(Result::ok)
            .any(|(listed,)| listed == target);
        Ok(found)
    }

    /// Whether a tool carries the tag a migration puts on the tools it makes
    ///
    /// # Arguments
    ///
    /// * `group` - The tool's group
    /// * `id` - The tool's id
    /// * `ctx` - The run's context
    async fn is_migrated_tool(
        group: &str,
        id: Uuid,
        ctx: &RunContext,
    ) -> Result<bool, MigrateError> {
        // look the tool's tags up by item through the tags view
        let ns = &ctx.backends.namespace;
        let rows = ctx
            .backends
            .scylla
            .query_unpaged(
                format!(
                    "SELECT key FROM {ns}.tags_by_item WHERE type = 'Entities' AND item = '{id}' \
                     AND group = ?"
                ),
                (group,),
            )
            .await
            .map_err(|err| backend_error("read a tool's tags", &err, false))?
            .into_rows_result()
            .map_err(|err| backend_error("read a tool's tags", &err, false))?;
        let found = rows
            .rows::<(String,)>()
            .map_err(|err| backend_error("read a tool's tags", &err, false))?
            .filter_map(Result::ok)
            .any(|(key,)| key == MIGRATED_FROM_IMAGE_TAG);
        Ok(found)
    }
}

#[async_trait]
impl MigrationRunner for ImagesToTools {
    /// Split the groups into contiguous ranges of names
    ///
    /// # Arguments
    ///
    /// * `n` - How many slices to make
    /// * `ctx` - The run's context
    async fn slices(&self, n: u32, ctx: &RunContext) -> Result<Vec<Slice>, MigrateError> {
        // forget what an earlier pass in this process collected
        let ns = &ctx.backends.namespace;
        PLANS
            .lock()
            .expect("plans lock")
            .retain(|id, _| &id.0 != ns);
        COLLECTED.lock().expect("collected lock").remove(ns);
        LISTINGS.lock().await.retain(|id, _| &id.0 != ns);
        // read the names of the namespace's tools once for the whole pass
        let names = Self::read_tool_names(ctx).await?;
        TOOL_NAMES
            .lock()
            .expect("tool names lock")
            .insert(ns.clone(), Arc::new(names));
        // split the sorted groups evenly, giving empty slices a null range
        let groups = members(&ctx.backends.redis, &format!("{ns}:groups")).await?;
        let size = groups.len().div_ceil(n.max(1) as usize).max(1);
        let chunks = groups.chunks(size).collect::<Vec<_>>();
        Ok((0..n.max(1))
            .map(|index| {
                let chunk = chunks.get(index as usize);
                Slice {
                    index,
                    start: chunk.map_or(serde_json::Value::Null, |c| serde_json::json!(c[0])),
                    end: chunk.map_or(serde_json::Value::Null, |c| {
                        serde_json::json!(c[c.len() - 1])
                    }),
                }
            })
            .collect())
    }

    /// Read the next images of a slice in group and name order
    ///
    /// # Arguments
    ///
    /// * `slice` - The slice to read
    /// * `after` - The last image done, as `{"group", "name"}`, or null at the start
    /// * `ctx` - The run's context
    async fn next_batch(
        &self,
        slice: &Slice,
        after: &serde_json::Value,
        ctx: &RunContext,
    ) -> Result<Batch, MigrateError> {
        // an empty slice has nothing to read
        let (Some(start), Some(end)) = (slice.start.as_str(), slice.end.as_str()) else {
            return Ok(Batch {
                records: Vec::new(),
                next: after.clone(),
                done: true,
            });
        };
        // list the slice's groups after the last one done, from the pass's one listing
        let ns = &ctx.backends.namespace;
        let pool = &ctx.backends.redis;
        let last_group = after["group"].as_str().unwrap_or("");
        let last_name = after["name"].as_str().unwrap_or("");
        let groups = listing(ctx, &format!("{ns}:groups")).await?;
        let mut records = Vec::new();
        let mut next = after.clone();
        let mut full = false;
        'groups: for group in groups
            .iter()
            .filter(|group| group.as_str() >= start && group.as_str() <= end)
            .filter(|group| group.as_str() >= last_group)
        {
            // read the group's images after the last one done, resuming past it in the group's
            // one listing
            let names = listing(ctx, &format!("{ns}:images:{group}")).await?;
            let first = if group.as_str() == last_group {
                names.partition_point(|name| name.as_str() <= last_name)
            } else {
                0
            };
            for name in &names[first..] {
                // stop once the batch is full
                if records.len() >= ctx.limits.batch_rows {
                    full = true;
                    break 'groups;
                }
                let key = format!("{ns}:image_data:{group}:{name}");
                let fields: BTreeMap<String, String> =
                    query(pool, "read an image", redis::cmd("HGETALL").arg(&key)).await?;
                // an image deleted since it was listed has no hash
                if !fields.is_empty() {
                    records.push(Record::Redis { key, fields });
                }
                next = serde_json::json!({"group": group, "name": name});
            }
        }
        Ok(Batch {
            records,
            next,
            done: !full,
        })
    }

    /// Classify one image: convertible, already migrated, or refused with its reason
    ///
    /// # Arguments
    ///
    /// * `record` - The image's Redis hash
    /// * `mode` - `Run` or `Check`
    /// * `ctx` - The run's context
    async fn process(
        &self,
        record: &Record,
        _mode: RunMode,
        ctx: &RunContext,
    ) -> Result<Outcome, MigrateError> {
        // a check and a run classify alike; only a run writes what converts
        let Record::Redis { key, fields } = record else {
            return Err(MigrateError::Systematic(
                "images-to-tools reads Redis hashes".to_owned(),
            ));
        };
        // an image that doesn't parse is malformed
        let ns = &ctx.backends.namespace;
        let (group, name) = split_key(ns, key);
        if parse_image(fields).is_none() {
            return Ok(issue(
                group,
                name,
                &IssueReason::new(IssueCategory::MalformedJson, ""),
            ));
        }
        // an image skipped by its fields or dependency lists is refused with that reason
        let plan = self.plan(group, ctx).await?;
        if let Some(reason) = plan.skipped.get(name) {
            return Ok(issue(group, name, &issue_reason(reason)));
        }
        // an image with no plan was deleted since it was listed
        let Some(image) = plan.images.get(name) else {
            return Ok(Outcome::Current);
        };
        // find the tools of the image's name that the pass read
        let names = TOOL_NAMES
            .lock()
            .expect("tool names lock")
            .get(ns)
            .cloned()
            .unwrap_or_default();
        let held = names
            .get(&(group.to_owned(), name.to_owned()))
            .map_or(&[][..], Vec::as_slice);
        let tool = ToolIdMap::tool_id(group, name);
        // a tool made by an earlier run is migrated once its first instance and the edge listing
        // the instance under it exist; a tool without its instance was left half made, and one
        // without the edge is written again, which repeats the rows it already has
        let finished = if held.contains(&tool) {
            let instance = ToolIdMap::first_instance_id(group, name);
            if !Self::entity_exists(group, instance, ctx).await? {
                return Ok(issue(
                    group,
                    name,
                    &IssueReason::new(IssueCategory::MissingField, "tool_instance"),
                ));
            }
            Self::edge_exists(group, name, ctx).await?
        } else {
            false
        };
        if finished {
            return Ok(Outcome::Current);
        }
        // a tool of the name that no migration made would collide with the image's tool
        for id in held.iter().filter(|id| **id != tool) {
            if !Self::is_migrated_tool(group, *id, ctx).await? {
                return Ok(issue(
                    group,
                    name,
                    &IssueReason::new(IssueCategory::InvalidValue, "name"),
                ));
            }
        }
        // convert the image, keeping its notes and warnings for the report
        let image_ctx = ImageContext {
            creator_in_group: plan.publishers.contains(&image.creator),
            last_modified: None,
            map: &plan.map,
        };
        match convert_image(image, &image_ctx) {
            Ok(migration) => {
                Self::collect(
                    ns,
                    (group.to_owned(), name.to_owned()),
                    &migration.notes,
                    &migration.warnings,
                );
                Ok(Outcome::Write(record.clone()))
            }
            Err(skipped) => Ok(issue(group, name, &issue_reason(&skipped.reason))),
        }
    }

    /// Create the tool an image converts into, with its first instance
    ///
    /// # Arguments
    ///
    /// * `record` - The image's Redis hash that classified as convertible
    /// * `ctx` - The run's context
    async fn write(&self, record: &Record, ctx: &RunContext) -> Result<(), MigrateError> {
        // the batches read images as Redis hashes
        let Record::Redis { key, .. } = record else {
            return Err(MigrateError::Systematic(
                "images-to-tools reads Redis hashes".to_owned(),
            ));
        };
        // find the image in its group's plan, which classifying it built
        let ns = &ctx.backends.namespace;
        let (group, name) = split_key(ns, key);
        let plan = self.plan(group, ctx).await?;
        // an image deleted since it was classified has nothing left to convert
        let Some(image) = plan.images.get(name) else {
            return Ok(());
        };
        // convert it again, which gives the tool and who publishes it
        let image_ctx = ImageContext {
            creator_in_group: plan.publishers.contains(&image.creator),
            last_modified: None,
            map: &plan.map,
        };
        let migration = convert_image(image, &image_ctx).map_err(|skipped| {
            MigrateError::Rejected(format!("the image no longer converts: {}", skipped.reason))
        })?;
        // create the tool with ids and a time that derive from the image, so repeating it is safe
        let stamp = PublishStamp {
            instance_id: ToolIdMap::first_instance_id(group, name),
            at: migrated_at(),
        };
        let shared = Box::pin(writer(ctx)).await?;
        // create the tool as the user the conversion chose
        let tool = &migration.tool;
        create_migrated_tool(tool, tool.submitter.username(), stamp, &shared)
            .await
            .map(|_| ())
            .map_err(|err| write_error(&err))
    }

    /// Release what the pass held
    ///
    /// # Arguments
    ///
    /// * `ctx` - The run's context
    async fn finish(&self, ctx: &RunContext) -> Result<(), MigrateError> {
        // the writer's connections, the plans, and the listings are only valid for this pass
        let ns = &ctx.backends.namespace;
        WRITERS.lock().await.remove(ns);
        PLANS
            .lock()
            .expect("plans lock")
            .retain(|id, _| &id.0 != ns);
        TOOL_NAMES.lock().expect("tool names lock").remove(ns);
        LISTINGS.lock().await.retain(|id, _| &id.0 != ns);
        Ok(())
    }

    /// Add the notes and warnings the pass collected, and how many images it mapped to tools, to the
    /// applied report
    ///
    /// # Arguments
    ///
    /// * `report` - The report to add to
    /// * `ctx` - The run's context
    async fn finish_report(
        &self,
        report: &mut AppliedReport,
        ctx: &RunContext,
    ) -> Result<(), MigrateError> {
        // every changed image is one mapping, to its tool and that tool's first instance
        report.mappings_total += report
            .groups
            .values()
            .map(|counts| counts.changed)
            .sum::<u64>();
        // take what this namespace's pass collected
        let collected = COLLECTED
            .lock()
            .expect("collected lock")
            .remove(&ctx.backends.namespace)
            .unwrap_or_default();
        // add it to the report, warnings first, within the report's bound
        report.warnings_total += collected.warnings.values().sum::<u64>();
        let (warned, plain): (Vec<_>, Vec<_>) = collected
            .notes
            .into_values()
            .flatten()
            .partition(|note| note.warning);
        report.notes.extend(warned.into_iter().chain(plain));
        report.notes.truncate(MAX_REPORT_NOTES);
        Ok(())
    }
}
