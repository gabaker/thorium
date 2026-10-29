//! The rule hit entities in Thorium
//!
//! A hit is a fact ("rule R matched sample S") rather than an occurrence so
//! its identity is derived from that fact: every writer, on every retry, on
//! every replica computes the same entity id and duplicate deliveries converge
//! onto one entity instead of multiplying.

use chrono::prelude::*;
use uuid::Uuid;

/// A single matched string within a yara rule hit
#[derive(Debug, Clone, Serialize, Deserialize, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct MatchedString {
    /// The identifier of the string that matched (e.g. `$marker`)
    pub id: String,
    /// The total number of times this string matched
    pub count: u64,
    /// The first offsets this string matched at (capped)
    pub first_offsets: Vec<u64>,
}

/// A single rule that matched within a yara source document
#[derive(Debug, Clone, Serialize, Deserialize, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct MatchedRule {
    /// The identifier of the rule that matched within its source document
    pub rule_identifier: String,
    /// The strings that matched for this rule (capped)
    pub strings: Vec<MatchedString>,
}

/// A yara rule hit against a sample
///
/// The hit records the exact rule text that fired (`rule_hash`), the ruleset
/// build it fired in (`ruleset_hash`), and the engine that evaluated it so a
/// hit stays interpretable after the rule is edited or the engine upgrades.
#[derive(Debug, Clone, Serialize, Deserialize, Hash)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct YaraRuleHit {
    /// The entity id of the yara rule that produced this hit
    pub rule_id: Uuid,
    /// The sha256 of the sample this hit is for
    pub sha256: String,
    /// The name of the yara rule entity at scan time
    pub rule_name: String,
    /// The content hash of the rule source that fired
    pub rule_hash: String,
    /// The hash of the compiled ruleset build this hit came from
    pub ruleset_hash: String,
    /// The yara engine version that evaluated this rule
    pub engine_version: String,
    /// When this hit was produced
    pub scanned_at: DateTime<Utc>,
    /// The rules within the source document that matched (capped)
    pub matched: Vec<MatchedRule>,
}

impl YaraRuleHit {
    /// The fixed namespace for deriving deterministic yara hit entity ids
    ///
    /// This must never change: every hit id ever derived depends on it.
    pub const ID_NAMESPACE: Uuid = Uuid::from_bytes(*b"thorium_yarahit!");

    /// The sample tag key every yara hit writer stamps on hit samples
    ///
    /// The UI renders this legacy tag key as a danger tag so it is the single
    /// source of truth for every writer and test that touches hit tags.
    pub const HIT_TAG: &str = "YaraRuleHits";

    /// The intrinsic entity tag key hits mirror their rules name under
    pub const RULE_NAME_TAG: &str = "YaraHitRuleName";

    /// The intrinsic entity tag key hits mirror the fired rule texts hash under
    ///
    /// A newer wins converge refresh that changes `rule_hash` must sweep the
    /// stale value rows for this key since the intrinsic refresh is add only.
    pub const RULE_HASH_TAG: &str = "YaraHitRuleHash";

    /// The max number of matched rules recorded per hit
    pub const MAX_MATCHED_RULES: usize = 64;

    /// The max number of matched strings recorded per rule
    pub const MAX_MATCHED_STRINGS: usize = 64;

    /// The max number of first offsets recorded per string
    pub const MAX_FIRST_OFFSETS: usize = 10;

    /// Derive the deterministic entity id for a hit
    ///
    /// The id is a UUIDv5 over `(rule_id, sha256)` so any writer computes the
    /// same id for the same logical hit and duplicate deliveries converge on
    /// one entity. The identity deliberately excludes `rule_hash` so rescans
    /// after a rule edit land on the same hit instead of duplicating it per
    /// rule version.
    ///
    /// # Arguments
    ///
    /// * `rule_id` - The entity id of the yara rule that produced this hit
    /// * `sha256` - The sha256 of the sample this hit is for
    pub fn deterministic_id(rule_id: &Uuid, sha256: &str) -> Uuid {
        // concat the rule id and sha256 into one name buffer
        let mut name = Vec::with_capacity(16 + sha256.len());
        // add the rule ids raw bytes
        name.extend_from_slice(rule_id.as_bytes());
        // add the sha256s bytes
        name.extend_from_slice(sha256.as_bytes());
        // derive the deterministic id for this hit
        Uuid::new_v5(&Self::ID_NAMESPACE, &name)
    }

