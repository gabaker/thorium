//! The `pipelines-to-tool-stages` migration
//!
//! Pipelines created before pipelines named tools store only their stage names, and each stage
//! ran the image of that name. This migration gives every such pipeline a `stage_tools` map from
//! each stage to the first instance of the tool its image was converted to, so the pipeline keeps
//! running what the image ran until its owner moves the stage. The id of that instance is derived
//! from the image's group and name, and a stage only converts once that instance exists (or, in a
//! check, once its image does and the image conversion accepts it), so the migration can run
//! before or after the image conversion and a rerun finishes what an earlier run couldn't. A
//! pipeline converts as a whole or not at all: one stage that can't be resolved leaves the
//! pipeline untouched and recorded as an issue.

use async_trait::async_trait;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, LazyLock};
use thorium::client::migrations::ToolIdMap;
use thorium::models::schema::{
    AppliedReport, Batch, IssueReport, MigrateError, MigrationRunner, ObjectClass, Outcome,
    RunContext, RunMode, Slice, backend_error, is_rejection,
};
use thorium::models::upgrades::{IssueCategory, IssueReason, ReasonBuilder, Record};
use uuid::Uuid;

use super::images_to_tools::ImagesToTools;
use super::registry::{Backend, MigrationDef, Support};

/// The id of this migration
pub const ID: &str = "pipelines-to-tool-stages";

/// Writes a pipeline's `stage_tools` unless the pipeline was deleted since it was read
const WRITE_STAGE_TOOLS: &str = "if redis.call('EXISTS', KEYS[1]) == 1 then \
     redis.call('HSET', KEYS[1], 'stage_tools', ARGV[1]) return 1 end return 0";

/// Every pipeline's `group/name` in key order, as listed when a pass started, by namespace
///
/// A pass lists the pipelines once and each batch reads on from its position in the listing,
/// instead of listing everything again for every batch. The lock is held while listing, so slices
/// that start together share one read.
static LISTINGS: LazyLock<tokio::sync::Mutex<HashMap<String, Arc<Vec<String>>>>> =
    LazyLock::new(|| tokio::sync::Mutex::new(HashMap::new()));

/// Split a pipeline's stored fields into the stages that still need a tool
///
/// # Arguments
///
/// * `fields` - The fields of the pipeline's data hash
pub fn pending_stages(
    fields: &BTreeMap<String, String>,
) -> Result<(BTreeMap<String, Uuid>, Vec<String>), IssueReason> {
    // the order is required; the stage tools are absent before the layout change
    let order = fields
        .get("order")
        .ok_or_else(|| IssueReason::new(IssueCategory::MissingField, "order"))?;
    let order: Vec<Vec<String>> = ReasonBuilder::parse(order, "order")?;
    let existing: BTreeMap<String, Uuid> = match fields.get("stage_tools") {
        Some(raw) => ReasonBuilder::parse(raw, "stage_tools")?,
        None => BTreeMap::new(),
    };
    // a stage is pending until it has a tool; a stage listed twice is pending once
    let mut seen = BTreeSet::new();
    let pending = order
        .into_iter()
        .flatten()
        .filter(|stage| !existing.contains_key(stage) && seen.insert(stage.clone()))
        .collect();
    Ok((existing, pending))
}

/// Add the first instances of the resolved stages' tools to a pipeline's `stage_tools`
///
/// # Arguments
///
/// * `group` - The pipeline's group
/// * `existing` - The stages that already have a tool
/// * `pending` - The stages that need one
/// * `resolved` - The pending stages whose tool's first instance is ready
pub fn merge(
    group: &str,
    existing: &BTreeMap<String, Uuid>,
    pending: &[String],
    resolved: &BTreeSet<String>,
) -> Result<String, IssueReason> {
    // the pipeline converts whole or not at all
    if pending.iter().any(|stage| !resolved.contains(stage)) {
        return Err(IssueReason::new(
            IssueCategory::InvalidValue,
            "stages.unresolved",
        ));
    }
    // every pending stage is pinned to the first instance derived from its image
    let mut merged = existing.clone();
    for stage in pending {
        merged.insert(stage.clone(), ToolIdMap::first_instance_id(group, stage));
    }
    serde_json::to_string(&merged)
        .map_err(|_| IssueReason::new(IssueCategory::InvalidValue, "stage_tools"))
}

