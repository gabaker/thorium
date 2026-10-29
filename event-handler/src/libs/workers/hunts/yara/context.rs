//! The compiled yara ruleset context shared by all yara scanner workers
//!
//! Every yara rule entity compiles into one global ruleset with one namespace
//! per rule (named by the rule's entity id) so hits map back to their rule by
//! construction. Rules that fail to compile are quarantined — never unwrapped —
//! so one bad stored rule can never crash the daemon.
//!
//! Builds are classified, hashed, then compiled: the ruleset hash is computed
//! before the expensive combined build so a serialized ruleset cached by a peer
//! under the same (engine version, ruleset hash) can be deserialized instead of
//! recompiled. Every cache failure falls back to a full compile, never fatally.

use data_encoding::HEXLOWER;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use thorium::models::{Entity, EntityKinds, EntityListOpts, EntityMetadata, TagMap, YaraRule};
use thorium::{Error, Thorium};
use tracing::{Level, event, instrument};
use uuid::Uuid;

/// The engine version recorded on every hit this build produces
///
/// This must match the exact pinned yara-x version in the workspace manifest.
pub const ENGINE_VERSION: &str = "yara-x 1.21.0";

/// The quarantine status tag key for yara rules
///
/// Aliased from the shared model constant so the scanner, the hunt snapshot
/// filter, and every client agree on the exact key.
pub const STATUS_TAG: &str = YaraRule::STATUS_TAG;

/// The engine version path segment used to key the serialized ruleset cache
///
/// Ruleset cache keys ride in route path segments that only allow
/// `[A-Za-z0-9._-]` so the human readable engine version has its spaces
/// flattened to dashes.
pub fn cache_engine_version() -> String {
    // flatten the engine version onto the allowed path segment charset
    ENGINE_VERSION.replace(' ', "-")
}

/// The scan time metadata a worker needs about one compiled rule
#[derive(Debug, Clone)]
pub struct RuleMeta {
    /// The name of this rules entity
    pub name: String,
    /// The groups this rule is in
    pub groups: Vec<String>,
    /// The content hash of this rules source
    pub rule_hash: String,
    /// Whether this rule is exempt from noisy/slow quarantine
    pub quarantine_exempt: bool,
}

/// A rule that failed to compile and was quarantined from the ruleset
#[derive(Debug, Clone)]
pub struct QuarantinedRule {
    /// The entity id of the quarantined rule
    pub id: Uuid,
    /// The name of the quarantined rules entity
    pub name: String,
    /// The user that submitted the quarantined rule
    pub submitter: String,
    /// The groups the quarantined rule is in
    pub groups: Vec<String>,
    /// The compile error that quarantined this rule
    pub error: String,
    /// Whether this rule already carried a broken status tag when it was
    /// listed, meaning a peer or a prior run already tagged and notified it
    pub already_tagged: bool,
}

/// The compiled ruleset and its side metadata for yara scanning
#[derive(Clone)]
pub struct YaraRuleContext {
    /// The compiled ruleset with one namespace per rule entity
    ///
    /// `yara_x::Rules` is not `Clone` and can be hundreds of MB so it is
    /// shared behind an `Arc`; broadcasts and `spawn_blocking` scan tasks
    /// clone the `Arc`, never the ruleset.
    pub rules: Arc<yara_x::Rules>,
    /// The scan time metadata for each compiled rule keyed by entity id
    pub meta: HashMap<Uuid, RuleMeta>,
    /// The hash identifying this exact ruleset build
    pub ruleset_hash: String,
    /// The rules that failed to compile and were quarantined
    pub quarantined: Vec<QuarantinedRule>,
}

