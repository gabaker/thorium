//! Backend logic for hunt entities
//!
//! Hunts are machine managed: creation resolves and freezes a rule snapshot,
//! splits the corpus into lanes, seeds the per lane checkpoint sequences, and
//! emits the first chunk events; lifecycle changes go through the dedicated
//! cancel route. The generic entity update path never touches hunt metadata.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use chrono::{DateTime, Duration, Utc};
use std::collections::{HashMap, HashSet};
use tracing::instrument;
use uuid::Uuid;

use crate::models::backends::db;
use crate::models::cursors::ApiCursor;
use crate::models::{
    Entity, EntityForm, EntityKinds, EntityListParams, EntityMetadata, EntityMetadataForm, Event,
    EventType, Group, GroupAllowAction, Hunt, HuntCheckpoint, HuntCheckpointResponse, HuntCorpus,
    HuntCorpusBounds, HuntCorpusBoundsRequest, HuntCorpusRequest, HuntCounters, HuntCreateRequest,
    HuntEngine, HuntLane, HuntQuotaView, HuntRuleLine, HuntRuleListParams, HuntSelector,
    HuntStatus, HuntStatusLine, HuntStatusParams, HuntSystemStatus, KeySupport, KindDepth,
    TagDeleteRequest, User, UserHuntCensus, YaraRule,
};
use crate::utils::{ApiError, Shared};
use crate::{bad, can_modify, conflict, for_groups, internal_err, is_admin, not_found};

/// A frozen rule snapshot resolved from a hunt's selector
#[derive(Debug, Default)]
struct FrozenSnapshot {
    /// The rule entity ids this hunt will scan with
    rules: Vec<Uuid>,
    /// How many rules the engine's snapshot filter excluded
    excluded_quarantined: u64,
    /// How many rule set members of another engine's kind were excluded
    excluded_foreign: u64,
}

/// What a cancel request should do given a hunt's current status
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CancelAction {
    /// The hunt is active so flip it to cancelled
    Proceed,
    /// The hunt is already cancelled so just re-run the stale tag sweep
    AlreadyCancelled,
    /// The hunt finished on its own and can no longer be cancelled
    Terminal,
}

/// Decide what a cancel request should do for a hunt status
///
/// Re-cancelling an already cancelled hunt is deliberately not an error so a
/// crash between the status persist and the stale tag sweep self heals on the
/// next cancel call.
///
/// # Arguments
///
/// * `status` - The hunt's current status
fn cancel_transition(status: HuntStatus) -> CancelAction {
    match status {
        // active hunts flip to cancelled
        HuntStatus::Created | HuntStatus::Running => CancelAction::Proceed,
        // cancelled hunts just re-run the stale tag sweep
        HuntStatus::Cancelled => CancelAction::AlreadyCancelled,
        // hunts that finished on their own stay in their terminal status
        HuntStatus::Completed | HuntStatus::Failed => CancelAction::Terminal,
    }
}

/// Compute a hunt's status after a checkpoint merges its facts
///
/// A terminal status is never revived: a checkpoint whose fresh read raced a
/// cancel keeps the cancel. A recorded failure beats completion since a hunt
/// that can no longer produce work did not honestly finish its corpus.
///
/// # Arguments
///
/// * `current` - The hunt's status from the fresh read inside the persist lock
/// * `all_exhausted` - Whether every lane has exhausted its time range
/// * `failed` - Whether a terminal failure has been recorded for this hunt
fn checkpoint_status(current: HuntStatus, all_exhausted: bool, failed: bool) -> HuntStatus {
    // terminal statuses are never revived by a late checkpoint
    if !current.is_active() {
        return current;
    }
    // a recorded failure stops the hunt
    if failed {
        return HuntStatus::Failed;
    }
    // the hunt completes once every lane exhausted its time range
    if all_exhausted {
        return HuntStatus::Completed;
    }
    // any won checkpoint means at least one chunk has committed
    HuntStatus::Running
}

/// Delete the stale active status tag rows a terminal hunt leaves behind
///
/// The intrinsic tag refresh only ever adds fresh values so the stale active
/// rows must be deleted explicitly or the quota census would count this hunt
/// forever. The db level delete skips group auth so a submitter who since
/// left a group can still cancel. This sweep is idempotent: terminal entry
/// paths re-run it so a crash between a terminal persist and its sweep self
/// heals on the next call.
///
/// # Arguments
///
/// * `entity` - The hunt entity whose stale active tags should be swept
/// * `shared` - Shared Thorium objects
async fn sweep_stale_active_tags(entity: &Entity, shared: &Shared) -> Result<(), ApiError> {
    // build the tag key for this hunt entity
    let entity_key = Entity::build_key(entity.id.to_string(), &());
    // build the delete for every active status value this hunt could carry
    let mut deletes = TagDeleteRequest::<Entity>::default().groups(entity.groups.clone());
    deletes.add_ref(Hunt::STATUS_TAG, HuntStatus::Created.to_string());
    deletes.add_ref(Hunt::STATUS_TAG, HuntStatus::Running.to_string());
    deletes.add_ref(Hunt::ACTIVE_TAG, "true");
    // delete any stale active status tag rows that exist
    db::tags::delete(&entity_key, &deletes, shared).await?;
    Ok(())
}

/// Delete the stale Created status tag row a now Running hunt leaves behind
///
/// The intrinsic tag refresh only ever adds fresh values so the Created row
/// from before the first checkpoint must be deleted explicitly or a Running
/// hunt would keep matching `HuntStatus=Created` browses forever. Idempotent
/// like every sweep so a crash before it lands self heals on the next
/// checkpoint that still sees the stale row.
///
/// # Arguments
///
/// * `entity` - The hunt entity whose stale Created tag should be swept
/// * `shared` - Shared Thorium objects
async fn sweep_created_status_tag(entity: &Entity, shared: &Shared) -> Result<(), ApiError> {
    // build the tag key for this hunt entity
    let entity_key = Entity::build_key(entity.id.to_string(), &());
    // build the delete for just the stale created status value
    let mut deletes = TagDeleteRequest::<Entity>::default().groups(entity.groups.clone());
    deletes.add_ref(Hunt::STATUS_TAG, HuntStatus::Created.to_string());
    // delete the stale created status row
    db::tags::delete(&entity_key, &deletes, shared).await?;
    Ok(())
}

