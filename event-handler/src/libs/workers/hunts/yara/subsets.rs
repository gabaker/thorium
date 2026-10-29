//! A bounded cache of compiled yara rule subsets for hunt workers
//!
//! Every hunt freezes its own rule id snapshot, so hunt workers compile per
//! subset instead of sharing the one global ruleset. Compiles are expensive
//! and hunts re-enter every chunk, so each worker keeps a small LRU of
//! compiled subsets keyed by the frozen id list - two hunts freezing the same
//! ids share one build. Entries remember the rule text hashes they compiled
//! from so a rule cache update only evicts the subsets an edit actually
//! touched, not every warm build on the ten minute full rebuild backstop.

use data_encoding::HEXLOWER;
use futures::StreamExt;
use futures::stream;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use thorium::models::EntityMetadata;
use thorium::{Error, Thorium};
use tracing::{Level, event, instrument};
use uuid::Uuid;

use super::context::{CompilableRule, RuleMeta, YaraRuleContext};

/// How many rule entity fetches a subset build runs concurrently
const RULE_FETCH_PARALLELISM: usize = 16;

/// One compiled hunt rule subset and the facts it was built from
pub struct CompiledSubset {
    /// The compiled subset context
    pub context: YaraRuleContext,
    /// The rule text hashes this subset was built from, keyed by rule id
    ///
    /// Rule cache updates compare these against the fresh global context's
    /// metadata to decide whether this subset is stale.
    members: HashMap<Uuid, String>,
    /// How many frozen rules no longer existed at build time
    pub missing_rules: u64,
}

/// A bounded least recently used cache of compiled rule subsets
pub struct SubsetLru {
    /// The cached compiled subsets keyed by their frozen id list hash
    map: HashMap<String, CompiledSubset>,
    /// The cached subset keys from least to most recently used
    order: VecDeque<String>,
    /// The max number of compiled subsets to keep
    cap: usize,
}

impl SubsetLru {
    /// Create a new empty subset cache
    ///
    /// # Arguments
    ///
    /// * `cap` - The max number of compiled subsets to keep; clamped to 1
    pub fn new(cap: usize) -> Self {
        SubsetLru {
            map: HashMap::default(),
            order: VecDeque::default(),
            // a zero cap would evict every build before it could be returned
            cap: cap.max(1),
        }
    }

    /// Build the cache key for a frozen rule id list
    ///
    /// The ids are hashed sorted so the same frozen set always keys the same
    /// entry regardless of snapshot order; this needs no rule text so cache
    /// hits skip the per id fetches entirely.
    ///
    /// # Arguments
    ///
    /// * `rules` - The frozen rule ids to key
    pub fn key(rules: &[Uuid]) -> String {
        // sort the ids so the key is order independent
        let mut sorted = rules.to_vec();
        sorted.sort();
        // hash the sorted ids into the cache key
        let mut hasher = Sha256::new();
        for id in &sorted {
            hasher.update(id.as_bytes());
        }
        HEXLOWER.encode(&hasher.finalize())
    }

    /// Get a cached subset and mark it most recently used
    ///
    /// # Arguments
    ///
    /// * `key` - The frozen id list hash to look up
    pub fn get(&mut self, key: &str) -> Option<&CompiledSubset> {
        // bail early if this subset is not cached
        if !self.map.contains_key(key) {
            return None;
        }
        // move this subset to the most recently used position
        self.touch(key);
        self.map.get(key)
    }

    /// Get a cached subset without touching its recency
    ///
    /// # Arguments
    ///
    /// * `key` - The frozen id list hash to look up
    pub fn peek(&self, key: &str) -> Option<&CompiledSubset> {
        self.map.get(key)
    }

    /// Insert a compiled subset, evicting the least recently used past the cap
    ///
    /// # Arguments
    ///
    /// * `key` - The frozen id list hash to cache this subset under
    /// * `subset` - The compiled subset to cache
    pub fn insert(&mut self, key: String, subset: CompiledSubset) {
        // replace any existing entry for this key in place
        if self.map.insert(key.clone(), subset).is_none() {
            // this key is new so evict down to the cap first
            while self.map.len() > self.cap {
                // drop the least recently used subset
                if let Some(oldest) = self.order.pop_front() {
                    self.map.remove(&oldest);
                } else {
                    break;
                }
            }
            // track this key as most recently used
            self.order.push_back(key);
        } else {
            // a replaced entry just refreshes its recency
            self.touch(&key);
        }
    }

