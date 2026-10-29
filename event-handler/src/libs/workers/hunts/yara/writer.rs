//! The single audited code path that writes yara hits with partitioned groups
//!
//! Every write this module makes is bounded by the write groups formula:
//! `write_groups = rule.groups ∩ event.groups ∩ allowable_groups`. An empty
//! intersection writes nothing — not even for the rule owner — which is what
//! keeps hits leak free in both directions. All sample tags written here carry
//! `trigger_depth = event depth + 1` (saturating) so hit tags can never reset
//! the trigger systems only loop breaker.

use chrono::prelude::*;
use std::collections::HashMap;
use std::sync::Arc;
use thorium::models::{
    AssociationKind, AssociationTarget, Entity, EntityMetadata, EntityUpdate, Group, MatchedRule,
    Sample, TagRequest, YaraHitWriteRequest, YaraHitWriteResponse,
};
use thorium::{Error, Thorium};
use tracing::{Level, event, instrument};
use uuid::Uuid;

use super::context::{ENGINE_VERSION, RuleMeta};

/// The skip marker tag key written on samples that will not be scanned
pub const SKIP_TAG: &str = "YaraScanSkipped";

/// The per group allowability snapshot the hit writer filters against
///
/// Group kill switches bind even admins and entity/association/tag creates
/// authorize every requested group in one call, so one disallowing group would
/// 401 whole writes; the writer pre-filters instead and counts suppressions.
#[derive(Debug, Clone, Copy, Default)]
pub struct GroupAllowances {
    /// Whether this group allows entity writes
    pub entities: bool,
    /// Whether this group allows association writes
    pub associations: bool,
    /// Whether this group allows tag writes
    pub tags: bool,
}

impl From<&Group> for GroupAllowances {
    /// Snapshot the hit relevant allowances from a group
    ///
    /// # Arguments
    ///
    /// * `group` - The group to snapshot allowances from
    fn from(group: &Group) -> Self {
        GroupAllowances {
            entities: group.allowed.entities,
            associations: group.allowed.associations,
            tags: group.allowed.tags,
        }
    }
}

/// Compute the groups a hit may be written into
///
/// A group makes the cut only when it can see the sample through this event,
/// can see the rule, and allows entity, association, and tag writes.
///
/// # Arguments
///
/// * `rule_groups` - The groups the matching rule is in
/// * `event_groups` - The groups this scan event made the sample visible too
/// * `allowances` - The per group allowability snapshot
pub fn write_groups(
    rule_groups: &[String],
    event_groups: &[String],
    allowances: &HashMap<String, GroupAllowances>,
) -> Vec<String> {
    // intersect the rules groups with the events groups and the kill switches
    event_groups
        .iter()
        .filter(|group| rule_groups.contains(group))
        .filter(|group| {
            // unknown groups are treated as disallowing to stay leak safe
            allowances
                .get(group.as_str())
                .is_some_and(|allowed| allowed.entities && allowed.associations && allowed.tags)
        })
        .cloned()
        .collect()
}

/// Compute the groups a skip marker tag may be written into
///
/// Skip markers are tags only so they are filtered by tag allowability alone;
/// a tags-allowed group that disallows entities still gets its skip marker.
///
/// # Arguments
///
/// * `event_groups` - The groups this scan event made the sample visible too
/// * `allowances` - The per group allowability snapshot
pub fn skip_groups(
    event_groups: &[String],
    allowances: &HashMap<String, GroupAllowances>,
) -> Vec<String> {
    // filter the events groups down to the tag allowing ones
    event_groups
        .iter()
        .filter(|group| {
            // unknown groups are treated as disallowing to stay leak safe
            allowances
                .get(group.as_str())
                .is_some_and(|allowed| allowed.tags)
        })
        .cloned()
        .collect()
}