    /// Bound this hits match metadata to its documented caps
    ///
    /// Hit metadata is stored inline in the entities tables `kind_data` column
    /// so it must stay small no matter how pathological the match is; counts
    /// stay exact while offsets/strings/rules are truncated.
    pub fn bound(mut self) -> Self {
        // cap the number of matched rules we record
        self.matched.truncate(Self::MAX_MATCHED_RULES);
        // step over our matched rules and cap their strings
        for matched in &mut self.matched {
            // cap the number of matched strings we record
            matched.strings.truncate(Self::MAX_MATCHED_STRINGS);
            // step over our matched strings and cap their offsets
            for string in &mut matched.strings {
                // cap the number of first offsets we record
                string.first_offsets.truncate(Self::MAX_FIRST_OFFSETS);
            }
        }
        self
    }

    /// Create a new [`YaraRuleHit`] with the info in the form
    ///
    /// # Errors
    ///
    /// * The rule id or sha256 identity fields are missing
    ///
    /// # Arguments
    ///
    /// * `form` - The metadata form
    #[cfg(feature = "api")]
    pub fn from_form(
        form: crate::models::entities::EntityMetadataForm,
    ) -> Result<Self, crate::utils::ApiError> {
        // hits must carry the rule id half of their identity
        let Some(rule_id) = form.yara_hit_rule_id else {
            return crate::bad!("Yara rule hits must have a rule id!".to_owned());
        };
        // hits must carry the sha256 half of their identity
        let Some(sha256) = form.yara_hit_sha256 else {
            return crate::bad!("Yara rule hits must have a sha256!".to_owned());
        };
        // an empty sha256 is a missing identity too
        if sha256.is_empty() {
            return crate::bad!("Yara rule hits cannot have an empty sha256!".to_owned());
        }
        // a nil rule id is a missing identity too
        if rule_id.is_nil() {
            return crate::bad!("Yara rule hits cannot have a nil rule id!".to_owned());
        }
        // build our yara rule hit bounded to its caps
        let hit = YaraRuleHit {
            rule_id,
            sha256,
            rule_name: form.yara_hit_rule_name.unwrap_or_default(),
            rule_hash: form.yara_hit_rule_hash.unwrap_or_default(),
            ruleset_hash: form.yara_hit_ruleset_hash.unwrap_or_default(),
            engine_version: form.yara_hit_engine_version.unwrap_or_default(),
            scanned_at: form.yara_hit_scanned_at.unwrap_or_else(Utc::now),
            matched: form.yara_hit_matched,
        };
        Ok(hit.bound())
    }

    /// Add this [`YaraRuleHit`]'s metadata to a form
    ///
    /// # Arguments
    ///
    /// * `form` - The form to add too
    #[cfg(feature = "client")]
    pub fn add_to_form(
        self,
        form: reqwest::multipart::Form,
    ) -> Result<reqwest::multipart::Form, crate::Error> {
        // always set our entity kind along with our scalar fields
        let mut form = form
            .text(
                "kind",
                crate::models::EntityKinds::YaraRuleHit.as_str().to_owned(),
            )
            .text("metadata[yara_hit_rule_id]", self.rule_id.to_string())
            .text("metadata[yara_hit_sha256]", self.sha256)
            .text("metadata[yara_hit_rule_name]", self.rule_name)
            .text("metadata[yara_hit_rule_hash]", self.rule_hash)
            .text("metadata[yara_hit_ruleset_hash]", self.ruleset_hash)
            .text("metadata[yara_hit_engine_version]", self.engine_version)
            .text(
                "metadata[yara_hit_scanned_at]",
                self.scanned_at.to_rfc3339(),
            );
        // add the matched rule info for this hit
        crate::multipart_list_serialize!(form, "metadata[yara_hit_matched][]", self.matched);
        Ok(form)
    }
}