impl YaraRuleContext {
    /// Build a [`YaraRuleContext`] from info in the Thorium API
    ///
    /// The rules are listed and classified, then the serialized ruleset cache
    /// is checked under this builds hash; only a cache miss pays for the
    /// combined compile, whose output is uploaded back best effort.
    ///
    /// # Arguments
    ///
    /// * `thorium` - A Thorium client
    #[instrument(name = "YaraRuleContext::new", skip_all, err(Debug))]
    pub async fn new(thorium: &Thorium) -> Result<Self, Error> {
        // build the options for listing yara rule entities
        let opts = EntityListOpts::default()
            .kind(EntityKinds::YaraRule)
            // use a large page size to reduce round trips
            .page_size(2000);
        // get a cursor over all yara rule entities
        let mut cursor = thorium.entities.list_details(&opts).await?;
        // gather the sources to compile
        let mut sources = Vec::with_capacity(cursor.data.len());
        // crawl all of the yara rules in Thorium
        loop {
            for entity in cursor.data.drain(..) {
                // skip any rules already quarantined as noisy or slow
                if should_skip_quarantined(&entity) {
                    // log that we are skipping a quarantined rule
                    event!(
                        Level::INFO,
                        msg = "Skipping quarantined yara rule",
                        rule = entity.name,
                        id = entity.id.to_string(),
                    );
                    continue;
                }
                // pull the yara rule metadata out of this entity
                let EntityMetadata::YaraRule(rule) = &entity.metadata else {
                    // log that we somehow listed a non yara rule entity
                    event!(
                        Level::ERROR,
                        msg = "Can't compile a non yara rule entity",
                        kind = entity.kind.as_str(),
                    );
                    continue;
                };
                // remember whether this rule already carries a broken tag so a
                // re-quarantine never re-tags or re-notifies it
                let broken_tagged = has_status_tag(&entity.tags, Some(YaraRule::STATUS_BROKEN));
                // add this rules source and info to our compile list
                sources.push(CompilableRule {
                    id: entity.id,
                    name: entity.name.clone(),
                    submitter: entity.submitter.clone(),
                    groups: entity.groups.clone(),
                    source: rule.rule.clone(),
                    rule_hash: rule.rule_hash(),
                    broken_tagged,
                    quarantine_exempt: rule.quarantine_exempt,
                });
            }
            // if this cursor is exhausted then stop listing yara rules
            if cursor.exhausted() {
                break;
            }
            // this cursor has more data so get the next page
            cursor.refill().await?;
        }
        // classification solo compiles every source so keep it off the async runtime
        let (good, quarantined) = tokio::task::spawn_blocking(move || Self::classify(sources))
            .await
            .map_err(|err| Error::new(format!("yara classify task failed: {err}")))?;
        // hash the surviving rules before any expensive combined build
        let ruleset_hash = Self::hash(&good);
        // try to skip the combined compile with the serialized ruleset cache
        if let Some(context) =
            Self::from_cache(thorium, ruleset_hash.clone(), &good, &quarantined).await
        {
            return Ok(context);
        }
        // compiling can take seconds at scale so keep it off the async runtime
        let context =
            tokio::task::spawn_blocking(move || Self::compile(good, ruleset_hash, quarantined))
                .await
                .map_err(|err| Error::new(format!("yara compile task failed: {err}")))?;
        // share this fresh build with our peers best effort
        context.upload_cache(thorium).await;
        Ok(context)
    }

    /// Build a [`YaraRuleContext`] for a hunt's frozen rule subset
    ///
    /// Subset builds reuse the live pipeline - classify, hash, serialized
    /// cache check, combined compile - but are GET-only against the serialized
    /// ruleset cache: the cache uploader best-effort deletes same engine
    /// version siblings so a subset upload would evict the live global
    /// ruleset's cache object and other hunts' builds. A subset whose frozen
    /// rules happen to equal the live set even hits the live cache object for
    /// free since the hash is content addressed.
    ///
    /// # Arguments
    ///
    /// * `thorium` - A Thorium client
    /// * `sources` - The subset's rules to compile
    #[instrument(name = "YaraRuleContext::for_subset", skip_all, err(Debug))]
    pub async fn for_subset(
        thorium: &Thorium,
        sources: Vec<CompilableRule>,
    ) -> Result<Self, Error> {
        // classification solo compiles every source so keep it off the async runtime
        let (good, quarantined) = tokio::task::spawn_blocking(move || Self::classify(sources))
            .await
            .map_err(|err| Error::new(format!("yara classify task failed: {err}")))?;
        // hash the surviving rules before any expensive combined build
        let ruleset_hash = Self::hash(&good);
        // try to skip the combined compile with the serialized ruleset cache
        if let Some(context) =
            Self::from_cache(thorium, ruleset_hash.clone(), &good, &quarantined).await
        {
            return Ok(context);
        }
        // compiling can take seconds at scale so keep it off the async runtime
        let context =
            tokio::task::spawn_blocking(move || Self::compile(good, ruleset_hash, quarantined))
                .await
                .map_err(|err| Error::new(format!("yara compile task failed: {err}")))?;
        // deliberately no cache upload: subset builds stay GET-only
        Ok(context)
    }