/// Build a sample tag request with the loop breaking trigger depth stamped
///
/// Sample tags emit `NewTags` events whose depth defaults to 0; an unstamped
/// tag written by the scanner would reset lineage depth and let a trigger plus
/// unpacker pair loop forever. The depth saturates so a depth of 255 can never
/// wrap back to 0.
///
/// # Arguments
///
/// * `groups` - The groups to write this tag into
/// * `event_depth` - The depth of the scan event this tag is for
/// * `key` - The tag key to write
/// * `value` - The tag value to write
pub fn stamped_tag(
    groups: Vec<String>,
    event_depth: u8,
    key: &str,
    value: String,
) -> TagRequest<Sample> {
    // build a tag request carrying the stamped trigger depth
    TagRequest::<Sample>::default()
        .groups(groups)
        .trigger_depth(event_depth.saturating_add(1))
        .add(key, value)
}

/// The single audited writer for yara hits, tags, and skip markers
pub struct YaraHitWriter {
    /// A service account client for Thorium
    thorium: Arc<Thorium>,
}

impl YaraHitWriter {
    /// Create a new hit writer
    ///
    /// # Arguments
    ///
    /// * `thorium` - A service account client for Thorium
    pub fn new(thorium: Arc<Thorium>) -> Self {
        YaraHitWriter { thorium }
    }

    /// Write one rules hit through the server side hit write route
    ///
    /// The route recomputes the write groups from authoritative server state
    /// (the rules groups, the samples current groups, and fresh kill switch
    /// allowances) and performs the whole partitioned write — entity,
    /// associations, depth stamped tag, and auto flag actions — in one
    /// audited idempotent call. The raw event groups are sent so a server
    /// view fresher than this workers caches still writes correctly; the
    /// response carries the per cause split for truthful stats.
    ///
    /// # Arguments
    ///
    /// * `rule_id` - The entity id of the rule that hit
    /// * `meta` - The scan time metadata for the rule that hit
    /// * `sha256` - The sha256 of the sample that was hit
    /// * `matched` - The bounded match info for this hit
    /// * `ruleset_hash` - The hash of the ruleset build this hit came from
    /// * `event_groups` - The raw groups the scan event made the sample visible too
    /// * `event_depth` - The depth of the scan event this hit is for
    /// * `hunt` - The hunt this scan ran for, if any
    #[allow(clippy::too_many_arguments)]
    #[instrument(name = "YaraHitWriter::write_hit", skip_all, err(Debug))]
    pub async fn write_hit(
        &self,
        rule_id: Uuid,
        meta: &RuleMeta,
        sha256: &str,
        matched: Vec<MatchedRule>,
        ruleset_hash: &str,
        event_groups: Vec<String>,
        event_depth: u8,
        hunt: Option<Uuid>,
    ) -> Result<YaraHitWriteResponse, Error> {
        // build the bounded hit write request from this scans facts
        let req = YaraHitWriteRequest {
            rule_id,
            sha256: sha256.to_owned(),
            rule_hash: meta.rule_hash.clone(),
            ruleset_hash: ruleset_hash.to_owned(),
            engine_version: ENGINE_VERSION.to_owned(),
            scanned_at: Utc::now(),
            event_groups,
            event_depth,
            matched,
            hunt,
        }
        .bound();
        // write this hit in one audited server side call
        let resp = self.thorium.entities.create_yara_hit(&req).await?;
        Ok(resp)
    }

    /// Record a deliberate skip so it never masquerades as a repairable gap
    ///
    /// # Arguments
    ///
    /// * `sha256` - The sha256 of the sample that was skipped
    /// * `reason` - The skip reason and its bound (e.g. `oversize:<cap>`)
    /// * `groups` - The tag allowable groups to record the skip in
    /// * `event_depth` - The depth of the scan event this skip is for
    #[instrument(name = "YaraHitWriter::write_skip_marker", skip_all, err(Debug))]
    pub async fn write_skip_marker(
        &self,
        sha256: &str,
        reason: String,
        groups: Vec<String>,
        event_depth: u8,
    ) -> Result<(), Error> {
        // a fully disallowed event writes no marker at all
        if groups.is_empty() {
            return Ok(());
        }
        // build the skip marker tag at the stamped depth
        let tag_req = stamped_tag(groups, event_depth, SKIP_TAG, reason);
        // record this deliberate skip
        self.thorium.files.tag(sha256, &tag_req).await?;
        Ok(())
    }