/// A request to write one yara rule hit through the server side hit route
///
/// The request carries only the scan time facts the scanner alone knows: the
/// hit identity, the rule text/build/engine that actually fired, the groups
/// the scan event made the sample visible too, and the events trigger depth.
/// Authority facts (the rules name, groups, score, and actions plus group
/// allowances and the samples groups) are recomputed server side from fresh
/// database state so a stale scanner cache can never widen a hit.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct YaraHitWriteRequest {
    /// The entity id of the yara rule that produced this hit
    pub rule_id: Uuid,
    /// The sha256 of the sample this hit is for
    pub sha256: String,
    /// The content hash of the rule source that fired
    pub rule_hash: String,
    /// The hash of the compiled ruleset build this hit came from
    pub ruleset_hash: String,
    /// The yara engine version that evaluated this rule
    pub engine_version: String,
    /// When this hit was produced
    pub scanned_at: DateTime<Utc>,
    /// The groups the scan event made the sample visible too
    pub event_groups: Vec<String>,
    /// The depth of the scan event this hit is for
    ///
    /// This field is deliberately required with no serde default: a defaulted
    /// depth of 0 would reset trigger lineage and reopen the infinite trigger
    /// loop that depth propagation exists to break.
    pub event_depth: u8,
    /// The rules within the source document that matched (capped)
    pub matched: Vec<MatchedRule>,
    /// The hunt this scan ran for, if any
    ///
    /// When set the route probes the hunt and decorates the written hit with
    /// a `HuntHit` association and a `HuntId` entity tag; a hunt deleted mid
    /// scan skips decoration (reported via `hunt_missing`) instead of failing
    /// the write. Defaulted so requests from older scan workers that predate
    /// hunt decoration keep deserializing.
    #[serde(default)]
    pub hunt: Option<Uuid>,
}

impl YaraHitWriteRequest {
    /// Bound this requests match metadata to the documented hit caps
    ///
    /// The server re-bounds metadata on write regardless; bounding client side
    /// too keeps pathological matches from shipping oversized request bodies.
    pub fn bound(mut self) -> Self {
        // cap the number of matched rules we send
        self.matched.truncate(YaraRuleHit::MAX_MATCHED_RULES);
        // step over our matched rules and cap their strings
        for matched in &mut self.matched {
            // cap the number of matched strings we send
            matched.strings.truncate(YaraRuleHit::MAX_MATCHED_STRINGS);
            // step over our matched strings and cap their offsets
            for string in &mut matched.strings {
                // cap the number of first offsets we send
                string
                    .first_offsets
                    .truncate(YaraRuleHit::MAX_FIRST_OFFSETS);
            }
        }
        self
    }
}