/// The slice a pipeline belongs to
///
/// # Arguments
///
/// * `key` - The pipeline's `group/name`
/// * `slices` - How many slices there are
#[must_use]
pub fn slice_of(key: &str, slices: u32) -> u32 {
    // FNV-1a, which never changes between builds, so a resumed run slices the same way
    let hash = key.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    });
    // the remainder is below the slice count, which is a u32
    u32::try_from(hash % u64::from(slices.max(1))).unwrap_or(0)
}

/// Split a pipeline's `group/name` key
///
/// # Arguments
///
/// * `key` - The pipeline's key
fn split_key(key: &str) -> Result<(&str, &str), MigrateError> {
    // group names never hold a slash, so the first one divides the key
    key.split_once('/')
        .ok_or_else(|| MigrateError::Systematic(format!("{key} is not a pipeline key")))
}

/// A Redis error as a migration error
///
/// # Arguments
///
/// * `err` - The error
fn redis_error(err: impl std::fmt::Display) -> MigrateError {
    // every Redis failure may pass on a retry
    MigrateError::Backend(err.to_string())
}

/// Read a pipeline's data hash as a record
///
/// # Arguments
///
/// * `ctx` - The run's context
/// * `key` - The pipeline's `group/name`
async fn read_pipeline(ctx: &RunContext, key: &str) -> Result<Option<Record>, MigrateError> {
    // an empty hash is a pipeline deleted since it was listed
    let (group, name) = split_key(key)?;
    let mut conn = ctx.backends.redis.get().await.map_err(redis_error)?;
    let fields: BTreeMap<String, String> = redis::cmd("HGETALL")
        .arg(format!(
            "{}:pipeline_data:{group}:{name}",
            ctx.backends.namespace
        ))
        .query_async(&mut *conn)
        .await
        .map_err(redis_error)?;
    Ok((!fields.is_empty()).then(|| Record::Redis {
        key: key.to_owned(),
        fields,
    }))
}