    /// Classify sources into those that compile solo and those to quarantine
    ///
    /// Each source compiles alone to isolate its errors so one bad source can
    /// never poison the shared compiler state; sources that fail are
    /// quarantined, never unwrapped.
    ///
    /// # Arguments
    ///
    /// * `sources` - The rules to classify
    pub fn classify(sources: Vec<CompilableRule>) -> (Vec<CompilableRule>, Vec<QuarantinedRule>) {
        // find which sources compile cleanly and quarantine the rest
        let mut quarantined = Vec::default();
        let mut good = Vec::with_capacity(sources.len());
        for rule in sources {
            // compile this source alone to isolate its errors
            let mut solo = yara_x::Compiler::new();
            // stored rules can never include other files
            solo.enable_includes(false);
            // check whether this source compiles on its own
            match solo.add_source(rule.source.as_str()) {
                Ok(_) => good.push(rule),
                Err(error) => {
                    // log that we are quarantining a rule that failed to compile
                    event!(
                        Level::ERROR,
                        msg = "Quarantining yara rule that failed to compile",
                        rule = rule.name,
                        id = rule.id.to_string(),
                        error = error.to_string(),
                    );
                    // quarantine this rule instead of crashing or skipping silently
                    quarantined.push(QuarantinedRule {
                        id: rule.id,
                        name: rule.name,
                        submitter: rule.submitter,
                        groups: rule.groups.clone(),
                        error: error.to_string(),
                        already_tagged: rule.broken_tagged,
                    });
                }
            }
        }
        (good, quarantined)
    }

    /// Hash the identity of a classified set of rules
    ///
    /// The sorted (id, rule hash) pairs are hashed so the same rules always
    /// hash the same regardless of listing order. This runs before the
    /// combined build so the hash can key the serialized ruleset cache.
    ///
    /// # Arguments
    ///
    /// * `good` - The classified rules that identify this build
    pub fn hash(good: &[CompilableRule]) -> String {
        // gather the (id, rule hash) pairs that identify this build
        let mut identity = good
            .iter()
            .map(|rule| (rule.id, rule.rule_hash.as_str()))
            .collect::<Vec<(Uuid, &str)>>();
        // hash the sorted identity pairs so the same rules always hash the same
        identity.sort();
        let mut hasher = Sha256::new();
        for (id, rule_hash) in &identity {
            // add this rules identity to our build hash
            hasher.update(id.as_bytes());
            hasher.update(rule_hash.as_bytes());
        }
        HEXLOWER.encode(&hasher.finalize())
    }

    /// Compile classified rules into a [`YaraRuleContext`]
    ///
    /// Each source compiles into its own namespace named by its entity id; a
    /// source that passed classification but fails the combined pass is still
    /// quarantined rather than crashing the daemon.
    ///
    /// # Arguments
    ///
    /// * `good` - The classified rules to compile
    /// * `ruleset_hash` - The hash identifying this exact build
    /// * `quarantined` - The rules classification already quarantined
    pub fn compile(
        good: Vec<CompilableRule>,
        ruleset_hash: String,
        mut quarantined: Vec<QuarantinedRule>,
    ) -> Self {
        // build the combined compiler over only the good sources
        let mut compiler = yara_x::Compiler::new();
        // stored rules can never include other files
        compiler.enable_includes(false);
        // track the scan time metadata for each compiled rule
        let mut meta = HashMap::with_capacity(good.len());
        for rule in good {
            // every rule compiles into its own namespace named by its entity id
            compiler.new_namespace(&rule.id.to_string());
            // this source already compiled solo so a failure here is a bug;
            // quarantine it anyway rather then crashing the daemon
            if let Err(error) = compiler.add_source(rule.source.as_str()) {
                // log that a previously good source failed the combined pass
                event!(
                    Level::ERROR,
                    msg = "Quarantining yara rule that failed the combined compile",
                    rule = rule.name,
                    id = rule.id.to_string(),
                    error = error.to_string(),
                );
                // quarantine this rule
                quarantined.push(QuarantinedRule {
                    id: rule.id,
                    name: rule.name,
                    submitter: rule.submitter,
                    groups: rule.groups.clone(),
                    error: error.to_string(),
                    already_tagged: rule.broken_tagged,
                });
                continue;
            }
            // track this rules scan time metadata
            meta.insert(
                rule.id,
                RuleMeta {
                    name: rule.name,
                    groups: rule.groups,
                    rule_hash: rule.rule_hash,
                    quarantine_exempt: rule.quarantine_exempt,
                },
            );
        }
        // build the final compiled ruleset
        let rules = compiler.build();
        YaraRuleContext {
            rules: Arc::new(rules),
            meta,
            ruleset_hash,
            quarantined,
        }
    }