    /// Evict every cached subset a rule cache update made stale
    ///
    /// A subset is stale when any member's text hash differs from the fresh
    /// global context's metadata or the member is absent from it entirely
    /// (quarantined rules are not in the global build, so subsets holding one
    /// rebuild each update - the churn is the price of picking up an edit to a
    /// quarantined rule, which the global metadata cannot witness).
    ///
    /// # Arguments
    ///
    /// * `meta` - The fresh global context's per rule metadata
    pub fn evict_changed(&mut self, meta: &HashMap<Uuid, RuleMeta>) {
        // find every cached subset with a changed or unwitnessed member
        let stale: Vec<String> = self
            .map
            .iter()
            .filter(|(_, subset)| {
                subset.members.iter().any(|(id, rule_hash)| {
                    meta.get(id)
                        .is_none_or(|fresh| &fresh.rule_hash != rule_hash)
                })
            })
            .map(|(key, _)| key.clone())
            .collect();
        // evict every stale subset
        for key in stale {
            self.map.remove(&key);
            self.order.retain(|held| held != &key);
        }
    }

    /// Move a cached subset to the most recently used position
    ///
    /// # Arguments
    ///
    /// * `key` - The subset key to touch
    fn touch(&mut self, key: &str) {
        // drop this key from wherever it sits in the recency order
        self.order.retain(|held| held != key);
        // re-add it as the most recently used
        self.order.push_back(key.to_owned());
    }
}