/// Split a corpus window into contiguous time shard lanes
///
/// Lanes are walked newest to oldest by their own chunk chains. Internal lane
/// boundaries overlap by one second - the older lane's start is the boundary
/// plus one second - because the corpus page pull is strict on both ends, so a
/// sample uploaded exactly on a boundary would otherwise belong to no lane.
/// Windows shorter than the configured lane count in seconds get fewer lanes
/// so every lane is always at least one second wide and the overlap fits.
///
/// # Arguments
///
/// * `start` - The newest timestamp the corpus covers; must be after `end`
/// * `end` - The oldest timestamp the corpus covers
/// * `conf_lanes` - The configured number of lanes to try to split into
fn split_lanes(start: DateTime<Utc>, end: DateTime<Utc>, conf_lanes: u32) -> Vec<HuntLane> {
    // sub second windows still get one lane covering the whole range
    let span = (start - end).num_seconds().max(1);
    // use fewer lanes when the window is shorter than the lane count in
    // seconds so every lane shard is at least one second wide
    let count = i64::from(conf_lanes.max(1)).min(span);
    // the base shard width in seconds with the remainder spread below
    let base = span / count;
    let rem = span % count;
    // build our lanes newest to oldest
    let mut lanes = Vec::with_capacity(usize::try_from(count).unwrap_or(1));
    // track the newest boundary of the lane being built
    let mut boundary = start;
    for index in 0..count {
        // the newest lane starts at the window start while older lanes start
        // one second past the shared boundary for the strict bounds overlap
        let lane_start = if index == 0 {
            start
        } else {
            boundary + Duration::seconds(1)
        };
        // size this shard with the remainder spread over the newest lanes
        let width = base + i64::from(index < rem);
        // the oldest lane ends exactly at the window end so integer division
        // rounding can never drift the final boundary
        let lane_end = if index == count - 1 {
            end
        } else {
            boundary - Duration::seconds(width)
        };
        // build this lane with its chain not yet started
        lanes.push(HuntLane {
            start: lane_start,
            current: lane_start,
            end: lane_end,
            seq: 0,
            cursor: None,
            exhausted: false,
        });
        // the next lane's newest boundary is this lane's oldest
        boundary = lane_end;
    }
    lanes
}

/// How many extra active hunt lines the census lists beyond the global quota
///
/// The quota check is best-effort by design so concurrent creates can briefly
/// push past `max_active_global`; doubling the quota plus this flat margin
/// keeps those racers visible while keeping hydration O(quota). If even more
/// hunts are somehow active the census floor-truncates its active lists.
const ACTIVE_CENSUS_HEADROOM: usize = 16;

/// How many created-in-window hunts the census scans for recent activity
///
/// Ten times the recent display cap: hunt creation is human-initiated and
/// throttled by the active quotas, so a window holding more than this is
/// extraordinary. When a window overflows this bound the scan keeps the
/// newest lines, `recent` may miss older terminal hunts, and the per-user
/// `recent_created` counts saturate at what was scanned.
const WINDOW_SCAN_LIMIT: usize = 1_000;

/// Check whether the active hunt count meets or exceeds a quota
///
/// This check is best-effort by design: it counts `HuntActive` tags through
/// the eventually consistent tag census, so concurrent creates can briefly
/// exceed the quota. It calls the db list directly - deliberately bypassing
/// per group authorization - because the global quota must see other users'
/// hunts in groups the creator is not in; it must therefore only ever return
/// a count and never any listed data.
///
/// # Arguments
///
/// * `user` - The user whose create is being quota checked
/// * `submitter` - Scope the count to one submitter's hunts when set
/// * `cap` - The quota to compare the active count against
/// * `shared` - Shared Thorium objects
#[instrument(
    name = "entities::hunts::exceeds_active_quota",
    skip(user, shared),
    err(Debug)
)]
async fn exceeds_active_quota(
    user: &User,
    submitter: Option<&str>,
    cap: usize,
    shared: &Shared,
) -> Result<bool, ApiError> {
    // a zero cap disables hunt creation outright
    if cap == 0 {
        return Ok(true);
    }
    // expand to every group so the census sees all users active hunts
    let groups = db::groups::list(0, 10_000, shared).await?.names;
    // build the strict AND active hunt tag filter; HuntActive is written on no
    // other entity kind so the tag walk needs no kind term
    let mut tags: HashMap<String, Vec<String>> = HashMap::with_capacity(2);
    tags.insert(Hunt::ACTIVE_TAG.to_owned(), vec!["true".to_owned()]);
    // scope the count to one submitter through the intrinsic submitter tag
    if let Some(submitter) = submitter {
        tags.insert("submitter".to_owned(), vec![submitter.to_owned()]);
    }
    // list just enough lines to know whether the quota is already full
    let params = EntityListParams {
        groups,
        start: Utc::now(),
        end: None,
        tags,
        cursor: None,
        limit: cap,
        kinds: vec![EntityKinds::Hunt],
    };
    // dedupe so a multi group hunt only counts once across its tag rows; this
    // one shot read never hands its cursor id out so it must not persist one
    let cursor = db::entities::list_once(user, params, true, shared).await?;
    // the quota caps how many hunts may be active at once so a create while
    // the cap is already reached would push past it
    Ok(cursor.data.len() >= cap)
}