    /// Shrink or delete this samples hits after groups were revoked
    ///
    /// Each hit on this sample recomputes `rule.groups ∩ current sample groups`
    /// and drops the difference; a hit whose recompute is empty is deleted
    /// outright since entity updates reject empty group sets (its deterministic
    /// identity recreates it on any future merge).
    ///
    /// # Arguments
    ///
    /// * `sha256` - The sha256 of the sample whose groups were revoked
    /// * `rule_groups` - The groups of every rule in the context by rule id
    #[instrument(name = "YaraHitWriter::shrink", skip_all, err(Debug))]
    pub async fn shrink(
        &self,
        sha256: &str,
        rule_groups: &HashMap<Uuid, Vec<String>>,
    ) -> Result<(), Error> {
        // get this samples current groups; a fully deleted sample has no hits
        // to shrink since its per group rows are already gone
        let sample_groups = match self.thorium.files.get(sha256).await {
            Ok(sample) => sample
                .groups()
                .into_iter()
                .map(ToOwned::to_owned)
                .collect::<Vec<String>>(),
            Err(_) => Vec::default(),
        };
        // list this samples associations to find its hits; a fully deleted
        // sample 404s here and has nothing left to shrink - erroring instead
        // would leave its revocation event in flight retrying forever (the
        // dangling hit side is the deferred association reapers problem)
        let opts = thorium::models::AssociationListOpts::default();
        let mut cursor = match self.thorium.associations.list_file(sha256, &opts).await {
            Ok(cursor) => cursor,
            Err(error) => {
                // a deleted sample is a convergent no-op, not a retryable failure
                if error.status() == Some(reqwest::StatusCode::NOT_FOUND) {
                    // log that this samples hits are already beyond shrinking
                    event!(
                        Level::INFO,
                        msg = "Sample no longer exists; nothing to shrink",
                        sha256 = sha256,
                    );
                    return Ok(());
                }
                return Err(error);
            }
        };
        // track the hit entities we found on this sample
        let mut hit_ids = Vec::default();
        loop {
            // crawl this page of associations for yara rule hits
            for assoc in cursor.data.drain(..) {
                // only yara rule hit associations point at hit entities
                if assoc.kind == AssociationKind::YaraRuleHit {
                    // the other side of this association is the hit entity
                    if let AssociationTarget::Entity { id, .. } = assoc.other {
                        hit_ids.push(id);
                    }
                }
            }
            // if this cursor is exhausted then stop listing
            if cursor.exhausted() {
                break;
            }
            // this cursor has more data so get the next page
            cursor.refill().await?;
        }
        // recompute each hits groups against the samples current groups
        for hit_id in hit_ids {
            // get this hit entity
            let entity: Entity = match self.thorium.entities.get(hit_id).await {
                Ok(entity) => entity,
                // a hit racing a delete is already converged
                Err(_) => continue,
            };
            // pull the rule id off this hits metadata
            let EntityMetadata::YaraRuleHit(hit) = &entity.metadata else {
                continue;
            };
            // a rule missing from the context keeps its hits untouched; the
            // reconciliation sweep (deferred) covers deleted rules
            let Some(groups) = rule_groups.get(&hit.rule_id) else {
                continue;
            };
            // recompute the groups this hit should still be visible in
            let desired = groups
                .iter()
                .filter(|group| sample_groups.contains(group))
                .cloned()
                .collect::<Vec<String>>();
            // find the groups this hit must lose
            let remove = entity
                .groups
                .iter()
                .filter(|group| !desired.contains(group))
                .cloned()
                .collect::<Vec<String>>();
            // a hit that keeps all its groups needs no shrink
            if remove.is_empty() {
                continue;
            }
            // a hit with no remaining visibility is deleted outright
            if desired.is_empty() {
                // delete this fully revoked hit
                if let Err(error) = self.thorium.entities.delete(hit_id).await {
                    // log that we failed to delete a revoked hit
                    event!(
                        Level::ERROR,
                        msg = "Failed to delete revoked yara hit",
                        hit = hit_id.to_string(),
                        error = error.to_string(),
                    );
                }
                continue;
            }
            // shrink this hits groups to the recomputed set
            let update = EntityUpdate {
                remove_groups: remove,
                ..Default::default()
            };
            // apply the shrink to this hit
            if let Err(error) = self.thorium.entities.update(hit_id, update).await {
                // log that we failed to shrink a revoked hit
                event!(
                    Level::ERROR,
                    msg = "Failed to shrink revoked yara hit",
                    hit = hit_id.to_string(),
                    error = error.to_string(),
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thorium::models::YaraRuleHit;

    /// Build an allowances map for tests
    ///
    /// # Arguments
    ///
    /// * `groups` - The groups and their (entities, associations, tags) toggles
    fn allowances(groups: &[(&str, bool, bool, bool)]) -> HashMap<String, GroupAllowances> {
        groups
            .iter()
            .map(|(name, entities, associations, tags)| {
                (
                    (*name).to_owned(),
                    GroupAllowances {
                        entities: *entities,
                        associations: *associations,
                        tags: *tags,
                    },
                )
            })
            .collect()
    }

    /// The write groups formula must intersect rule, event, and policy groups
    #[test]
    fn intersection_matrix() {
        // the allowances for our test groups; d disallows entities
        let allowed = allowances(&[
            ("a", true, true, true),
            ("b", true, true, true),
            ("c", true, true, true),
            ("d", false, true, true),
        ]);
        let rule_groups = vec!["a".to_owned(), "b".to_owned(), "d".to_owned()];
        // an event visible to {a} intersects to {a}
        let groups = write_groups(&rule_groups, &["a".to_owned()], &allowed);
        assert_eq!(groups, vec!["a".to_owned()]);
        // an event visible to {b, c} intersects to {b} since c lacks the rule
        let groups = write_groups(&rule_groups, &["b".to_owned(), "c".to_owned()], &allowed);
        assert_eq!(groups, vec!["b".to_owned()]);
        // an event visible to {c} intersects to nothing at all
        let groups = write_groups(&rule_groups, &["c".to_owned()], &allowed);
        assert!(groups.is_empty());
        // d carries the rule but disallows entities so it is suppressed
        let groups = write_groups(&rule_groups, &["d".to_owned()], &allowed);
        assert!(groups.is_empty());
        // an unknown group is treated as disallowing to stay leak safe
        let groups = write_groups(&rule_groups, &["z".to_owned()], &allowed);
        assert!(groups.is_empty());
    }

    /// Skip markers are filtered by tag allowability alone
    #[test]
    fn skip_marker_uses_tags_only_allowability() {
        // d disallows entities but allows tags; e disallows tags
        let allowed = allowances(&[("d", false, true, true), ("e", true, true, false)]);
        // d must still get its skip marker
        let groups = skip_groups(&["d".to_owned(), "e".to_owned()], &allowed);
        assert_eq!(groups, vec!["d".to_owned()]);
    }

    /// Every sample tag the writer builds must stamp a saturating depth
    #[test]
    fn tag_depth_saturating_add() {
        // a normal depth stamps depth plus one
        let tag = stamped_tag(
            vec!["a".to_owned()],
            2,
            YaraRuleHit::HIT_TAG,
            "rule".to_owned(),
        );
        assert_eq!(tag.trigger_depth, 3);
        // the max depth must saturate instead of wrapping back to zero
        let tag = stamped_tag(
            vec!["a".to_owned()],
            u8::MAX,
            SKIP_TAG,
            "oversize:1024".to_owned(),
        );
        assert_eq!(tag.trigger_depth, u8::MAX);
    }
}