/// Fetch and compile a hunt's frozen rule subset
///
/// Rule text is read at load time so a rule edited mid hunt scans with its
/// current text; a deleted rule is skipped and counted. A fetch failure that
/// is not a clean 404 fails the whole build so the chunk stays in flight and
/// retries - a transient API error must never shrink a hunt's rule set, let
/// alone fail the hunt.
///
/// # Arguments
///
/// * `thorium` - A Thorium client
/// * `rules` - The frozen rule ids to fetch and compile
#[instrument(name = "subsets::build_subset", skip_all, fields(rules = rules.len()), err(Debug))]
pub async fn build_subset(thorium: &Arc<Thorium>, rules: &[Uuid]) -> Result<CompiledSubset, Error> {
    // fetch every frozen rule entity with bounded concurrency
    let fetched = stream::iter(rules.iter().copied())
        .map(|id| {
            // clone the client handle this fetch needs
            let thorium = thorium.clone();
            async move { (id, thorium.entities.get(id).await) }
        })
        .buffer_unordered(RULE_FETCH_PARALLELISM)
        .collect::<Vec<_>>()
        .await;
    // gather the sources to compile and the facts eviction compares against
    let mut sources = Vec::with_capacity(fetched.len());
    let mut members = HashMap::with_capacity(fetched.len());
    let mut missing_rules = 0;
    for (id, result) in fetched {
        // unpack this fetch
        let entity = match result {
            Ok(entity) => entity,
            Err(error) => {
                // a deleted rule skips and counts
                if error.status() == Some(reqwest::StatusCode::NOT_FOUND) {
                    // log that this frozen rule no longer exists
                    event!(
                        Level::WARN,
                        msg = "Frozen hunt rule no longer exists",
                        id = id.to_string(),
                    );
                    missing_rules += 1;
                    continue;
                }
                // anything else is transient; fail the build so the chunk retries
                return Err(error);
            }
        };
        // pull the yara rule metadata out of this entity
        let EntityMetadata::YaraRule(rule) = &entity.metadata else {
            // a frozen id that is not a yara rule can never scan; skip it
            event!(
                Level::ERROR,
                msg = "Frozen hunt rule is not a yara rule",
                id = id.to_string(),
                kind = entity.kind.as_str(),
            );
            missing_rules += 1;
            continue;
        };
        // hash this rules current text for the eviction compare
        let rule_hash = rule.rule_hash();
        members.insert(id, rule_hash.clone());
        // add this rules source to our compile list
        sources.push(CompilableRule {
            id: entity.id,
            name: entity.name.clone(),
            submitter: entity.submitter.clone(),
            groups: entity.groups.clone(),
            source: rule.rule.clone(),
            rule_hash,
            // hunts never tag or notify so the broken flag is irrelevant here
            broken_tagged: false,
            // set truthfully even though hunts never quarantine either
            quarantine_exempt: rule.quarantine_exempt,
        });
    }
    // compile this subset GET-only against the serialized ruleset cache
    let context = YaraRuleContext::for_subset(thorium, sources).await?;
    Ok(CompiledSubset {
        context,
        members,
        missing_rules,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a bare compiled subset around a member map for cache tests
    ///
    /// # Arguments
    ///
    /// * `members` - The (rule id, rule hash) members to record
    fn test_subset(members: &[(Uuid, &str)]) -> CompiledSubset {
        CompiledSubset {
            // an empty compile is fine here; cache tests never scan
            context: YaraRuleContext::from_rules(Vec::default()),
            members: members
                .iter()
                .map(|(id, hash)| (*id, (*hash).to_owned()))
                .collect(),
            missing_rules: 0,
        }
    }

    /// Build the global metadata map eviction compares against
    ///
    /// # Arguments
    ///
    /// * `rules` - The (rule id, rule hash) pairs the global context carries
    fn test_meta(rules: &[(Uuid, &str)]) -> HashMap<Uuid, RuleMeta> {
        rules
            .iter()
            .map(|(id, hash)| {
                (
                    *id,
                    RuleMeta {
                        name: "rule".to_owned(),
                        groups: vec!["corn".to_owned()],
                        rule_hash: (*hash).to_owned(),
                        quarantine_exempt: false,
                    },
                )
            })
            .collect()
    }

    /// The cache key must not depend on the frozen id order
    #[test]
    fn subset_key_order_independent() {
        // build two ids in both orders
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        // the key must be identical regardless of order
        assert_eq!(SubsetLru::key(&[a, b]), SubsetLru::key(&[b, a]));
        // different sets must key differently
        assert_ne!(SubsetLru::key(&[a]), SubsetLru::key(&[a, b]));
    }

    /// The cache must evict its least recently used entry past the cap
    #[test]
    fn subset_lru_evicts_least_recently_used() {
        // build a cache that holds two subsets
        let mut lru = SubsetLru::new(2);
        let rule = Uuid::new_v4();
        // insert two subsets
        lru.insert("a".to_owned(), test_subset(&[(rule, "h1")]));
        lru.insert("b".to_owned(), test_subset(&[(rule, "h1")]));
        // touch the oldest so the other becomes least recently used
        assert!(lru.get("a").is_some());
        // inserting a third must evict the least recently used entry
        lru.insert("c".to_owned(), test_subset(&[(rule, "h1")]));
        assert!(lru.get("b").is_none());
        assert!(lru.get("a").is_some());
        assert!(lru.get("c").is_some());
    }

    /// A zero cap must clamp to one so a build can always be returned
    #[test]
    fn subset_lru_zero_cap_clamps() {
        // build a cache with a nonsense zero cap
        let mut lru = SubsetLru::new(0);
        let rule = Uuid::new_v4();
        // the inserted subset must survive its own insert
        lru.insert("a".to_owned(), test_subset(&[(rule, "h1")]));
        assert!(lru.get("a").is_some());
    }

    /// Rule cache updates must evict only the subsets an edit touched
    #[test]
    fn subset_lru_evicts_changed_members_only() {
        // build three rules where only one will change
        let stable = Uuid::new_v4();
        let edited = Uuid::new_v4();
        let quarantined = Uuid::new_v4();
        // cache three subsets: untouched, edited member, quarantined member
        let mut lru = SubsetLru::new(4);
        lru.insert("stable".to_owned(), test_subset(&[(stable, "h1")]));
        lru.insert(
            "edited".to_owned(),
            test_subset(&[(stable, "h1"), (edited, "h2")]),
        );
        lru.insert(
            "quarantined".to_owned(),
            test_subset(&[(quarantined, "h3")]),
        );
        // the fresh global context carries the edit and lacks the quarantined rule
        let meta = test_meta(&[(stable, "h1"), (edited, "h2-edited")]);
        lru.evict_changed(&meta);
        // the untouched subset must stay warm
        assert!(lru.get("stable").is_some());
        // the subset holding the edited member must be evicted
        assert!(lru.get("edited").is_none());
        // the subset holding a rule the global build cannot witness must be
        // evicted so an edit to a quarantined rule is still picked up
        assert!(lru.get("quarantined").is_none());
    }
}