/// Resolve and freeze a hunt's selector into a rule id snapshot
///
/// Resolution runs under the creator's authorization so the frozen set is
/// exactly the rules the creator could see at creation. The engine's snapshot
/// filter excludes quarantined rules (counted), rule set members of another
/// engine's kind are excluded and counted, and a selector that resolves past
/// `max_rules_per_hunt` or down to zero usable rules is a 400.
///
/// # Arguments
///
/// * `user` - The user creating this hunt
/// * `engine` - The engine whose rules this hunt freezes
/// * `selector` - The selector to resolve
/// * `shared` - Shared Thorium objects
#[instrument(
    name = "entities::hunts::resolve_selector",
    skip(user, shared),
    err(Debug)
)]
async fn resolve_selector(
    user: &User,
    engine: HuntEngine,
    selector: &HuntSelector,
    shared: &Shared,
) -> Result<FrozenSnapshot, ApiError> {
    // get the frozen snapshot size cap
    let cap = shared.config.thorium.hunts.max_rules_per_hunt;
    // start with an empty snapshot
    let mut snapshot = FrozenSnapshot::default();
    // track seen ids so duplicates can never inflate the snapshot
    let mut seen: HashSet<Uuid> = HashSet::default();
    match selector {
        HuntSelector::Rules(ids) => {
            // an explicit selector with no rules can never produce work
            if ids.is_empty() {
                return bad!("Hunt rule selectors cannot be empty!".to_owned());
            }
            // dedupe the requested ids so the missing id diff stays truthful
            let unique: Vec<Uuid> = ids.iter().copied().filter(|id| seen.insert(*id)).collect();
            // enforce the snapshot cap before fetching anything
            if unique.len() > cap {
                return bad!(format!(
                    "Hunts are capped at {cap} rules (thorium.hunts.max_rules_per_hunt); \
                    narrow the selector"
                ));
            }
            // get all of the requested rules visible to this user in one batch
            let entities = for_groups!(db::entities::get_many, user, shared, &unique)?;
            // get_many silently drops ids that don't exist or aren't visible
            // so diff the results to name the first missing rule
            if entities.len() != unique.len() {
                // build a set of the ids that were found
                let found: HashSet<Uuid> = entities.iter().map(|entity| entity.id).collect();
                // find the first missing id to report
                if let Some(missing) = unique.iter().find(|rule_id| !found.contains(rule_id)) {
                    return not_found!(format!("Rule {missing} was not found!"));
                }
            }
            // freeze every explicitly named rule of this engines kind
            for entity in entities {
                // an explicitly named rule of the wrong kind is a user error
                if entity.kind != engine.rule_kind() {
                    return bad!(format!(
                        "Entity {} ({}) is not a {engine} rule!",
                        entity.name, entity.id
                    ));
                }
                // quarantined rules are excluded and counted even when named
                if engine.excludes_from_snapshot(&entity) {
                    snapshot.excluded_quarantined += 1;
                } else {
                    snapshot.rules.push(entity.id);
                }
            }
        }
        HuntSelector::RuleSet(id) => {
            // get this rule set under the creators visibility with its
            // membership hydrated from associations
            let ruleset = Entity::get(user, *id, shared).await?;
            // make sure this entity actually is a rule set
            let EntityMetadata::RuleSet(members) = &ruleset.metadata else {
                return bad!(format!("Entity {id} is not a rule set!"));
            };
            // freeze this rule sets members of our engines kind
            for member in &members.rules {
                // members of another engines kind are excluded and counted
                if member.kind != engine.rule_kind() {
                    snapshot.excluded_foreign += 1;
                    continue;
                }
                // quarantined members are excluded and counted
                if engine.excludes_from_snapshot(member) {
                    snapshot.excluded_quarantined += 1;
                    continue;
                }
                // freeze this member once no matter how many rows hydrated
                if seen.insert(member.id) {
                    snapshot.rules.push(member.id);
                }
            }
            // enforce the snapshot cap in case the rule set cap is configured
            // above the hunt cap
            if snapshot.rules.len() > cap {
                return bad!(format!(
                    "Hunts are capped at {cap} rules (thorium.hunts.max_rules_per_hunt); \
                    narrow the selector"
                ));
            }
        }
        HuntSelector::Tag { .. } | HuntSelector::All => {
            // a tag selector matches within the engines kind; all matches every
            // rule of that kind visible to the creator
            let tag_filter = match selector {
                HuntSelector::Tag { key, value } => {
                    HashMap::from([(key.clone(), vec![value.clone()])])
                }
                _ => HashMap::default(),
            };
            // page the entity list under the creators authorization until the
            // selector is exhausted or the snapshot cap aborts the walk
            let mut cursor_id = None;
            loop {
                // empty groups expand to the creators own visible groups
                let params = EntityListParams {
                    groups: Vec::default(),
                    start: Utc::now(),
                    end: None,
                    tags: tag_filter.clone(),
                    cursor: cursor_id,
                    limit: 1000,
                    kinds: vec![engine.rule_kind()],
                };
                // list one page and hydrate details since the quarantine
                // filter needs each rules tags
                let page = Entity::list(user, params, true, shared)
                    .await?
                    .details(user, shared)
                    .await?;
                // save the continuation before consuming the page
                cursor_id = page.cursor;
                // freeze this pages rules through the engines snapshot filter
                for entity in page.data {
                    // the tag walk cannot filter by kind server side so
                    // enforce the engines rule kind per line
                    if entity.kind != engine.rule_kind() {
                        continue;
                    }
                    // skip any rules we have already seen across pages
                    if !seen.insert(entity.id) {
                        continue;
                    }
                    // quarantined rules are excluded and counted
                    if engine.excludes_from_snapshot(&entity) {
                        snapshot.excluded_quarantined += 1;
                        continue;
                    }
                    // freeze this rule
                    snapshot.rules.push(entity.id);
                    // abort the walk the moment the snapshot passes the cap
                    if snapshot.rules.len() > cap {
                        return bad!(format!(
                            "Hunts are capped at {cap} rules \
                            (thorium.hunts.max_rules_per_hunt); narrow the selector"
                        ));
                    }
                }
                // stop walking once the selector is exhausted
                if cursor_id.is_none() {
                    break;
                }
            }
        }
    }
    // an empty snapshot can never produce work so reject it at create instead
    // of stranding a hunt that would only ever fail at load time
    if snapshot.rules.is_empty() {
        return bad!("Hunt selector resolved to 0 usable rules!".to_owned());
    }
    Ok(snapshot)
}