    /// Build a [`YaraRuleContext`] from already gathered rule info
    ///
    /// This is a pure function so unit tests can drive the compiler without a
    /// live API: it classifies the sources, hashes the survivors, then runs
    /// the combined compile. It is also the full compile fallback whenever the
    /// serialized ruleset cache cannot serve a build.
    ///
    /// # Arguments
    ///
    /// * `sources` - The rules to compile
    // only tests build contexts without the serialized ruleset cache today
    #[allow(dead_code)]
    pub fn from_rules(sources: Vec<CompilableRule>) -> Self {
        // find which sources compile cleanly and quarantine the rest
        let (good, quarantined) = Self::classify(sources);
        // hash the surviving rules before the combined build
        let ruleset_hash = Self::hash(&good);
        // compile the surviving rules into a context
        Self::compile(good, ruleset_hash, quarantined)
    }

    /// Build the scan time metadata map for a set of classified rules
    ///
    /// # Arguments
    ///
    /// * `good` - The classified rules to snapshot metadata for
    fn build_meta(good: &[CompilableRule]) -> HashMap<Uuid, RuleMeta> {
        // snapshot each rules scan time metadata
        good.iter()
            .map(|rule| {
                (
                    rule.id,
                    RuleMeta {
                        name: rule.name.clone(),
                        groups: rule.groups.clone(),
                        rule_hash: rule.rule_hash.clone(),
                        quarantine_exempt: rule.quarantine_exempt,
                    },
                )
            })
            .collect()
    }

    /// Try to build this context from the serialized ruleset cache
    ///
    /// Every failure here (missing route on an old API, S3 miss, deserialize
    /// error) is just a cache miss: the caller falls back to a full compile so
    /// a broken cache can never take scanning down. Quarantine info still
    /// comes from our own classification so notifications fire on cache hits.
    ///
    /// # Arguments
    ///
    /// * `thorium` - A Thorium client
    /// * `ruleset_hash` - The hash identifying this exact ruleset build
    /// * `good` - The classified rules this build covers
    /// * `quarantined` - The rules classification quarantined
    async fn from_cache(
        thorium: &Thorium,
        ruleset_hash: String,
        good: &[CompilableRule],
        quarantined: &[QuarantinedRule],
    ) -> Option<Self> {
        // ask the api for a serialized ruleset matching our exact build
        let bytes = match thorium
            .events
            .get_yara_ruleset(&cache_engine_version(), &ruleset_hash)
            .await
        {
            Ok(Some(bytes)) => bytes,
            // a clean miss means no peer has uploaded this build yet
            Ok(None) => return None,
            Err(error) => {
                // warn and fall back to a full compile
                event!(
                    Level::WARN,
                    msg = "Failed to get cached yara ruleset",
                    ruleset_hash = ruleset_hash,
                    error = error.to_string(),
                );
                return None;
            }
        };
        // deserializing large rulesets is cpu bound so keep it off the async runtime
        let task = tokio::task::spawn_blocking(move || yara_x::Rules::deserialize(bytes));
        // get the deserialized ruleset falling back to a compile on any failure
        let rules = match task.await {
            Ok(Ok(rules)) => rules,
            Ok(Err(error)) => {
                // warn and fall back to a full compile
                event!(
                    Level::WARN,
                    msg = "Failed to deserialize cached yara ruleset",
                    ruleset_hash = ruleset_hash,
                    error = error.to_string(),
                );
                return None;
            }
            Err(error) => {
                // warn and fall back to a full compile
                event!(
                    Level::WARN,
                    msg = "Yara ruleset deserialize task failed",
                    ruleset_hash = ruleset_hash,
                    error = error.to_string(),
                );
                return None;
            }
        };
        // log that this build was served from the serialized ruleset cache
        event!(
            Level::INFO,
            msg = "Loaded yara ruleset from the serialized cache",
            ruleset_hash = ruleset_hash,
            rules = good.len(),
        );
        // the metadata comes from our own classified rules, not the cache
        Some(YaraRuleContext {
            rules: Arc::new(rules),
            meta: Self::build_meta(good),
            ruleset_hash,
            quarantined: quarantined.to_vec(),
        })
    }

