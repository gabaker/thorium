//! Thorium's fixed Elastic indexes as the schema registry declares them

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

use super::{Fingerprint, SchemaDefinition, SnapshotObject};
use crate::models::elastic::{ElasticIndex, ElasticSource};

/// The id of the migration that drops Elastic indexes so the search-streamer reingests them
///
/// A step that runs it finishes `Reingesting` rather than `Done`: its reingest goes on after the
/// rollout and is checked there.
pub const REBUILD_MIGRATION: &str = "elastic-rebuild";

/// Whether a migration is a derived rebuild, whose step finishes `Reingesting`
///
/// # Arguments
///
/// * `migration` - The id of the thoradm migration a step runs
#[must_use]
pub fn reingests(migration: &str) -> bool {
    // only the Elastic rebuild hands its work to the search-streamer
    migration == REBUILD_MIGRATION
}

/// One of Thorium's fixed Elastic indexes as the schema registry declares it: its stable name,
/// its source, and the mappings the search-streamer creates it with
///
/// The body holds the mappings only; the index itself is created with
/// `ElasticIndex::create_body`, which adds the settings Thorium sets from the deployment's config.
#[derive(Debug, Clone, PartialEq)]
pub struct ElasticIndexDef {
    /// The index's stable name, the `ElasticIndex` variant (`SampleResults`, …), not the
    /// deployment's configured index name
    pub name: &'static str,
    /// The source the index belongs to
    pub source: ElasticSource,
    /// The index's mappings, as `ElasticIndex::mappings` builds them (`{"mappings": …}`), without
    /// the settings `ElasticIndex::create_body` adds
    pub body: serde_json::Value,
}

impl ElasticIndexDef {
    /// Every Elastic index Thorium declares, in the order of [`ElasticIndex::ALL`]
    #[must_use]
    pub fn declared() -> Vec<ElasticIndexDef> {
        // declare each index in its fixed order
        ElasticIndex::ALL.iter().map(ElasticIndexDef::of).collect()
    }

    /// Declare one of Thorium's Elastic indexes
    ///
    /// Its body holds only the mappings: the one setting Thorium sets, the highlighter's
    /// `max_analyzed_offset`, comes from the deployment's config, so it is configuration rather
    /// than schema.
    ///
    /// # Arguments
    ///
    /// * `index` - The index to declare
    #[must_use]
    pub fn of(index: &ElasticIndex) -> ElasticIndexDef {
        // name the index by its variant and take the mappings the streamer creates it with
        let name = match index {
            ElasticIndex::SampleResults => "SampleResults",
            ElasticIndex::RepoResults => "RepoResults",
            ElasticIndex::SampleTags => "SampleTags",
            ElasticIndex::RepoTags => "RepoTags",
        };
        ElasticIndexDef {
            name,
            source: index.source(),
            body: serde_json::json!({ "mappings": index.mappings() }),
        }
    }
}

impl ElasticSource {
    /// Both sources, in the order their indexes are declared
    pub const ALL: [ElasticSource; 2] = [ElasticSource::Results, ElasticSource::Tags];

    /// The source's name as the search-streamer keys its init session by (`Elastic-<name>`)
    #[must_use]
    pub fn name(self) -> &'static str {
        // the data source's name in the streamer
        match self {
            ElasticSource::Results => "Results",
            ElasticSource::Tags => "Tags",
        }
    }

    /// The Redis keys of the source's init session: its `info` and its `log`
    ///
    /// # Arguments
    ///
    /// * `namespace` - The Thorium namespace the keys are prefixed with
    #[must_use]
    pub fn init_keys(self, namespace: &str) -> [String; 2] {
        // the streamer's `InitSessionKeys`, for the Elastic store and this source
        let name = self.name();
        [
            format!("{namespace}:search-streamer:init:info:Elastic-{name}"),
            format!("{namespace}:search-streamer:init:log:Elastic-{name}"),
        ]
    }

    /// The source's indexes, in the order they are declared
    #[must_use]
    pub fn indexes(self) -> Vec<ElasticIndex> {
        // the declared indexes that belong to this source
        ElasticIndex::ALL
            .into_iter()
            .filter(|index| index.source() == self)
            .collect()
    }
}

impl ElasticIndexDef {
    /// The fingerprint of the mappings this build declares for the index
    #[must_use]
    pub fn fingerprint(&self) -> Fingerprint {
        // the canonical form is what the probe compares an observed index to
        Fingerprint::of(&SchemaDefinition::Elastic(self.clone()).canonical())
    }