impl Entity {
    /// Create a hunt entity, freeze its rules, and start its lane chains
    ///
    /// Any user may create hunts. The selector resolves under the creator's
    /// authorization through the engine's snapshot filter, the corpus clamps
    /// to the creation instant, and the write order is entity (with its
    /// status/engine/active tags) then lane sequence keys then one depth 0
    /// chunk event per lane. An emission failure after the entity write leaves
    /// the hunt in Created with no chain behind it; the documented remedy is
    /// cancel and recreate.
    ///
    /// # Arguments
    ///
    /// * `user` - The user creating this hunt
    /// * `req` - The hunt create request to apply
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "Entity::create_hunt", skip_all, err(Debug))]
    pub async fn create_hunt(
        user: &User,
        req: HuntCreateRequest,
        shared: &Shared,
    ) -> Result<Entity, ApiError> {
        // make sure this engine has a shipped hunt plug; serde already rejects
        // unknown engine names so this match is the landing spot for engines
        // that gain a variant before their execution plug ships
        match req.engine {
            HuntEngine::Yara => (),
        }
        // make sure this corpus kind is walkable and get its requested bounds
        let bounds_req = match req.corpus {
            // repos stay designed in but rejected until an engine scans them
            Some(HuntCorpusRequest::Repos(_)) => {
                return bad!(
                    "Hunts cannot walk a Repos corpus until an engine scans repos".to_owned()
                );
            }
            Some(HuntCorpusRequest::Files(bounds)) => bounds,
            // an omitted corpus defaults to everything up to creation
            None => HuntCorpusBoundsRequest::default(),
        };
        // make sure this hunt is in at least one group before doing any work
        if req.groups.is_empty() {
            return bad!("Hunt must be in at least 1 group!".to_owned());
        }
        // make sure this user can create entities in all of these groups
        Group::authorize_check_allow_all(
            user,
            &req.groups,
            Group::editable,
            "edit",
            Some(GroupAllowAction::Entities),
            shared,
        )
        .await?;
        // corpus groups default to the hunts groups and must stay a subset so
        // every finding write stays bounded by the hunts own groups
        let corpus_groups = if bounds_req.groups.is_empty() {
            req.groups.clone()
        } else {
            // find any corpus group outside the hunts groups
            if let Some(outside) = bounds_req
                .groups
                .iter()
                .find(|group| !req.groups.contains(group))
            {
                return bad!(format!(
                    "Corpus group {outside} is not one of this hunts groups!"
                ));
            }
            bounds_req.groups.clone()
        };
        // get the hunt chassis settings
        let conf = &shared.config.thorium.hunts;
        // enforce the per user active hunt quota (best-effort by design)
        if exceeds_active_quota(user, Some(&user.username), conf.max_active_per_user, shared)
            .await?
        {
            return bad!(format!(
                "Too many active hunts for {}: limit {} (thorium.hunts.max_active_per_user)",
                user.username, conf.max_active_per_user
            ));
        }
        // enforce the global active hunt quota (best-effort by design)
        if exceeds_active_quota(user, None, conf.max_active_global, shared).await? {
            return bad!(format!(
                "Too many active hunts cluster-wide: limit {} (thorium.hunts.max_active_global)",
                conf.max_active_global
            ));
        }
        // resolve and freeze the selector through the engines snapshot filter
        let frozen = resolve_selector(user, req.engine, &req.selector, shared).await?;
        // clamp the corpus window: the scan path owns everything after the
        // creation instant and the oldest bound defaults to the corpus kinds
        // configured earliest
        let now = Utc::now();
        let start = bounds_req.start.map_or(now, |start| start.min(now));
        let end = match bounds_req.end {
            Some(end) => end,
            // fall back to the configured earliest file timestamp
            None => {
                match chrono::TimeZone::timestamp_opt(&Utc, shared.config.thorium.files.earliest, 0)
                {
                    chrono::LocalResult::Single(default_end) => default_end,
                    _ => {
                        return crate::internal_err!(format!(
                            "default earliest files timestamp is invalid or ambigous - {}",
                            shared.config.thorium.files.earliest
                        ));
                    }
                }
            }
        };
        // an inverted or empty window can never list anything
        if start <= end {
            return bad!(format!(
                "Corpus window is empty: start {start} must be after end {end}"
            ));
        }
        // split the corpus window into contiguous time shard lanes
        let lanes = split_lanes(start, end, conf.lanes);
        // every lane emits one first chunk event below
        let lane_count = u32::try_from(lanes.len()).unwrap_or(u32::MAX);
        // assemble the frozen hunt metadata in Created
        let hunt = Hunt {
            engine: req.engine,
            selector: req.selector,
            rules: frozen.rules,
            excluded_quarantined: frozen.excluded_quarantined,
            excluded_foreign: frozen.excluded_foreign,
            corpus: HuntCorpus::Files(HuntCorpusBounds {
                groups: corpus_groups,
                start,
                end,
                tags: bounds_req.tags,
                tags_case_insensitive: bounds_req.tags_case_insensitive,
            }),
            status: HuntStatus::Created,
            lanes,
            counters: HuntCounters::default(),
            failure: None,
        };
        // build the metadata form with our server built hunt in the carrier
        // field the multipart parser never sets
        let metadata = EntityMetadataForm {
            hunt: Some(hunt),
            ..EntityMetadataForm::default()
        };
        // build the entity form for this hunt
        let entity_form = EntityForm {
            name: Some(req.name),
            kind: Some(EntityKinds::Hunt),
            metadata,
            groups: req.groups,
            description: req.description,
            ..EntityForm::default()
        };
        // write this hunt and its intrinsic status/engine/active tags
        let entity = db::entities::create(user, entity_form, Uuid::new_v4(), shared).await?;
        // seed the per lane checkpoint sequence keys the chunk chains commit
        // through before any chunk event could reference them
        db::hunts::seed_lane_seqs(&entity.id, lane_count, shared).await?;
        // emit one depth 0 first chunk event per lane on the engines queue;
        // an emission failure here strands this hunt in Created (documented;
        // the remedy is cancel and recreate)
        for lane in 0..lane_count {
            // build the first chunk event for this lane
            let event = Event::hunt_chunk(&user.username, req.engine, entity.id, lane, 0);
            // enqueue this chunk event on the engines queue
            db::events::create(&event, shared).await?;
        }
        // re-read this hunt so the response carries its intrinsic tags
        db::entities::get(&entity.groups, entity.id, shared).await
    }

    /// Cancel a hunt
    ///
    /// Only the hunt's submitter or an admin may cancel it. Cancel flips the
    /// status and tags to Cancelled; running chunks observe the terminal
    /// status lazily within one chunk. Cancelling an already cancelled hunt is
    /// an idempotent no-op that re-runs the stale tag sweep; a hunt that
    /// finished on its own is a 400.
    ///
    /// # Arguments
    ///
    /// * `user` - The user cancelling this hunt
    /// * `id` - The id of the hunt to cancel
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "Entity::cancel_hunt", skip(user, shared), err(Debug))]
    pub async fn cancel_hunt(user: &User, id: Uuid, shared: &Shared) -> Result<(), ApiError> {
        // serialize against concurrent checkpoint persists so a checkpoint's
        // read-modify-write can never clobber the cancel back to Running
        let lock = db::hunts::lock_persist(&id, shared).await?;
        // run the cancel while holding the persist mutex
        let outcome = Self::cancel_hunt_locked(user, id, shared).await;
        // always release our mutex no matter how the cancel went
        let _ = db::hunts::unlock_persist(lock, shared).await;
        outcome
    }

    /// Cancel a hunt while holding its persist mutex
    ///
    /// # Arguments
    ///
    /// * `user` - The user cancelling this hunt
    /// * `id` - The id of the hunt to cancel
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "Entity::cancel_hunt_locked", skip(user, shared), err(Debug))]
    async fn cancel_hunt_locked(user: &User, id: Uuid, shared: &Shared) -> Result<(), ApiError> {
        // get this hunt under the requesting users visibility
        let mut entity = Entity::get(user, id, shared).await?;
        // make sure this entity actually is a hunt and get its status
        let status = match &entity.metadata {
            EntityMetadata::Hunt(hunt) => hunt.status,
            _ => return bad!(format!("Entity {id} is not a hunt!")),
        };
        // only the submitter or an admin may cancel a hunt
        can_modify!(entity.submitter, user);
        // decide what this cancel should do based on the current status
        match cancel_transition(status) {
            // hunts that finished on their own can no longer be cancelled
            CancelAction::Terminal => {
                return bad!(format!("Hunt {id} is already terminal ({status})"));
            }
            CancelAction::Proceed => {
                // flip this hunts status inside its metadata
                if let EntityMetadata::Hunt(hunt) = &mut entity.metadata {
                    hunt.status = HuntStatus::Cancelled;
                }
                // persist the cancelled metadata; this also upserts the fresh
                // HuntStatus=Cancelled tag and writes no active tag
                db::entities::update(user, entity.clone(), &[], &[], shared).await?;
            }
            // already cancelled hunts just re-run the stale tag sweep below so
            // a crash between the persist and the sweep self heals
            CancelAction::AlreadyCancelled => (),
        }
        // sweep the stale active status rows this hunt left behind
        sweep_stale_active_tags(&entity, shared).await?;
        Ok(())
    }

    /// Commit one processed hunt chunk
    ///
    /// Only the admin service account may checkpoint hunts. The lane sequence
    /// compare-and-set and the next chunk emission run in one atomic Redis
    /// script so duplicated checkpoints collapse instead of forking the lane
    /// chain; the entity's lanes and counters are the persisted view, merged
    /// under the per hunt persist mutex after the CAS. A lost CAS still merges
    /// the sticky facts (sequence floor, exhaustion, failure, progress) so a
    /// checkpoint whose view persist was lost is repaired by its own replay,
    /// then reports a 409 so the caller drops its stale chain.
    ///
    /// # Arguments
    ///
    /// * `user` - The user committing this checkpoint
    /// * `id` - The id of the hunt this checkpoint advances
    /// * `req` - The checkpoint to commit
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "Entity::checkpoint_hunt", skip(user, req, shared), err(Debug))]
    pub async fn checkpoint_hunt(
        user: &User,
        id: Uuid,
        req: HuntCheckpoint,
        shared: &Shared,
    ) -> Result<HuntCheckpointResponse, ApiError> {
        // only the admin service account commits hunt progress
        is_admin!(user);
        // get this hunt; admins see every group
        let entity = Entity::get(user, id, shared).await?;
        // make sure this entity actually is a hunt
        let EntityMetadata::Hunt(hunt) = &entity.metadata else {
            return bad!(format!("Entity {id} is not a hunt!"));
        };
        // make sure this checkpoint names a real lane
        let Some(lane_view) = hunt.lanes.get(req.lane as usize) else {
            return bad!(format!("Hunt {id} has no lane {}!", req.lane));
        };
        // a terminal hunt commits nothing; re-run the idempotent tag sweep so
        // a crash between a terminal persist and its sweep self heals on the
        // chunk's retry, exactly like re-cancelling an already cancelled hunt
        if !hunt.status.is_active() {
            sweep_stale_active_tags(&entity, shared).await?;
            return Ok(HuntCheckpointResponse {
                status: hunt.status,
                emitted: false,
            });
        }
        // an exhausted or failed lane ends its chain; everything else emits
        // the next chunk event in the same atomic step as the CAS
        let emit = !req.exhausted && req.failed.is_none();
        let next_event = emit
            .then(|| Event::hunt_chunk(&entity.submitter, hunt.engine, id, req.lane, req.seq + 1));
        // try to win this checkpoint's compare-and-set
        let commit = db::hunts::commit_lane(
            &id,
            req.lane,
            req.seq,
            lane_view.seq,
            hunt.engine.chunk_event_type(),
            next_event.as_ref(),
            shared.config.thorium.hunts.inter_chunk_pacing_secs,
            shared,
        )
        .await?;
        // merge this checkpoint into the persisted view under the hunt's
        // persist mutex; losses still merge their sticky facts
        let status = persist_checkpoint(user, id, &req, commit, shared).await?;
        // losers emit nothing and report a conflict so stale chains collapse
        if !commit.won {
            return conflict!(format!(
                "Hunt {id} lane {} expects seq {}, not {}",
                req.lane, commit.stored, req.seq
            ));
        }
        Ok(HuntCheckpointResponse {
            status,
            emitted: emit,
        })
    }

    /// List a page of a hunt's frozen rule snapshot
    ///
    /// The snapshot is immutable so the cursor is a plain index into the
    /// frozen rule vec encoded as a Uuid (the shared cursor wire type); a
    /// missing cursor starts at index zero. Page members are hydrated with a
    /// batch get under the caller's groups so rules that were deleted after
    /// the freeze, or that the caller cannot see, come back as id only lines
    /// (the two cases are deliberately indistinguishable). This is read only:
    /// hunt metadata is machine written and must never be mutated here.
    ///
    /// # Arguments
    ///
    /// * `user` - The user listing this hunts frozen rules
    /// * `id` - The id of the hunt to list frozen rules for
    /// * `params` - The query params for this list request
    /// * `shared` - Shared Thorium objects
    #[instrument(
        name = "Entity::list_hunt_rules",
        skip(user, params, shared),
        err(Debug)
    )]
    pub async fn list_hunt_rules(
        user: &User,
        id: Uuid,
        params: HuntRuleListParams,
        shared: &Shared,
    ) -> Result<ApiCursor<HuntRuleLine>, ApiError> {
        // get this hunt under the requesting users visibility
        let entity = Entity::get(user, id, shared).await?;
        // make sure this entity actually is a hunt
        let EntityMetadata::Hunt(hunt) = &entity.metadata else {
            return bad!(format!("Entity {id} is not a hunt!"));
        };
        // decode our index cursor; reject cursors that do not round trip as a
        // small index (a usize truncation of a foreign cursor uuid would
        // silently alias it onto an arbitrary page instead of erroring)
        let start = match params.cursor {
            // a missing cursor starts at index zero
            None => 0,
            // decode this cursor as a plain index into the frozen snapshot
            Some(cursor) => match usize::try_from(cursor.as_u128()) {
                // clamp in-range indexes to the snapshot so a final replay
                // of the last page stays a clean empty response
                Ok(index) if index <= hunt.rules.len() => index,
                // anything else was never a cursor this route handed out
                _ => return bad!(format!("Invalid hunt rule cursor {cursor}")),
            },
        };
        // clamp the limit to at least one line so a page always advances and
        // the returned cursor can never equal the one that produced it
        let limit = params.limit.max(1);
        // find where this page of frozen rule ids ends
        let end = start.saturating_add(limit).min(hunt.rules.len());
        // slice the page of frozen rule ids to hydrate
        let page = &hunt.rules[start..end];
        // hydrate the rules on this page that this user can see
        let found = for_groups!(db::entities::get_many, user, shared, page)?;
        // map the hydrated rules by id so the lines can keep the frozen order
        let mut found: HashMap<Uuid, Entity> = found
            .into_iter()
            .map(|entity| (entity.id, entity))
            .collect();
        // build this pages lines in frozen order
        let data = page
            .iter()
            .map(|rule_id| match found.remove(rule_id) {
                // this rule is visible so hydrate its line
                Some(rule) => {
                    // read the current quarantine status values off this rules tags
                    let status =
                        rule.tags
                            .get(YaraRule::STATUS_TAG)
                            .map_or_else(Vec::new, |values| {
                                // tag values are map keys so collect and sort them
                                let mut values: Vec<String> = values.keys().cloned().collect();
                                values.sort_unstable();
                                values
                            });
                    // read the exemption flag off this rules metadata
                    let quarantine_exempt = match &rule.metadata {
                        EntityMetadata::YaraRule(yara) => yara.quarantine_exempt,
                        _ => false,
                    };
                    // build the hydrated line for this frozen rule
                    HuntRuleLine {
                        id: *rule_id,
                        name: Some(rule.name),
                        kind: Some(rule.kind),
                        created: Some(rule.created),
                        status,
                        quarantine_exempt,
                    }
                }
                // deleted or invisible rules render as id only lines
                None => HuntRuleLine {
                    id: *rule_id,
                    name: None,
                    kind: None,
                    created: None,
                    status: Vec::new(),
                    quarantine_exempt: false,
                },
            })
            .collect();
        // only hand back a cursor when more frozen rules remain
        let cursor = (end < hunt.rules.len()).then(|| Uuid::from_u128(end as u128));
        Ok(ApiCursor {
            cursor,
            data,
            metadata: None,
        })
    }

    /// Assemble the admin hunt census for the status page
    ///
    /// The census is computed at read time from entities, tags, and queue
    /// depths; nothing per-user is ever persisted. Groups expand through the
    /// same 10,000 group walk the quota census uses (never `for_groups!` and
    /// its 1,000 group cap) so both views see the same cluster, and active
    /// hunts split on their authoritative metadata status: a hunt whose
    /// terminal transition raced the active tag sweep sorts into neither
    /// active list, which is correct rather than a bug.
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is reading the census
    /// * `params` - The query params bounding the census
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "Entity::hunt_system_status", skip_all, err(Debug))]
    pub async fn hunt_system_status(
        user: &User,
        params: HuntStatusParams,
        shared: &Shared,
    ) -> Result<HuntSystemStatus, ApiError> {
        // only admins can read the cross group hunt census
        is_admin!(user);
        // clamp the display bounds to their caps
        let recent_cap = params.recent.min(100);
        let window_days = i64::from(params.window_days.clamp(1, 30));
        // get the hunt chassis settings
        let conf = &shared.config.thorium.hunts;
        // expand to every group so the census sees all users hunts
        let groups = db::groups::list(0, 10_000, shared).await?.names;
        // build the strict AND active hunt tag filter
        let mut active_tags: HashMap<String, Vec<String>> = HashMap::with_capacity(1);
        active_tags.insert(Hunt::ACTIVE_TAG.to_owned(), vec!["true".to_owned()]);
        // list the active hunts with headroom beyond the global quota since
        // the quota check is best effort and racing creates can pass it
        let active_params = EntityListParams {
            groups: groups.clone(),
            start: Utc::now(),
            end: None,
            tags: active_tags,
            cursor: None,
            limit: conf
                .max_active_global
                .saturating_mul(2)
                .saturating_add(ACTIVE_CENSUS_HEADROOM),
            kinds: vec![EntityKinds::Hunt],
        };
        // scan the created-in-window hunts for recent and per user activity;
        // the kind filter applies server side on this tag free list path
        let window_params = EntityListParams {
            groups: groups.clone(),
            start: Utc::now(),
            end: Some(Utc::now() - Duration::days(window_days)),
            tags: HashMap::default(),
            cursor: None,
            limit: WINDOW_SCAN_LIMIT,
            kinds: vec![EntityKinds::Hunt],
        };
        // the active branch, the window branch, and the depth read are
        // independent so they run concurrently; status lines never read tags
        // so both hydrations skip them, and neither one shot list ever hands
        // its cursor id out so neither persists a cursor
        let active_branch = async {
            // dedupe so a multi group hunt only counts once across its tag rows
            let active_cursor = db::entities::list_once(user, active_params, true, shared).await?;
            // hydrate the active lines through the explicitly expanded group
            // list so the detail fetch sees every group the tag walk did
            let active_ids: Vec<Uuid> = active_cursor.data.iter().map(|line| line.id).collect();
            db::entities::get_many_bare(&groups, &active_ids, shared).await
        };
        let window_branch = async {
            // dedupe multi group hunts here too
            let window_cursor = db::entities::list_once(user, window_params, true, shared).await?;
            // hydrate the window lines so terminal status and submitter are known
            let window_ids: Vec<Uuid> = window_cursor.data.iter().map(|line| line.id).collect();
            db::entities::get_many_bare(&groups, &window_ids, shared).await
        };
        // fetch both branches and the queue depths concurrently
        let (active_entities, window_entities, depths) =
            tokio::try_join!(active_branch, window_branch, db::events::depths(shared))?;
        // split the active hunts on their authoritative metadata status
        let mut ongoing = Vec::new();
        let mut upcoming = Vec::new();
        // the per-user quota every census row reports
        let quota = conf.max_active_per_user as u64;
        // build the per user census rows as the hydrated hunts stream past
        let mut users: HashMap<String, UserHuntCensus> = HashMap::new();
        for entity in active_entities {
            // project this entity into a bounded status line
            let Some(line) = HuntStatusLine::from_entity(entity) else {
                continue;
            };
            // a terminal hunt here just raced the tag sweep so skip it
            if !line.status.is_active() {
                continue;
            }
            // count this hunt against its submitters active census
            let row = users
                .entry(line.submitter.clone())
                .or_insert_with(|| UserHuntCensus::new(&line.submitter, quota));
            row.active += 1;
            // running hunts are ongoing while created hunts are upcoming
            if line.status == HuntStatus::Running {
                ongoing.push(line);
            } else {
                upcoming.push(line);
            }
        }
        // walk the window newest first keeping terminal lines up to the cap
        let mut recent = Vec::with_capacity(recent_cap.min(window_entities.len()));
        for entity in window_entities {
            // project this entity into a bounded status line
            let Some(line) = HuntStatusLine::from_entity(entity) else {
                continue;
            };
            // count this hunt against its creators recent census
            let row = users
                .entry(line.submitter.clone())
                .or_insert_with(|| UserHuntCensus::new(&line.submitter, quota));
            row.recent_created += 1;
            // keep the newest terminal hunts up to the display cap
            if !line.status.is_active() && recent.len() < recent_cap {
                recent.push(line);
            }
        }
        // keep the hunt chunk backlog kinds from the queue depths
        let chunk_backlog: HashMap<EventType, KindDepth> = depths
            .kinds
            .into_iter()
            .filter(|(kind, _)| kind.is_hunt_chunk())
            .collect();
        // report global usage from the same split the active lists show
        let quotas = HuntQuotaView {
            max_active_per_user: conf.max_active_per_user as u64,
            max_active_global: conf.max_active_global as u64,
            active_global: (ongoing.len() + upcoming.len()) as u64,
        };
        // order the user census deterministically: busiest first then by name
        let mut users: Vec<UserHuntCensus> = users.into_values().collect();
        users.sort_unstable_by(|left, right| {
            right
                .active
                .cmp(&left.active)
                .then_with(|| right.recent_created.cmp(&left.recent_created))
                .then_with(|| left.username.cmp(&right.username))
        });
        Ok(HuntSystemStatus {
            ongoing,
            upcoming,
            recent,
            users,
            chunk_backlog,
            quotas,
        })
    }
}