    /// Upload this contexts serialized ruleset to the cache best effort
    ///
    /// A failed upload only costs peers one compile so it is warned about and
    /// never fatal; an old API without the route 404s here harmlessly.
    ///
    /// # Arguments
    ///
    /// * `thorium` - A Thorium client
    async fn upload_cache(&self, thorium: &Thorium) {
        // serializing large rulesets is cpu bound so keep it off the async runtime
        let rules = self.rules.clone();
        let task = tokio::task::spawn_blocking(move || rules.serialize());
        // get the serialized ruleset giving up on any failure
        let serialized = match task.await {
            Ok(Ok(serialized)) => serialized,
            Ok(Err(error)) => {
                // warn that this build could not be serialized
                event!(
                    Level::WARN,
                    msg = "Failed to serialize yara ruleset for the cache",
                    ruleset_hash = self.ruleset_hash,
                    error = error.to_string(),
                );
                return;
            }
            Err(error) => {
                // warn that the serialize task itself failed
                event!(
                    Level::WARN,
                    msg = "Yara ruleset serialize task failed",
                    ruleset_hash = self.ruleset_hash,
                    error = error.to_string(),
                );
                return;
            }
        };
        // upload this serialized build so peers can skip their compiles
        if let Err(error) = thorium
            .events
            .upload_yara_ruleset(&cache_engine_version(), &self.ruleset_hash, serialized)
            .await
        {
            // warn that this upload failed; peers just compile for themselves
            event!(
                Level::WARN,
                msg = "Failed to upload serialized yara ruleset",
                ruleset_hash = self.ruleset_hash,
                error = error.to_string(),
            );
        }
    }
}

/// A rule gathered from the API that is ready to compile
#[derive(Debug, Clone)]
pub struct CompilableRule {
    /// The entity id for this rule
    pub id: Uuid,
    /// The name of this rules entity
    pub name: String,
    /// The user that submitted this rule
    pub submitter: String,
    /// The groups this rule is in
    pub groups: Vec<String>,
    /// The raw yara source for this rule
    pub source: String,
    /// The content hash of this rules source
    pub rule_hash: String,
    /// Whether this rules entity already carried a broken status tag
    pub broken_tagged: bool,
    /// Whether this rule is exempt from noisy/slow quarantine
    ///
    /// Exemption is not rule identity so it never participates in the
    /// ruleset hash; flipping it keeps serialized ruleset cache keys stable.
    pub quarantine_exempt: bool,
}

/// Check whether a tag map carries a yara rule status tag
///
/// # Arguments
///
/// * `tags` - The entity tag map to check
/// * `status` - The status value to look for; `None` matches any status value
pub fn has_status_tag(tags: &TagMap, status: Option<&str>) -> bool {
    // get the values under the status tag key if any exist
    match tags.get(STATUS_TAG) {
        Some(values) => match status {
            // look for one specific status value
            Some(status) => values.keys().any(|value| value == status),
            // any status value at all counts
            None => !values.is_empty(),
        },
        // this entity carries no status tags at all
        None => false,
    }
}

/// Check whether an entity carries a quarantine status tag
///
/// # Arguments
///
/// * `entity` - The entity to check for quarantine tags
fn is_status_quarantined(entity: &Entity) -> bool {
    // check if this entity has any noisy or slow status tags; use the shared
    // model constants so this predicate can never drift from the API's hunt
    // snapshot filter
    has_status_tag(&entity.tags, Some(YaraRule::STATUS_NOISY))
        || has_status_tag(&entity.tags, Some(YaraRule::STATUS_SLOW))
}