/// The outcome of one server side yara hit write
///
/// Policy suppression is normal operation so every outcome short of an error
/// is a 200 carrying the per cause split of the requested event groups; the
/// scanners stats stay truthful by reading this split instead of guessing
/// from its own possibly stale caches.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct YaraHitWriteResponse {
    /// The deterministic id for this hits identity
    pub id: Uuid,
    /// The name of the hit entity if one was written or merged
    pub name: Option<String>,
    /// The groups this call wrote or merged the hit into
    pub written: Vec<String>,
    /// The event groups suppressed because the rule is not in them
    pub suppressed_rule: Vec<String>,
    /// The event groups suppressed because the sample is not in them
    pub suppressed_sample: Vec<String>,
    /// The event groups suppressed by kill switches or group deletion
    pub suppressed_policy: Vec<String>,
    /// Whether the sample no longer exists so the write was skipped
    pub sample_missing: bool,
    /// Whether a supplied hunt no longer existed when decoration was attempted
    ///
    /// This is only ever true when this call wrote the hit somewhere;
    /// suppressed or sample missing outcomes never attempt decoration.
    /// Defaulted so responses from older APIs that predate hunt decoration
    /// keep deserializing.
    #[serde(default)]
    pub hunt_missing: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The deterministic id must stay pinned since every stored hit uses it
    #[test]
    fn deterministic_id_stable_vector() {
        // build a fixed rule id
        let rule_id = Uuid::parse_str("11111111-2222-3333-4444-555555555555")
            .expect("failed to parse fixed rule id");
        // derive the hit id for a fixed sample
        let id = YaraRuleHit::deterministic_id(&rule_id, &"a".repeat(64));
        // the same inputs must always derive the same id
        let again = YaraRuleHit::deterministic_id(&rule_id, &"a".repeat(64));
        assert_eq!(id, again);
        // and the id must be a v5 uuid
        assert_eq!(id.get_version_num(), 5);
        // pin the derived id so the namespace/derivation can never drift
        assert_eq!(id.to_string(), "0098f885-ee2c-5918-ad20-22ce22559987");
    }

    /// Different identities must derive different ids
    #[test]
    fn deterministic_id_differs_by_identity() {
        // build two different rule ids
        let rule_a = Uuid::parse_str("11111111-2222-3333-4444-555555555555").unwrap();
        let rule_b = Uuid::parse_str("66666666-7777-8888-9999-aaaaaaaaaaaa").unwrap();
        // the same sample under different rules must derive different ids
        assert_ne!(
            YaraRuleHit::deterministic_id(&rule_a, &"a".repeat(64)),
            YaraRuleHit::deterministic_id(&rule_b, &"a".repeat(64)),
        );
        // the same rule across different samples must derive different ids
        assert_ne!(
            YaraRuleHit::deterministic_id(&rule_a, &"a".repeat(64)),
            YaraRuleHit::deterministic_id(&rule_a, &"b".repeat(64)),
        );
    }

    /// Hit metadata must truncate to its documented caps
    #[test]
    fn hit_metadata_truncates_offsets() {
        // build a hit with far too many offsets, strings, and rules
        let string = MatchedString {
            id: "$marker".to_owned(),
            count: 1_000_000,
            first_offsets: (0..1000).collect(),
        };
        let matched = MatchedRule {
            rule_identifier: "huge".to_owned(),
            strings: vec![string; 1000],
        };
        let hit = YaraRuleHit {
            rule_id: Uuid::parse_str("11111111-2222-3333-4444-555555555555").unwrap(),
            sha256: "a".repeat(64),
            rule_name: "huge".to_owned(),
            rule_hash: "b".repeat(64),
            ruleset_hash: "c".repeat(64),
            engine_version: "yara-x 1.21.0".to_owned(),
            scanned_at: Utc::now(),
            matched: vec![matched; 1000],
        };
        // bound this hit to its caps
        let bounded = hit.bound();
        // every cap must have been applied while counts stay exact
        assert_eq!(bounded.matched.len(), YaraRuleHit::MAX_MATCHED_RULES);
        assert_eq!(
            bounded.matched[0].strings.len(),
            YaraRuleHit::MAX_MATCHED_STRINGS
        );
        assert_eq!(
            bounded.matched[0].strings[0].first_offsets.len(),
            YaraRuleHit::MAX_FIRST_OFFSETS
        );
        assert_eq!(bounded.matched[0].strings[0].count, 1_000_000);
    }

    /// The hit tag key must stay pinned since the UI renders it by name
    #[test]
    fn hit_tag_stable_value() {
        // the UI danger tag rendering keys on this exact legacy value
        assert_eq!(YaraRuleHit::HIT_TAG, "YaraRuleHits");
    }

    /// The intrinsic tag keys must stay pinned since stored rows carry them
    #[test]
    fn intrinsic_tag_keys_stable() {
        // every stored hit already carries rows under these exact keys
        assert_eq!(YaraRuleHit::RULE_NAME_TAG, "YaraHitRuleName");
        assert_eq!(YaraRuleHit::RULE_HASH_TAG, "YaraHitRuleHash");
    }

    /// Build a write request with oversized match metadata for tests
    fn oversized_request() -> YaraHitWriteRequest {
        // build a pathological matched string
        let string = MatchedString {
            id: "$marker".to_owned(),
            count: 42,
            first_offsets: (0..1000).collect(),
        };
        // build a pathological matched rule
        let matched = MatchedRule {
            rule_identifier: "huge".to_owned(),
            strings: vec![string; 1000],
        };
        YaraHitWriteRequest {
            rule_id: Uuid::parse_str("11111111-2222-3333-4444-555555555555").unwrap(),
            sha256: "a".repeat(64),
            rule_hash: "b".repeat(64),
            ruleset_hash: "c".repeat(64),
            engine_version: "yara-x 1.21.0".to_owned(),
            scanned_at: Utc::now(),
            event_groups: vec!["corn".to_owned()],
            event_depth: 3,
            matched: vec![matched; 1000],
            hunt: None,
        }
    }

    /// Write requests must truncate their match metadata to the hit caps
    #[test]
    fn write_request_bounds_metadata() {
        // bound a pathological write request
        let bounded = oversized_request().bound();
        // every cap must have been applied while counts stay exact
        assert_eq!(bounded.matched.len(), YaraRuleHit::MAX_MATCHED_RULES);
        assert_eq!(
            bounded.matched[0].strings.len(),
            YaraRuleHit::MAX_MATCHED_STRINGS
        );
        assert_eq!(
            bounded.matched[0].strings[0].first_offsets.len(),
            YaraRuleHit::MAX_FIRST_OFFSETS
        );
        assert_eq!(bounded.matched[0].strings[0].count, 42);
    }

    /// Write requests must round trip through serde with their depth intact
    #[test]
    fn write_request_serde_round_trip() {
        // bound a pathological request down to something serializable
        let mut req = oversized_request().bound();
        // decorate this request with a hunt id so it round trips too
        req.hunt = Some(Uuid::parse_str("66666666-7777-8888-9999-aaaaaaaaaaaa").unwrap());
        // serialize and deserialize this request
        let raw = serde_json::to_string(&req).expect("failed to serialize request");
        let back: YaraHitWriteRequest =
            serde_json::from_str(&raw).expect("failed to deserialize request");
        // the identity, depth, and hunt must survive the round trip exactly
        assert_eq!(back.rule_id, req.rule_id);
        assert_eq!(back.sha256, req.sha256);
        assert_eq!(back.event_depth, 3);
        assert_eq!(back.event_groups, req.event_groups);
        assert_eq!(back.hunt, req.hunt);
    }

    /// A request missing its hunt field must deserialize to an undecorated one
    #[test]
    fn write_request_hunt_is_optional() {
        // serialize a request and strip its hunt field
        let req = oversized_request().bound();
        let mut raw = serde_json::to_value(&req).expect("failed to serialize request");
        raw.as_object_mut()
            .expect("request must serialize to an object")
            .remove("hunt");
        // a huntless request must deserialize since older scan workers that
        // predate hunt decoration never send the field
        let back = serde_json::from_value::<YaraHitWriteRequest>(raw)
            .expect("a missing hunt field must default");
        assert_eq!(back.hunt, None);
    }

    /// A response missing its hunt flag must deserialize to a decorated one
    #[test]
    fn response_hunt_missing_defaults() {
        // build the response shape an older API without hunt decoration sends
        let raw = serde_json::json!({
            "id": "11111111-2222-3333-4444-555555555555",
            "name": "corn-rule",
            "written": ["corn"],
            "suppressed_rule": [],
            "suppressed_sample": [],
            "suppressed_policy": [],
            "sample_missing": false,
        });
        // newer workers must keep deserializing old API responses mid roll
        let back = serde_json::from_value::<YaraHitWriteResponse>(raw)
            .expect("a missing hunt flag must default");
        assert!(!back.hunt_missing, "a missing hunt flag must default false");
    }

    /// A request missing its event depth must fail to deserialize
    #[test]
    fn write_request_depth_is_required() {
        // serialize a request and strip its depth field
        let req = oversized_request().bound();
        let mut raw = serde_json::to_value(&req).expect("failed to serialize request");
        raw.as_object_mut()
            .expect("request must serialize to an object")
            .remove("event_depth");
        // a depthless request must be rejected instead of defaulting to 0
        // since a defaulted depth would reset the trigger loop breaker
        let back = serde_json::from_value::<YaraHitWriteRequest>(raw);
        assert!(back.is_err(), "a missing event depth must not default");
    }
}