impl<S> FromRequestParts<S> for HuntRuleListParams
where
    S: Send + Sync,
{
    type Rejection = ApiError;

    /// Extract hunt frozen rule list params from a requests query string
    ///
    /// # Arguments
    ///
    /// * `parts` - The request parts to extract params from
    /// * `_state` - The shared state for this extractor (unused)
    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        // try to extract our query
        if let Some(query) = parts.uri.query() {
            // try to deserialize our query string
            Ok(serde_qs::Config::new()
                .max_depth(5)
                .deserialize_str(query)?)
        } else {
            // provide default params if none were given
            Ok(Self::default())
        }
    }
}

impl<S> FromRequestParts<S> for HuntStatusParams
where
    S: Send + Sync,
{
    type Rejection = ApiError;

    /// Extract hunt census params from a requests query string
    ///
    /// # Arguments
    ///
    /// * `parts` - The request parts to extract params from
    /// * `_state` - The shared state for this extractor (unused)
    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        // try to extract our query
        if let Some(query) = parts.uri.query() {
            // try to deserialize our query string
            Ok(serde_qs::Config::new()
                .max_depth(5)
                .deserialize_str(query)?)
        } else {
            // provide default params if none were given
            Ok(Self::default())
        }
    }
}

/// Merge one checkpoint into a hunt's persisted view under its persist mutex
///
/// The write is a read-modify-write of the whole hunt metadata blob, so it
/// re-reads fresh inside the mutex, verifies the mutex right before writing,
/// and re-applies on an expired hold - counters are only absorbed by the first
/// write attempt of a won commit so a re-apply can never double count.
///
/// # Arguments
///
/// * `user` - The user committing this checkpoint
/// * `id` - The id of the hunt this checkpoint advances
/// * `req` - The checkpoint being committed
/// * `commit` - The outcome of this checkpoint's compare-and-set
/// * `shared` - Shared Thorium objects
#[instrument(
    name = "entities::hunts::persist_checkpoint",
    skip(user, req, shared),
    err(Debug)
)]
async fn persist_checkpoint(
    user: &User,
    id: Uuid,
    req: &HuntCheckpoint,
    commit: db::hunts::LaneCommit,
    shared: &Shared,
) -> Result<HuntStatus, ApiError> {
    // only a won commit may absorb counter deltas and only once
    let mut absorb = commit.won;
    // persist under the mutex with a bounded number of re-apply attempts
    for _ in 0..3 {
        // serialize entity writers for this hunt
        let lock = db::hunts::lock_persist(&id, shared).await?;
        // apply this checkpoint onto a fresh read inside the mutex
        let applied = apply_checkpoint(user, id, req, commit, absorb, shared, &lock).await;
        // always release our mutex; a false return means it expired mid hold
        let released_ours = db::hunts::unlock_persist(lock, shared).await?;
        match applied {
            // the write landed while we held the mutex the whole time
            Ok(Some((status, sweep_created))) => {
                if released_ours {
                    // sweep the stale status rows this checkpoint left behind;
                    // the intrinsic refresh only ever adds fresh values
                    if !status.is_active() {
                        // a terminal transition sweeps every active row
                        let entity = Entity::get(user, id, shared).await?;
                        sweep_stale_active_tags(&entity, shared).await?;
                    } else if sweep_created {
                        // a running hunt sheds its stale created row
                        let entity = Entity::get(user, id, shared).await?;
                        sweep_created_status_tag(&entity, shared).await?;
                    }
                    return Ok(status);
                }
                // the mutex expired mid write so a peer may have clobbered our
                // write; re-apply the idempotent lane facts without counters
                absorb = false;
            }
            // the mutex expired before the write so nothing was persisted;
            // retry the whole apply with our absorb state unchanged
            Ok(None) => (),
            // real errors bubble up; the chunk retries and converges
            Err(error) => return Err(error),
        }
    }
    // the mutex kept expiring under us; the chunk's retry repairs the view
    // through the loss path merge
    internal_err!(format!(
        "Failed to persist a checkpoint for hunt {id} lane {}",
        req.lane
    ))
}