/// Check whether a listed rule entity should be skipped as quarantined
///
/// Exempt rules are never skipped, even when a stale status tag survived a
/// quarantine race (a worker tagging concurrently with the admin's
/// exemption) — exemption always wins over quarantine.
///
/// # Arguments
///
/// * `entity` - The listed rule entity to test
fn should_skip_quarantined(entity: &Entity) -> bool {
    // exemption always wins over a stale quarantine status tag
    if matches!(&entity.metadata, EntityMetadata::YaraRule(rule) if rule.quarantine_exempt) {
        return false;
    }
    // non exempt rules skip on any noisy or slow status tag
    is_status_quarantined(entity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Build a compilable rule around a source for tests
    ///
    /// # Arguments
    ///
    /// * `name` - The name for this test rule
    /// * `source` - The yara source for this test rule
    fn test_rule(name: &str, source: &str) -> CompilableRule {
        CompilableRule {
            id: Uuid::new_v4(),
            name: name.to_owned(),
            submitter: "mcarson".to_owned(),
            groups: vec!["corn".to_owned()],
            source: source.to_owned(),
            rule_hash: {
                // hash this rules source like the real model does
                let mut hasher = Sha256::new();
                hasher.update(source.as_bytes());
                HEXLOWER.encode(&hasher.finalize())
            },
            broken_tagged: false,
            quarantine_exempt: false,
        }
    }

    /// Good sources must compile into namespaces while bad ones quarantine
    #[test]
    fn context_from_rules_compiles_and_quarantines_bad_source() {
        // build two good rules and one broken rule
        let good_a = test_rule("good_a", "rule good_a { condition: true }");
        let good_b = test_rule(
            "good_b",
            "rule good_b { strings: $a = \"corn\" condition: $a }",
        );
        let broken = test_rule("broken", "rule broken {");
        let ids = (good_a.id, good_b.id, broken.id);
        // compile this rule set
        let context = YaraRuleContext::from_rules(vec![good_a, good_b, broken]);
        // both good rules must be in the compiled set and the metadata map
        assert_eq!(context.rules.iter().count(), 2);
        assert!(context.meta.contains_key(&ids.0));
        assert!(context.meta.contains_key(&ids.1));
        // the broken rule must be quarantined, not compiled
        assert_eq!(context.quarantined.len(), 1);
        assert_eq!(context.quarantined[0].id, ids.2);
        assert!(!context.meta.contains_key(&ids.2));
    }

    /// Build a listed yara rule entity for list time skip tests
    ///
    /// # Arguments
    ///
    /// * `statuses` - The `YaraRuleStatus` tag values to set on the entity
    /// * `exempt` - Whether this rule is exempt from quarantine
    fn listed_entity(statuses: &[&str], exempt: bool) -> Entity {
        // build the yara metadata with the requested exemption
        let mut rule = thorium::models::YaraRule::new(
            "rule a { condition: true }",
            thorium::models::YaraRuleAppliesTo::Samples,
        )
        .expect("failed to build yara rule");
        rule.quarantine_exempt = exempt;
        // build the status tag values for this rule
        let mut tags = TagMap::default();
        if !statuses.is_empty() {
            // the group set a value is visible in doesn't matter here
            let values = statuses
                .iter()
                .map(|status| ((*status).to_owned(), HashSet::new()))
                .collect();
            tags.insert(STATUS_TAG.to_owned(), values);
        }
        // build a bare listed entity carrying those tags
        Entity {
            id: Uuid::new_v4(),
            name: "test-rule".to_owned(),
            kind: EntityKinds::YaraRule,
            metadata: EntityMetadata::YaraRule(rule),
            description: None,
            submitter: "mcarson".to_owned(),
            groups: vec!["corn".to_owned()],
            tags,
            image: None,
            created: chrono::Utc::now(),
        }
    }

    /// The list time skip must never drop an exempt rule
    ///
    /// A stale status tag can survive a quarantine race so exemption must win
    /// over the tag or an exempt rule could be benched until manual cleanup.
    #[test]
    fn list_skip_exemption_wins_over_stale_tag() {
        // a quarantined rule without an exemption must be skipped
        assert!(should_skip_quarantined(&listed_entity(&["Noisy"], false)));
        // the same rule with an exemption must never be skipped
        assert!(!should_skip_quarantined(&listed_entity(&["Noisy"], true)));
        // untagged rules are never skipped either way
        assert!(!should_skip_quarantined(&listed_entity(&[], false)));
        assert!(!should_skip_quarantined(&listed_entity(&[], true)));
    }

    /// A rules quarantine exemption must ride into its scan time metadata
    ///
    /// `quarantine()` keys off `RuleMeta.quarantine_exempt` so the plumb from
    /// the gathered rule into the compiled contexts metadata map is what makes
    /// exempt rules unquarantinable.
    #[test]
    fn exemption_rides_into_rule_meta() {
        // build one exempt rule and one normal rule
        let mut exempt = test_rule("exempt", "rule exempt { condition: true }");
        exempt.quarantine_exempt = true;
        let normal = test_rule("normal", "rule normal { condition: false }");
        let (exempt_id, normal_id) = (exempt.id, normal.id);
        // compile both rules into a context
        let context = YaraRuleContext::from_rules(vec![exempt, normal]);
        // the exemption must ride into the metadata map untouched
        assert!(context.meta[&exempt_id].quarantine_exempt);
        assert!(!context.meta[&normal_id].quarantine_exempt);
    }

    /// A rules quarantine exemption must never change the ruleset hash
    ///
    /// Exemption is not rule identity: flipping it must keep serialized
    /// ruleset cache keys stable so an exemption never forces a recompile.
    #[test]
    fn exemption_keeps_ruleset_hash_stable() {
        // build the same rule with and without an exemption
        let plain = test_rule("a", "rule a { condition: true }");
        let mut exempt = plain.clone();
        exempt.quarantine_exempt = true;
        // the ruleset hash must be identical for both
        assert_eq!(
            YaraRuleContext::hash(&[plain]),
            YaraRuleContext::hash(&[exempt])
        );
    }

    /// The same rules must always hash to the same ruleset hash
    #[test]
    fn ruleset_hash_stable_across_order() {
        // build two rules
        let rule_a = test_rule("a", "rule a { condition: true }");
        let rule_b = test_rule("b", "rule b { condition: false }");
        // compile them in both orders
        let forward = YaraRuleContext::from_rules(vec![rule_a.clone(), rule_b.clone()]);
        let reverse = YaraRuleContext::from_rules(vec![rule_b, rule_a]);
        // the build hash must not depend on listing order
        assert_eq!(forward.ruleset_hash, reverse.ruleset_hash);
    }

    /// The classify then hash split must produce the exact hash the combined
    /// `from_rules` build pins, so cache keys match across both paths
    #[test]
    fn classify_hash_split_matches_from_rules() {
        // build two good rules and one broken rule
        let rule_a = test_rule("a", "rule a { condition: true }");
        let rule_b = test_rule("b", "rule b { condition: false }");
        let broken = test_rule("broken", "rule broken {");
        let sources = vec![rule_a, rule_b, broken];
        // run the split classify then hash path
        let (good, quarantined) = YaraRuleContext::classify(sources.clone());
        let split_hash = YaraRuleContext::hash(&good);
        // the broken rule must be classified out before hashing
        assert_eq!(good.len(), 2);
        assert_eq!(quarantined.len(), 1);
        // the split hash must match the full build path exactly
        let context = YaraRuleContext::from_rules(sources);
        assert_eq!(split_hash, context.ruleset_hash);
    }

    /// Classification must carry an entities broken tag into the quarantine
    /// so already tagged rules are never re-tagged or re-notified
    #[test]
    fn classify_carries_already_tagged() {
        // build two broken rules where only one is already tagged
        let mut tagged = test_rule("tagged", "rule tagged {");
        tagged.broken_tagged = true;
        let fresh = test_rule("fresh", "rule fresh {");
        let (tagged_id, fresh_id) = (tagged.id, fresh.id);
        // classify both broken rules
        let (good, quarantined) = YaraRuleContext::classify(vec![tagged, fresh]);
        assert!(good.is_empty());
        assert_eq!(quarantined.len(), 2);
        // the already tagged rule must carry its flag into the quarantine
        for rule in &quarantined {
            if rule.id == tagged_id {
                assert!(rule.already_tagged, "tagged rule must stay already_tagged");
            } else {
                assert_eq!(rule.id, fresh_id);
                assert!(
                    !rule.already_tagged,
                    "fresh rule must not be already_tagged"
                );
            }
        }
    }

    /// Status tag checks must match specific values and the any-value case
    #[test]
    fn has_status_tag_matrix() {
        // build a tag map carrying a broken status tag
        let mut values = HashMap::new();
        values.insert("Broken".to_owned(), HashSet::from(["corn".to_owned()]));
        let mut tags: TagMap = HashMap::new();
        tags.insert(STATUS_TAG.to_owned(), values);
        // the exact status value must match
        assert!(has_status_tag(&tags, Some("Broken")));
        // a different status value must not match
        assert!(!has_status_tag(&tags, Some("Noisy")));
        // any-value checks must match any status tag at all
        assert!(has_status_tag(&tags, None));
        // a tag map without the status key must never match
        let empty: TagMap = HashMap::new();
        assert!(!has_status_tag(&empty, Some("Broken")));
        assert!(!has_status_tag(&empty, None));
    }

    /// The cache engine version must stay on the servers path segment charset
    #[test]
    fn cache_engine_version_is_path_safe() {
        // get the cache key form of the engine version
        let version = cache_engine_version();
        // it must derive from the recorded engine version
        assert_eq!(version, ENGINE_VERSION.replace(' ', "-"));
        // it must fit the servers `^[A-Za-z0-9._-]{1,128}$` validation
        assert!(!version.is_empty() && version.len() <= 128);
        assert!(
            version
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
        );
        // it must never contain a path traversal
        assert!(!version.contains(".."));
    }

    /// Scanning with the compiled context must attribute hits by namespace
    #[test]
    fn scan_smoke_matches_expected_rule() {
        // build a rule that matches a marker string
        let marker = test_rule(
            "marker",
            "rule marker { strings: $m = \"thorium-test-marker\" condition: $m }",
        );
        let miss = test_rule(
            "miss",
            "rule miss { strings: $m = \"never-here\" condition: $m }",
        );
        let marker_id = marker.id;
        // compile this rule set
        let context = YaraRuleContext::from_rules(vec![marker, miss]);
        // scan bytes containing our marker
        let mut scanner = yara_x::Scanner::new(&context.rules);
        let results = scanner
            .scan(b"some bytes with a thorium-test-marker inside")
            .expect("scan failed");
        // exactly our marker rule must match
        let matching = results.matching_rules().collect::<Vec<_>>();
        assert_eq!(matching.len(), 1);
        // the namespace must map back to the rules entity id
        assert_eq!(matching[0].namespace(), marker_id.to_string());
        assert_eq!(matching[0].identifier(), "marker");
        // the matched string info must carry its offsets
        let patterns = matching[0].patterns().collect::<Vec<_>>();
        assert_eq!(patterns.len(), 1);
        assert!(patterns[0].matches().next().is_some());
    }

    /// A serialized then deserialized ruleset must scan identically, proving
    /// the cache hit path can stand in for a fresh compile
    #[test]
    fn serialize_roundtrip_scans_identically() {
        // build and compile a rule that matches a marker string
        let marker = test_rule(
            "marker",
            "rule marker { strings: $m = \"thorium-test-marker\" condition: $m }",
        );
        let marker_id = marker.id;
        let context = YaraRuleContext::from_rules(vec![marker]);
        // round trip the compiled ruleset through its serialized form
        let serialized = context.rules.serialize().expect("serialize failed");
        let restored = yara_x::Rules::deserialize(serialized).expect("deserialize failed");
        // the restored ruleset must still attribute hits by namespace
        let mut scanner = yara_x::Scanner::new(&restored);
        let results = scanner
            .scan(b"some bytes with a thorium-test-marker inside")
            .expect("scan failed");
        let matching = results.matching_rules().collect::<Vec<_>>();
        assert_eq!(matching.len(), 1);
        assert_eq!(matching[0].namespace(), marker_id.to_string());
    }
}