/// List every pipeline's `group/name`, sorted
///
/// # Arguments
///
/// * `ctx` - The run's context
async fn list_pipelines(ctx: &RunContext) -> Result<Vec<String>, MigrateError> {
    // find each group's set of pipelines, then read each set
    let ns = &ctx.backends.namespace;
    let prefix = format!("{ns}:pipelines:");
    let mut conn = ctx.backends.redis.get().await.map_err(redis_error)?;
    let mut sets = BTreeSet::new();
    let mut cursor = 0_u64;
    loop {
        let (next, keys): (u64, Vec<String>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg(format!("{prefix}*"))
            .arg("COUNT")
            .arg(500)
            .query_async(&mut *conn)
            .await
            .map_err(redis_error)?;
        sets.extend(keys);
        cursor = next;
        if cursor == 0 {
            break;
        }
    }
    let mut pipelines = BTreeSet::new();
    for set in sets {
        let names: Vec<String> = redis::cmd("SMEMBERS")
            .arg(&set)
            .query_async(&mut *conn)
            .await
            .map_err(redis_error)?;
        let group = set.strip_prefix(&prefix).unwrap_or(&set);
        pipelines.extend(names.into_iter().map(|name| format!("{group}/{name}")));
    }
    Ok(pipelines.into_iter().collect())
}

/// Get the pass's listing of every pipeline, reading it on first use
///
/// # Arguments
///
/// * `ctx` - The run's context
async fn listing(ctx: &RunContext) -> Result<Arc<Vec<String>>, MigrateError> {
    // reuse the listing this pass already read, holding the lock so only one slice reads it
    let ns = &ctx.backends.namespace;
    let mut listings = LISTINGS.lock().await;
    if let Some(found) = listings.get(ns) {
        return Ok(found.clone());
    }
    // list every pipeline once and keep it for the pass's other batches
    let found = Arc::new(list_pipelines(ctx).await?);
    listings.insert(ns.clone(), found.clone());
    Ok(found)
}

/// Whether a group has an image of this name
///
/// # Arguments
///
/// * `ctx` - The run's context
/// * `group` - The image's group
/// * `name` - The image's name
async fn image_exists(ctx: &RunContext, group: &str, name: &str) -> Result<bool, MigrateError> {
    // the group's set of image names
    let mut conn = ctx.backends.redis.get().await.map_err(redis_error)?;
    redis::cmd("SISMEMBER")
        .arg(format!("{}:images:{group}", ctx.backends.namespace))
        .arg(name)
        .query_async(&mut *conn)
        .await
        .map_err(redis_error)
}

/// Whether the first instance of the tool migrated from an image is stored
///
/// # Arguments
///
/// * `ctx` - The run's context
/// * `group` - The image's group
/// * `name` - The image's name
async fn instance_exists(ctx: &RunContext, group: &str, name: &str) -> Result<bool, MigrateError> {
    // read the entity at the derived id and check which tool it belongs to
    let id = ToolIdMap::first_instance_id(group, name);
    let tool = ToolIdMap::tool_id(group, name).to_string();
    let result = ctx
        .backends
        .scylla
        .query_unpaged(
            format!(
                "SELECT group, kind_data FROM {}.entities_by_id WHERE id = ?",
                ctx.backends.namespace
            ),
            (id,),
        )
        .await
        .map_err(|err| backend_error("read a tool instance", &err, is_rejection(&err)))?;
    let rows = result
        .into_rows_result()
        .map_err(|err| backend_error("read a tool instance", &err, false))?;
    let rows = rows
        .rows::<(String, String)>()
        .map_err(|err| backend_error("read a tool instance", &err, false))?;
    // the instance must be in the pipeline's group and name this tool
    for row in rows {
        let (row_group, kind_data) =
            row.map_err(|err| backend_error("read a tool instance", &err, false))?;
        let instance_tool = serde_json::from_str::<serde_json::Value>(&kind_data)
            .ok()
            .and_then(|data| data["ToolInstance"]["tool"].as_str().map(str::to_owned));
        if row_group == group && instance_tool.as_deref() == Some(tool.as_str()) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Converts pipelines' stages to tools
pub struct PipelinesToToolStages;

#[async_trait]
impl MigrationRunner for PipelinesToToolStages {
    /// Split the pipelines into slices by a hash of their key
    ///
    /// # Arguments
    ///
    /// * `n` - How many slices to make
    /// * `ctx` - The run's context
    async fn slices(&self, n: u32, ctx: &RunContext) -> Result<Vec<Slice>, MigrateError> {
        // forget the listing an earlier pass in this process read, so this pass lists afresh
        LISTINGS.lock().await.remove(&ctx.backends.namespace);
        // a slice is an index of the count; nothing is read, so any number works
        let n = n.max(1);
        Ok((0..n)
            .map(|index| Slice {
                index,
                start: serde_json::json!({"index": index, "of": n}),
                end: serde_json::json!({"index": index, "of": n}),
            })
            .collect())
    }

    /// Read the next pipelines of a slice after the last one done
    ///
    /// # Arguments
    ///
    /// * `slice` - The slice to read
    /// * `after` - The last pipeline done, or null at the start
    /// * `ctx` - The run's context
    async fn next_batch(
        &self,
        slice: &Slice,
        after: &serde_json::Value,
        ctx: &RunContext,
    ) -> Result<Batch, MigrateError> {
        // the slice's share of the pipelines past the position, in key order
        let of = slice.start["of"]
            .as_u64()
            .and_then(|of| u32::try_from(of).ok())
            .ok_or_else(|| MigrateError::Systematic("a slice without a count".to_owned()))?;
        let after = after.as_str().unwrap_or_default();
        // resume just past the last pipeline done in the pass's one listing, and take the slice's
        // next keys, one past a batch to learn whether any are left
        let all = listing(ctx).await?;
        let start = all.partition_point(|key| key.as_str() <= after);
        let size = ctx.limits.batch_rows.max(1);
        let mut remaining: Vec<&String> = all[start..]
            .iter()
            .filter(|key| slice_of(key, of) == slice.index)
            .take(size + 1)
            .collect();
        // take one batch and read its hashes, skipping the ones deleted since the listing
        let rest = remaining.split_off(remaining.len().min(size));
        let mut records = Vec::new();
        for key in &remaining {
            records.extend(read_pipeline(ctx, key).await?);
        }
        Ok(Batch {
            records,
            next: remaining
                .last()
                .map_or_else(|| serde_json::json!(after), |key| serde_json::json!(key)),
            done: rest.is_empty(),
        })
    }

    /// Decide what to do with one pipeline
    ///
    /// # Arguments
    ///
    /// * `record` - The pipeline
    /// * `mode` - `Run` or `Check`
    /// * `ctx` - The run's context
    async fn process(
        &self,
        record: &Record,
        mode: RunMode,
        ctx: &RunContext,
    ) -> Result<Outcome, MigrateError> {
        // the pipeline's stages that still need a tool
        let Record::Redis { key, fields } = record else {
            return Err(MigrateError::Systematic(
                "a pipeline record must come from Redis".to_owned(),
            ));
        };
        let issue = |reason| {
            Outcome::Issue(IssueReport {
                key: key.clone(),
                transform: ID.to_owned(),
                class: ObjectClass::Unrecognized,
                reason,
            })
        };
        let (group, _) = split_key(key)?;
        let (existing, pending) = match pending_stages(fields) {
            Ok(split) => split,
            Err(reason) => return Ok(issue(reason)),
        };
        if pending.is_empty() {
            return Ok(Outcome::Current);
        }
        // a stage is ready once its tool's first instance exists; a check also counts its image
        // unless the image conversion refuses it, since that image never gets an instance
        let mut resolved = BTreeSet::new();
        for stage in &pending {
            let ready = instance_exists(ctx, group, stage).await?
                || (mode == RunMode::Check
                    && image_exists(ctx, group, stage).await?
                    && !ImagesToTools.refuses(group, stage, ctx).await?);
            if ready {
                resolved.insert(stage.clone());
            }
        }
        // convert the pipeline only when every stage is ready
        match merge(group, &existing, &pending, &resolved) {
            Ok(stage_tools) => {
                let mut fields = fields.clone();
                fields.insert("stage_tools".to_owned(), stage_tools);
                Ok(Outcome::Write(Record::Redis {
                    key: key.clone(),
                    fields,
                }))
            }
            Err(reason) => Ok(issue(reason)),
        }
    }

    /// Write a converted pipeline's `stage_tools`
    ///
    /// # Arguments
    ///
    /// * `record` - The converted pipeline
    /// * `ctx` - The run's context
    async fn write(&self, record: &Record, ctx: &RunContext) -> Result<(), MigrateError> {
        // only the stage tools change, and only while the pipeline still exists
        let Record::Redis { key, fields } = record else {
            return Err(MigrateError::Systematic(
                "a pipeline record must come from Redis".to_owned(),
            ));
        };
        let (group, name) = split_key(key)?;
        let stage_tools = fields.get("stage_tools").ok_or_else(|| {
            MigrateError::Systematic("a converted pipeline has no stage tools".to_owned())
        })?;
        let mut conn = ctx.backends.redis.get().await.map_err(redis_error)?;
        redis::Script::new(WRITE_STAGE_TOOLS)
            .key(format!(
                "{}:pipeline_data:{group}:{name}",
                ctx.backends.namespace
            ))
            .arg(stage_tools)
            .invoke_async::<i64>(&mut *conn)
            .await
            .map(|_| ())
            .map_err(redis_error)
    }

    /// Read the named pipelines directly
    ///
    /// # Arguments
    ///
    /// * `keys` - The pipelines' `group/name` keys
    /// * `ctx` - The run's context
    async fn fetch(
        &self,
        keys: &[String],
        ctx: &RunContext,
    ) -> Result<Option<Vec<Record>>, MigrateError> {
        // one read per key; deleted pipelines are left out
        let mut found = Vec::new();
        for key in keys {
            found.extend(read_pipeline(ctx, key).await?);
        }
        Ok(Some(found))
    }

    /// Check that every stage of a pipeline has a tool
    ///
    /// # Arguments
    ///
    /// * `record` - The pipeline
    /// * `ctx` - The run's context
    async fn verify(&self, record: &Record, ctx: &RunContext) -> Result<(), IssueReason> {
        // the pipeline loads when its fields parse and no stage is left without a tool
        let _ = ctx;
        let Record::Redis { fields, .. } = record else {
            return Err(IssueReason::new(IssueCategory::BadType, ""));
        };
        let (_, pending) = pending_stages(fields)?;
        if pending.is_empty() {
            Ok(())
        } else {
            Err(IssueReason::new(
                IssueCategory::InvalidValue,
                "stages.unresolved",
            ))
        }
    }

    /// Drop the pass's listing once every slice is done
    ///
    /// # Arguments
    ///
    /// * `ctx` - The run's context
    async fn finish(&self, ctx: &RunContext) -> Result<(), MigrateError> {
        // the pipelines carry their own state; only the pass's listing is dropped
        LISTINGS.lock().await.remove(&ctx.backends.namespace);
        Ok(())
    }

    /// Count each converted pipeline as one mapping in the applied report
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
        // every changed pipeline is one mapping, of its stages to tool instances
        let _ = ctx;
        report.mappings_total += report
            .groups
            .values()
            .map(|counts| counts.changed)
            .sum::<u64>();
        Ok(())
    }
}

/// The migration's registry entry
pub const PIPELINES_TO_TOOL_STAGES: MigrationDef = MigrationDef {
    id: ID,
    backend: Backend::Redis,
    // checked only until the release's catalog step runs it; flips to `Support::Yes` with that step
    support: Support::CheckOnly,
    runner: &PipelinesToToolStages,
    pass: "pipelines",
    chain: &[ID],
    group_max: 5,
    families: &["pipelines"],
    has_views: false,
};

#[cfg(test)]
mod tests {
    use super::*;

    /// Build the stored fields of a pipeline
    ///
    /// # Arguments
    ///
    /// * `order` - The `order` field
    /// * `stage_tools` - The `stage_tools` field, when the pipeline has one
    fn fields(order: &str, stage_tools: Option<&str>) -> BTreeMap<String, String> {
        // a pipeline's hash always has its order; stage_tools only after the layout change
        let mut fields = BTreeMap::new();
        fields.insert("order".to_owned(), order.to_owned());
        if let Some(tools) = stage_tools {
            fields.insert("stage_tools".to_owned(), tools.to_owned());
        }
        fields
    }

    /// A pipeline without `stage_tools` has every stage pending, in order
    #[test]
    fn every_stage_is_pending_without_stage_tools() {
        // two parallel stages in the first position, one in the second
        let (done, pending) = pending_stages(&fields(r#"[["a","b"],["c"]]"#, None)).unwrap();
        assert!(done.is_empty());
        assert_eq!(pending, ["a", "b", "c"]);
    }

    /// Stages that already have a tool aren't pending
    #[test]
    fn converted_stages_are_not_pending() {
        // a is converted already
        let a = Uuid::new_v4();
        let json = format!(r#"{{"a":"{a}"}}"#);
        let (done, pending) = pending_stages(&fields(r#"[["a"],["b"]]"#, Some(&json))).unwrap();
        assert_eq!(done.get("a"), Some(&a));
        assert_eq!(pending, ["b"]);
    }

    /// Malformed JSON names the field it was in and no content
    #[test]
    fn malformed_fields_are_issues() {
        // a broken order, then a broken stage_tools
        let order = pending_stages(&fields("not json", None)).unwrap_err();
        assert_eq!(order.to_string(), "malformed_json:order");
        let tools = pending_stages(&fields("[[\"a\"]]", Some("{"))).unwrap_err();
        assert_eq!(tools.to_string(), "malformed_json:stage_tools");
        // a pipeline with no order field at all
        let missing = pending_stages(&BTreeMap::new()).unwrap_err();
        assert_eq!(missing.to_string(), "missing_field:order");
    }

    /// A merge adds the derived first instance ids of the resolved stages, keeping the converted
    /// ones
    #[test]
    fn merging_adds_derived_instance_ids() {
        // a was converted earlier, b and c resolve now
        let a = Uuid::new_v4();
        let existing = BTreeMap::from([("a".to_owned(), a)]);
        let pending = vec!["b".to_owned(), "c".to_owned()];
        let resolved: BTreeSet<String> = pending.iter().cloned().collect();
        let json = merge("corn", &existing, &pending, &resolved).unwrap();
        let merged: BTreeMap<String, Uuid> = serde_json::from_str(&json).unwrap();
        assert_eq!(merged.len(), 3);
        assert_eq!(merged["a"], a);
        let b = thorium::client::migrations::ToolIdMap::first_instance_id("corn", "b");
        assert_eq!(merged["b"], b);
        let c = thorium::client::migrations::ToolIdMap::first_instance_id("corn", "c");
        assert_eq!(merged["c"], c);
    }

    /// One unresolved stage keeps the whole pipeline untouched
    #[test]
    fn an_unresolved_stage_is_an_issue() {
        // c never resolves
        let pending = vec!["b".to_owned(), "c".to_owned()];
        let resolved = BTreeSet::from(["b".to_owned()]);
        let issue = merge("corn", &BTreeMap::new(), &pending, &resolved).unwrap_err();
        assert_eq!(issue.to_string(), "invalid_value:stages.unresolved");
    }

    /// Every pipeline maps to one slice below the count, the same one every time
    #[test]
    fn slices_are_stable_and_in_range() {
        // many names, three slices
        let mut seen = BTreeSet::new();
        for n in 0..200 {
            let key = format!("corn/p{n}");
            let slice = slice_of(&key, 3);
            assert!(slice < 3);
            assert_eq!(slice, slice_of(&key, 3));
            seen.insert(slice);
        }
        assert_eq!(seen.len(), 3);
        // one slice takes everything
        assert_eq!(slice_of("corn/p", 1), 0);
    }
}