/// Apply one checkpoint onto a fresh hunt read and persist it
///
/// Returns `Ok(None)` when the persist mutex expired before the write so the
/// caller can retry without any state having changed; a successful apply also
/// reports whether a stale Created status row still needs sweeping.
///
/// # Arguments
///
/// * `user` - The user committing this checkpoint
/// * `id` - The id of the hunt this checkpoint advances
/// * `req` - The checkpoint being committed
/// * `commit` - The outcome of this checkpoint's compare-and-set
/// * `absorb` - Whether this apply may absorb the checkpoint's counter deltas
/// * `shared` - Shared Thorium objects
/// * `lock` - The held persist mutex to verify right before writing
#[allow(clippy::too_many_arguments)]
#[instrument(
    name = "entities::hunts::apply_checkpoint",
    skip(user, req, shared, lock),
    err(Debug)
)]
async fn apply_checkpoint(
    user: &User,
    id: Uuid,
    req: &HuntCheckpoint,
    commit: db::hunts::LaneCommit,
    absorb: bool,
    shared: &Shared,
    lock: &db::hunts::HuntPersistLock,
) -> Result<Option<(HuntStatus, bool)>, ApiError> {
    // read this hunt fresh inside the mutex so sibling lane updates and
    // concurrent cancels are never lost to a stale read
    let mut entity = Entity::get(user, id, shared).await?;
    // a running hunt still carrying its created status row needs it swept;
    // checking the fresh tags (not just the transition edge) self heals a
    // crash between an earlier persist and its sweep
    let created_tagged = entity
        .tags
        .get(Hunt::STATUS_TAG)
        .is_some_and(|values| values.contains_key(&HuntStatus::Created.to_string()));
    // make sure this entity still is a hunt
    let EntityMetadata::Hunt(hunt) = &mut entity.metadata else {
        return bad!(format!("Entity {id} is not a hunt!"));
    };
    // make sure this lane still exists on the fresh read
    let Some(lane) = hunt.lanes.get_mut(req.lane as usize) else {
        return bad!(format!("Hunt {id} has no lane {}!", req.lane));
    };
    // the entity floor tracks the authoritative sequence but never leads it
    lane.seq = lane.seq.max(commit.stored);
    if commit.won {
        // a won commit owns this lane's whole view
        lane.current = req.current;
        lane.cursor = req.cursor;
        lane.exhausted = req.exhausted;
    } else {
        // a lost commit only merges facts that must survive a lost persist:
        // exhaustion is sticky, progress only ever moves toward the lane end,
        // and a live cursor is better than none
        lane.exhausted |= req.exhausted;
        if req.current < lane.current {
            lane.current = req.current;
        }
        if lane.cursor.is_none() {
            lane.cursor = req.cursor;
        }
    }
    // only the first write attempt of a won commit absorbs counter deltas
    if absorb {
        hunt.counters.absorb(
            req.counters.clone(),
            shared.config.thorium.hunts.max_error_samples,
        );
    }
    // a recorded failure is sticky across replays and lost persists
    if hunt.failure.is_none() {
        hunt.failure = req.failed.clone();
    }
    // recompute this hunts status from the merged facts
    let all_exhausted = hunt.lanes.iter().all(|lane| lane.exhausted);
    hunt.status = checkpoint_status(hunt.status, all_exhausted, hunt.failure.is_some());
    let status = hunt.status;
    // verify our mutex right before the write so an expired hold is detected
    // while the write can still be abandoned instead of clobbering a peer
    if !db::hunts::persist_lock_held(lock, shared).await? {
        return Ok(None);
    }
    // persist the merged view; this also upserts the fresh status tags
    db::entities::update(user, entity, &[], &[], shared).await?;
    // the stale created row needs sweeping once this hunt is running
    Ok(Some((
        status,
        created_tagged && status == HuntStatus::Running,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Check the structural invariants every lane split must uphold
    ///
    /// # Arguments
    ///
    /// * `lanes` - The lanes to check
    /// * `start` - The newest timestamp the window covers
    /// * `end` - The oldest timestamp the window covers
    fn check_lane_invariants(lanes: &[HuntLane], start: DateTime<Utc>, end: DateTime<Utc>) {
        // the newest lane must start exactly at the window start
        assert_eq!(lanes.first().unwrap().start, start);
        // the oldest lane must end exactly at the window end
        assert_eq!(lanes.last().unwrap().end, end);
        for (index, lane) in lanes.iter().enumerate() {
            // every lane must cover a non empty range
            assert!(lane.start > lane.end, "lane {index} is empty or inverted");
            // every lane must start its chain unexhausted at its own start
            assert_eq!(lane.current, lane.start);
            assert_eq!(lane.seq, 0);
            assert!(lane.cursor.is_none());
            assert!(!lane.exhausted);
            // every older lane must start one second past the shared boundary
            if index > 0 {
                assert_eq!(
                    lane.start,
                    lanes[index - 1].end + Duration::seconds(1),
                    "lane {index} does not overlap its boundary by one second"
                );
            }
        }
    }

    /// An evenly divisible window must split into equal width lanes
    #[test]
    fn split_lanes_even() {
        // build a 4 hour window split into 4 lanes
        let start = Utc::now();
        let end = start - Duration::hours(4);
        let lanes = split_lanes(start, end, 4);
        // the window must split into exactly 4 lanes
        assert_eq!(lanes.len(), 4);
        check_lane_invariants(&lanes, start, end);
        // every boundary must land exactly on an hour step
        for (index, lane) in lanes.iter().enumerate() {
            let expected_end = start - Duration::hours(i64::try_from(index).unwrap() + 1);
            assert_eq!(lane.end, expected_end, "lane {index} boundary drifted");
        }
    }

    /// A remainder must spread over the newest lanes without drifting the end
    #[test]
    fn split_lanes_remainder() {
        // build a 10 second window split into 4 lanes (base 2, remainder 2)
        let start = Utc::now();
        let end = start - Duration::seconds(10);
        let lanes = split_lanes(start, end, 4);
        // the window must still split into 4 lanes
        assert_eq!(lanes.len(), 4);
        check_lane_invariants(&lanes, start, end);
        // the two newest lanes must absorb the remainder second each
        assert_eq!(lanes[0].end, start - Duration::seconds(3));
        assert_eq!(lanes[1].end, start - Duration::seconds(6));
        assert_eq!(lanes[2].end, start - Duration::seconds(8));
    }

    /// A window shorter than the lane count must produce fewer lanes
    #[test]
    fn split_lanes_degenerate_short_window() {
        // build a 2 second window asked to split into 4 lanes
        let start = Utc::now();
        let end = start - Duration::seconds(2);
        let lanes = split_lanes(start, end, 4);
        // only 2 lanes fit a 2 second window with 1 second shards
        assert_eq!(lanes.len(), 2);
        check_lane_invariants(&lanes, start, end);
    }

    /// A sub second window must collapse to a single lane
    #[test]
    fn split_lanes_subsecond_single() {
        // build a window narrower than one second
        let start = Utc::now();
        let end = start - Duration::milliseconds(250);
        let lanes = split_lanes(start, end, 4);
        // the whole window must be one lane covering the full range
        assert_eq!(lanes.len(), 1);
        assert_eq!(lanes[0].start, start);
        assert_eq!(lanes[0].end, end);
    }

    /// A lane count of zero must be clamped up to a single lane
    #[test]
    fn split_lanes_zero_lanes_clamps() {
        // build an hour window asked to split into 0 lanes
        let start = Utc::now();
        let end = start - Duration::hours(1);
        let lanes = split_lanes(start, end, 0);
        // the whole window must be one lane covering the full range
        assert_eq!(lanes.len(), 1);
        check_lane_invariants(&lanes, start, end);
    }

    /// Checkpoint status transitions must follow the hunt lifecycle rules
    #[test]
    fn checkpoint_status_matrix() {
        // the first won checkpoint moves a created hunt to running
        assert_eq!(
            checkpoint_status(HuntStatus::Created, false, false),
            HuntStatus::Running
        );
        // a running hunt with work left stays running
        assert_eq!(
            checkpoint_status(HuntStatus::Running, false, false),
            HuntStatus::Running
        );
        // every lane exhausted completes the hunt
        assert_eq!(
            checkpoint_status(HuntStatus::Running, true, false),
            HuntStatus::Completed
        );
        assert_eq!(
            checkpoint_status(HuntStatus::Created, true, false),
            HuntStatus::Completed
        );
        // a recorded failure stops the hunt and beats completion
        assert_eq!(
            checkpoint_status(HuntStatus::Running, false, true),
            HuntStatus::Failed
        );
        assert_eq!(
            checkpoint_status(HuntStatus::Running, true, true),
            HuntStatus::Failed
        );
        // terminal statuses are never revived by a late checkpoint
        for terminal in [
            HuntStatus::Cancelled,
            HuntStatus::Completed,
            HuntStatus::Failed,
        ] {
            assert_eq!(checkpoint_status(terminal, false, false), terminal);
            assert_eq!(checkpoint_status(terminal, true, true), terminal);
        }
    }

    /// Every hunt status must map to the right cancel action
    #[test]
    fn cancel_transition_matrix() {
        // active hunts must proceed to cancelled
        assert_eq!(
            cancel_transition(HuntStatus::Created),
            CancelAction::Proceed
        );
        assert_eq!(
            cancel_transition(HuntStatus::Running),
            CancelAction::Proceed
        );
        // already cancelled hunts must stay idempotent
        assert_eq!(
            cancel_transition(HuntStatus::Cancelled),
            CancelAction::AlreadyCancelled
        );
        // hunts that finished on their own must reject the cancel
        assert_eq!(
            cancel_transition(HuntStatus::Completed),
            CancelAction::Terminal
        );
        assert_eq!(
            cancel_transition(HuntStatus::Failed),
            CancelAction::Terminal
        );
    }
}