    /// The declared indexes the observed ones don't match: missing, or with other mappings
    ///
    /// # Arguments
    ///
    /// * `observed` - What the probe found, which leaves a missing index out
    #[must_use]
    pub fn mismatched(observed: &[SnapshotObject]) -> Vec<&'static str> {
        // an index matches only when it was observed with the declared fingerprint
        ElasticIndexDef::declared()
            .into_iter()
            .filter(|def| {
                let fingerprint = def.fingerprint();
                !observed
                    .iter()
                    .any(|object| object.name == def.name && object.fingerprint == fingerprint)
            })
            .map(|def| def.name)
            .collect()
    }
}

/// What one `elastic-rebuild` run drops; several rebuilds in a plan merge into one
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebuildTarget {
    /// The indexes to delete, by stable name
    pub indexes: BTreeSet<String>,
    /// The sources whose init `info` and `log` keys are deleted (every source an index belongs to)
    pub sources: BTreeSet<ElasticSource>,
}

impl RebuildTarget {
    /// The target that drops the named indexes and resets the sources they belong to
    ///
    /// A name that isn't one of Thorium's indexes is left out.
    ///
    /// # Arguments
    ///
    /// * `names` - The stable names of the indexes to drop
    #[must_use]
    pub fn of<'a>(names: impl IntoIterator<Item = &'a str>) -> RebuildTarget {
        // keep the declared indexes among the names, with the source of each
        let wanted = names.into_iter().collect::<BTreeSet<_>>();
        let mut target = RebuildTarget::default();
        for def in ElasticIndexDef::declared() {
            if wanted.contains(def.name) {
                target.indexes.insert(def.name.to_owned());
                target.sources.insert(def.source);
            }
        }
        target
    }

    /// Whether the target drops nothing
    #[must_use]
    pub fn is_empty(&self) -> bool {
        // no index means no source to reset either
        self.indexes.is_empty()
    }

    /// Merge another rebuild into this one, so one run drops everything both would
    ///
    /// # Arguments
    ///
    /// * `other` - The rebuild to merge in
    #[must_use]
    pub fn merge(mut self, other: &RebuildTarget) -> RebuildTarget {
        // the union of the indexes and of the sources
        self.indexes.extend(other.indexes.iter().cloned());
        self.sources.extend(other.sources.iter().copied());
        self
    }
}

/// What the rebuild step's post-rollout check observes for one source while the step is
/// `Reingesting`; the step is done when no source has init keys left and no index mismatches
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebuildProgress {
    /// The source checked
    pub source: ElasticSource,
    /// Whether the source's init session keys still exist (a reingest running, or a stale session)
    pub init_keys_present: bool,
    /// The source's indexes that are missing or whose mappings the probe sees differ
    pub mismatched: Vec<String>,
}

impl RebuildProgress {
    /// Observe a source from its init keys and what the probe found
    ///
    /// # Arguments
    ///
    /// * `source` - The source to observe
    /// * `init_keys_present` - Whether any of its init session keys exist
    /// * `observed` - What the probe found
    #[must_use]
    pub fn observe(
        source: ElasticSource,
        init_keys_present: bool,
        observed: &[SnapshotObject],
    ) -> RebuildProgress {
        // only the source's own indexes count against it
        let mismatched = ElasticIndexDef::mismatched(observed)
            .into_iter()
            .filter(|name| {
                ElasticIndexDef::declared()
                    .iter()
                    .any(|def| def.name == *name && def.source == source)
            })
            .map(ToOwned::to_owned)
            .collect();
        RebuildProgress {
            source,
            init_keys_present,
            mismatched,
        }
    }

    /// Whether the source finished reingesting: no init session left and every index as declared
    #[must_use]
    pub fn finished(&self) -> bool {
        // a session still in Redis is a reingest running, and a mismatch is one that never ran
        !self.init_keys_present && self.mismatched.is_empty()
    }

    /// Whether the source's reingest was lost: no session runs, yet an index is missing or has
    /// other mappings, so the rebuild must run again
    #[must_use]
    pub fn lost(&self) -> bool {
        // with a session in Redis the streamer is still at work
        !self.init_keys_present && !self.mismatched.is_empty()
    }

    /// Say where the source stands, for the `SearchIncomplete` condition
    #[must_use]
    pub fn describe(&self) -> String {
        // name the source and what keeps it from being done
        let name = self.source.name();
        if self.init_keys_present {
            format!("{name} (the init session is still running)")
        } else if self.mismatched.is_empty() {
            format!("{name} (finished)")
        } else {
            format!(
                "{name} (index {} is missing or mapped differently)",
                self.mismatched.join(", ")
            )
        }
    }
}
