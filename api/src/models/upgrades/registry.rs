//! The backends and layout versions the schema registry declares, kept beside the catalog so
//! `const` steps can name them

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A backend whose schema Thorium declares
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    JsonSchema,
    clap::ValueEnum,
)]
#[serde(rename_all = "lowercase")]
pub enum SchemaBackend {
    /// Scylla tables and materialized views
    Scylla,
    /// Thorium's fixed Elastic indexes
    Elastic,
    /// Redis data families, versioned by layout
    Redis,
    /// S3 buckets, versioned by key layout
    S3,
    /// A value this build doesn't know, read from a status a newer operator wrote; never
    /// declared
    #[serde(other)]
    #[value(skip)]
    Unknown,
}

/// The declared layout version of one Redis family or S3 bucket
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutDef {
    /// `Redis` or `S3`
    pub backend: SchemaBackend,
    /// The Redis family (`users`, `jobs`, …), or for S3 the bucket role (`files`, `repos`,
    /// `results`, `attachments`, `ephemeral`, `reaction_cache`, `graphics`); never a configured
    /// bucket name, since bucket names are deployment config
    pub family: &'static str,
    /// The layout this build writes
    pub version: u32,
    /// The oldest layout this build still reads, so a bump that reads the old layout classifies
    /// as additive
    pub reads_from: u32,
    /// The fields of this layout a lossy downgrade must fill restrictively, such as the users'
    /// `role`, checked by the fill lint (not part of the fingerprint)
    pub security_sensitive: &'static [&'static str],
    /// How migrations enumerate the family (Redis only; S3 layouts are enumerated by prefix); not
    /// part of the fingerprint
    pub enumerate: Option<Enumeration>,
}

/// How the runner enumerates a Redis family for a check, a run, or a `--reason` re-run
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enumeration {
    /// Through an index set holding every member's key part (`SSCAN {ns}:users`), sliced by
    /// member hash modulo the slice count
    IndexSet(&'static str),
    /// Through `SCAN MATCH <pattern>` with `COUNT 10000`, in one slice whose checkpoint is the
    /// `SCAN` cursor; its cost follows the instance's total key count, so only for families with no
    /// index, such as the census keys
    Scan(&'static str),
}

/// Every Redis family and S3 bucket role this build declares a layout for, in the order they
/// were added (append-only)
///
/// A family's version is bumped by hand in the same change as the migration that converts its
/// data. Enumeration patterns are written after the `{ns}:` prefix, and `{group}` stands for each
/// member of the `groups` index set.
pub const LAYOUTS: &[LayoutDef] = &[
    REDIS_USERS_LAYOUT,
    REDIS_GROUPS_LAYOUT,
    REDIS_IMAGES_LAYOUT,
    REDIS_PIPELINES_LAYOUT,
    REDIS_NETWORK_POLICIES_LAYOUT,
    REDIS_JOBS_LAYOUT,
    REDIS_REACTIONS_LAYOUT,
    REDIS_STREAMS_LAYOUT,
    REDIS_SYSTEM_LAYOUT,
    REDIS_WORKERS_LAYOUT,
    REDIS_EVENTS_LAYOUT,
    REDIS_SEARCH_EVENTS_LAYOUT,
    REDIS_SEARCH_STREAMER_INIT_LAYOUT,
    REDIS_OAUTH_LAYOUT,
    REDIS_VENDORS_LAYOUT,
    REDIS_CURSORS_LAYOUT,
    REDIS_CENSUS_SAMPLES_LAYOUT,
    REDIS_CENSUS_REPOS_LAYOUT,
    REDIS_CENSUS_COMMITISH_LAYOUT,
    REDIS_CENSUS_TAGS_LAYOUT,
    REDIS_CENSUS_TAGS_CASE_INSENSITIVE_LAYOUT,
    REDIS_CENSUS_ENTITIES_LAYOUT,
    REDIS_CENSUS_ASSOCIATIONS_LAYOUT,
    S3_FILES_LAYOUT,
    S3_REPOS_LAYOUT,
    S3_RESULTS_LAYOUT,
    S3_ATTACHMENTS_LAYOUT,
    S3_EPHEMERAL_LAYOUT,
    S3_REACTION_CACHE_LAYOUT,
    S3_GRAPHICS_LAYOUT,
];

/// The Redis layout of user data, the token, email and scoped-token maps, the analysts set, and
/// each user's groups (`{ns}:users`, `{ns}:user_data:{user}`, …)
///
/// Its baseline is layout 2, the layout that keeps `theme` inside `settings`; layout 1 is the
/// development-only layout the example users migration converts from.
pub const REDIS_USERS_LAYOUT: LayoutDef = LayoutDef {
    backend: SchemaBackend::Redis,
    family: "users",
    version: 2,
    reads_from: 2,
    security_sensitive: &["role"],
    enumerate: Some(Enumeration::IndexSet("users")),
};

/// The Redis layout of group data and the per-role member sets (`{ns}:groups`,
/// `{ns}:groups_data:{group}`, …)
pub const REDIS_GROUPS_LAYOUT: LayoutDef = LayoutDef {
    backend: SchemaBackend::Redis,
    family: "groups",
    version: 1,
    reads_from: 1,
    security_sensitive: &[],
    enumerate: Some(Enumeration::IndexSet("groups")),
};

/// The Redis layout of image data, each group's image set, runtimes and users
/// (`{ns}:image_data:{group}:{name}`, …)
pub const REDIS_IMAGES_LAYOUT: LayoutDef = LayoutDef {
    backend: SchemaBackend::Redis,
    family: "images",
    version: 1,
    reads_from: 1,
    security_sensitive: &[],
    enumerate: Some(Enumeration::IndexSet("images:{group}")),
};

/// The Redis layout of pipeline data and each group's pipeline set
/// (`{ns}:pipeline_data:{group}:{name}`, …)
pub const REDIS_PIPELINES_LAYOUT: LayoutDef = LayoutDef {
    backend: SchemaBackend::Redis,
    family: "pipelines",
    version: 2,
    reads_from: 1,
    security_sensitive: &[],
    enumerate: Some(Enumeration::IndexSet("pipelines:{group}")),
};

/// The Redis layout of each group's network policy set and the images using each policy
/// (`{ns}:netpols:{group}`, …)
pub const REDIS_NETWORK_POLICIES_LAYOUT: LayoutDef = LayoutDef {
    backend: SchemaBackend::Redis,
    family: "network_policies",
    version: 1,
    reads_from: 1,
    security_sensitive: &[],
    enumerate: Some(Enumeration::IndexSet("netpols:{group}")),
};

/// The Redis layout of job data and the job queues (`{ns}:job_data:{id}`, `{ns}:job_queue:…`)
pub const REDIS_JOBS_LAYOUT: LayoutDef = LayoutDef {
    backend: SchemaBackend::Redis,
    family: "jobs",
    version: 1,
    reads_from: 1,
    security_sensitive: &[],
    enumerate: Some(Enumeration::Scan("job_data:*")),
};

/// The Redis layout of reaction data, status and tag sets, jobs, logs, sub-reactions, generators
/// and caches (`{ns}:reaction_data:{group}:{id}`, …)
pub const REDIS_REACTIONS_LAYOUT: LayoutDef = LayoutDef {
    backend: SchemaBackend::Redis,
    family: "reactions",
    version: 1,
    reads_from: 1,
    security_sensitive: &[],
    enumerate: Some(Enumeration::Scan("reaction_data:*")),
};

/// The Redis layout of the timestamp-scored streams (`{ns}:streams:{group}:{namespace}:{stream}`,
/// …)
pub const REDIS_STREAMS_LAYOUT: LayoutDef = LayoutDef {
    backend: SchemaBackend::Redis,
    family: "streams",
    version: 1,
    reads_from: 1,
    security_sensitive: &[],
    enumerate: Some(Enumeration::Scan("streams:*")),
};

/// The Redis layout of the system info and settings hashes (`{ns}:system_info`,
/// `{ns}:system_settings`)
pub const REDIS_SYSTEM_LAYOUT: LayoutDef = LayoutDef {
    backend: SchemaBackend::Redis,
    family: "system",
    version: 1,
    reads_from: 1,
    security_sensitive: &[],
    enumerate: Some(Enumeration::Scan("system_*")),
};

/// The Redis layout of worker data and the worker sets per cluster, node and scaler
/// (`{ns}:worker_data:{name}`, …)
pub const REDIS_WORKERS_LAYOUT: LayoutDef = LayoutDef {
    backend: SchemaBackend::Redis,
    family: "workers",
    version: 1,
    reads_from: 1,
    security_sensitive: &[],
    enumerate: Some(Enumeration::Scan("worker_data:*")),
};

/// The Redis layout of the event handler's cache flag, queues and in-flight maps
/// (`{ns}:event-handler:…`)
pub const REDIS_EVENTS_LAYOUT: LayoutDef = LayoutDef {
    backend: SchemaBackend::Redis,
    family: "events",
    version: 1,
    reads_from: 1,
    security_sensitive: &[],
    enumerate: Some(Enumeration::Scan("event-handler:*")),
};

/// The Redis layout of the search-streamer's event queues and in-flight maps
/// (`{ns}:search-streamer:{results|tags}:…`)
pub const REDIS_SEARCH_EVENTS_LAYOUT: LayoutDef = LayoutDef {
    backend: SchemaBackend::Redis,
    family: "search_events",
    version: 1,
    reads_from: 1,
    security_sensitive: &[],
    enumerate: Some(Enumeration::Scan("search-streamer:[rt]*")),
};

/// The Redis layout of the search-streamer's init session info and logs
/// (`{ns}:search-streamer:init:…`)
pub const REDIS_SEARCH_STREAMER_INIT_LAYOUT: LayoutDef = LayoutDef {
    backend: SchemaBackend::Redis,
    family: "search_streamer_init",
    version: 1,
    reads_from: 1,
    security_sensitive: &[],
    enumerate: Some(Enumeration::Scan("search-streamer:init:*")),
};

/// The Redis layout of OAuth contexts, alias maps, registration sessions and link tokens
/// (`{ns}:oauth:…`)
pub const REDIS_OAUTH_LAYOUT: LayoutDef = LayoutDef {
    backend: SchemaBackend::Redis,
    family: "oauth",
    version: 1,
    reads_from: 1,
    security_sensitive: &[],
    enumerate: Some(Enumeration::Scan("oauth:*")),
};

/// The Redis layout of vendor device sets (`{ns}:entities:vendors:{id}:devices`)
pub const REDIS_VENDORS_LAYOUT: LayoutDef = LayoutDef {
    backend: SchemaBackend::Redis,
    family: "vendors",
    version: 1,
    reads_from: 1,
    security_sensitive: &[],
    enumerate: Some(Enumeration::Scan("entities:vendors:*")),
};

/// The Redis layout of saved cursors (`{ns}:cursors:{kind}:v2:{user}:{id}`), at the layout
/// version the keys carry; cursors saved under an older layout are never read, so their ids are
/// reported as not found
pub const REDIS_CURSORS_LAYOUT: LayoutDef = LayoutDef {
    backend: SchemaBackend::Redis,
    family: "cursors",
    version: 2,
    reads_from: 2,
    security_sensitive: &[],
    enumerate: Some(Enumeration::Scan("cursors:*")),
};

/// The Redis layout of the samples census buckets (`{ns}:census:samples:stream:{group}:{year}`)
pub const REDIS_CENSUS_SAMPLES_LAYOUT: LayoutDef = LayoutDef {
    backend: SchemaBackend::Redis,
    family: "census_samples",
    version: 1,
    reads_from: 1,
    security_sensitive: &[],
    enumerate: Some(Enumeration::Scan("census:samples:stream:*")),
};

/// The Redis layout of the repos census buckets (`{ns}:census:repos:stream:{group}:{year}`)
pub const REDIS_CENSUS_REPOS_LAYOUT: LayoutDef = LayoutDef {
    backend: SchemaBackend::Redis,
    family: "census_repos",
    version: 1,
    reads_from: 1,
    security_sensitive: &[],
    enumerate: Some(Enumeration::Scan("census:repos:stream:*")),
};

/// The Redis layout of the commitish census buckets
/// (`{ns}:census:commitish:stream:{kind}:{group}:{repo}:{year}`)
pub const REDIS_CENSUS_COMMITISH_LAYOUT: LayoutDef = LayoutDef {
    backend: SchemaBackend::Redis,
    family: "census_commitish",
    version: 1,
    reads_from: 1,
    security_sensitive: &[],
    enumerate: Some(Enumeration::Scan("census:commitish:stream:*")),
};

/// The Redis layout of the tags census buckets
/// (`{ns}:census:tags:stream:{kind}:{group}:{key}:{value}:{year}`)
pub const REDIS_CENSUS_TAGS_LAYOUT: LayoutDef = LayoutDef {
    backend: SchemaBackend::Redis,
    family: "census_tags",
    version: 1,
    reads_from: 1,
    security_sensitive: &[],
    enumerate: Some(Enumeration::Scan("census:tags:stream:*")),
};

/// The Redis layout of the case-insensitive tags census buckets
/// (`{ns}:census:tags_case_insensitive:stream:…`)
pub const REDIS_CENSUS_TAGS_CASE_INSENSITIVE_LAYOUT: LayoutDef = LayoutDef {
    backend: SchemaBackend::Redis,
    family: "census_tags_case_insensitive",
    version: 1,
    reads_from: 1,
    security_sensitive: &[],
    enumerate: Some(Enumeration::Scan("census:tags_case_insensitive:stream:*")),
};

/// The Redis layout of the entities census buckets (`{ns}:census:entities:stream:{group}:{year}`)
pub const REDIS_CENSUS_ENTITIES_LAYOUT: LayoutDef = LayoutDef {
    backend: SchemaBackend::Redis,
    family: "census_entities",
    version: 1,
    reads_from: 1,
    security_sensitive: &[],
    enumerate: Some(Enumeration::Scan("census:entities:stream:*")),
};

/// The Redis layout of the associations census buckets
/// (`{ns}:census:associations:stream:{group}:{source}:{year}`)
pub const REDIS_CENSUS_ASSOCIATIONS_LAYOUT: LayoutDef = LayoutDef {
    backend: SchemaBackend::Redis,
    family: "census_associations",
    version: 1,
    reads_from: 1,
    security_sensitive: &[],
    enumerate: Some(Enumeration::Scan("census:associations:stream:*")),
};

/// The S3 layout of carted files keyed by their S3 id (`{s3_id}`)
pub const S3_FILES_LAYOUT: LayoutDef = LayoutDef {
    backend: SchemaBackend::S3,
    family: "files",
    version: 1,
    reads_from: 1,
    security_sensitive: &[],
    enumerate: None,
};

/// The S3 layout of carted repo zips keyed by their S3 id (`{s3_id}`)
pub const S3_REPOS_LAYOUT: LayoutDef = LayoutDef {
    backend: SchemaBackend::S3,
    family: "repos",
    version: 1,
    reads_from: 1,
    security_sensitive: &[],
    enumerate: None,
};

/// The S3 layout of result files and entity lists (`{result_id}/{file}`,
/// `{result_id}/__thorium_entities/{kind}.json`)
pub const S3_RESULTS_LAYOUT: LayoutDef = LayoutDef {
    backend: SchemaBackend::S3,
    family: "results",
    version: 1,
    reads_from: 1,
    security_sensitive: &[],
    enumerate: None,
};

/// The S3 layout of comment attachments (`{sha256|key}/{comment_id}/{s3_id}`)
pub const S3_ATTACHMENTS_LAYOUT: LayoutDef = LayoutDef {
    backend: SchemaBackend::S3,
    family: "attachments",
    version: 1,
    reads_from: 1,
    security_sensitive: &[],
    enumerate: None,
};

/// The S3 layout of ephemeral reaction files (`{reaction_id}/{name}`)
pub const S3_EPHEMERAL_LAYOUT: LayoutDef = LayoutDef {
    backend: SchemaBackend::S3,
    family: "ephemeral",
    version: 1,
    reads_from: 1,
    security_sensitive: &[],
    enumerate: None,
};

/// The S3 layout of carted reaction cache files (`{reaction_id}/files/{path}`)
pub const S3_REACTION_CACHE_LAYOUT: LayoutDef = LayoutDef {
    backend: SchemaBackend::S3,
    family: "reaction_cache",
    version: 1,
    reads_from: 1,
    security_sensitive: &[],
    enumerate: None,
};

/// The S3 layout of entity and user images (`{entity_id}/{filename}.{ext}`,
/// `users/{ns}/{user}/icon.{ext}`)
pub const S3_GRAPHICS_LAYOUT: LayoutDef = LayoutDef {
    backend: SchemaBackend::S3,
    family: "graphics",
    version: 1,
    reads_from: 1,
    security_sensitive: &[],
    enumerate: None,
};

/// What kind of Scylla object a legacy object is
#[cfg(feature = "scylla-utils")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LegacyKind {
    /// A table, dropped with `DROP TABLE`
    Table,
    /// A materialized view, dropped with `DROP MATERIALIZED VIEW`
    View,
}

#[cfg(feature = "scylla-utils")]
impl LegacyKind {
    /// Returns the kind's name for humans
    #[must_use]
    pub fn label(self) -> &'static str {
        // name the kind the way the admin guide does
        match self {
            LegacyKind::Table => "table",
            LegacyKind::View => "view",
        }
    }
}

/// Whether a revision needs a legacy object's data
#[cfg(feature = "scylla-utils")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LegacyNeed {
    /// A downgrade to the revision recreates the object, empty or rebuilt from its base table
    Rebuildable,
    /// The object's rows are the only copy, so dropping it loses data for the revision
    Required,
}

/// A revision that reads a legacy object, and whether its data can be recreated
#[cfg(feature = "scylla-utils")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct LegacyUse {
    /// The revision that reads the object
    pub revision: &'static str,
    /// Whether the object's data can be recreated or must be kept
    pub need: LegacyNeed,
}

/// One object an older release created that the current build no longer declares
///
/// [`LEGACY`] is append-only: an entry is never removed or edited to say the object never
/// existed, because a revision in the support window may have created it.
#[cfg(feature = "scylla-utils")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct LegacyDef {
    /// The store the object lives in; Scylla for every entry today
    pub backend: SchemaBackend,
    /// Whether the object is a table or a view
    pub kind: LegacyKind,
    /// The object's name
    pub name: &'static str,
    /// The object's data class, the same one the class table gives it
    pub class: crate::models::schema::DataClass,
    /// The revisions whose setup created it
    pub created_by: &'static [&'static str],
    /// The revisions that read it, each marked rebuildable or required
    pub used_by: &'static [LegacyUse],
    /// A short note on why it was retired
    pub retirement: &'static str,
}

#[cfg(feature = "scylla-utils")]
impl LegacyDef {
    /// Returns the object's id as the admin commands name it, `<backend>:<name>`
    #[must_use]
    pub fn id(&self) -> String {
        // the backend's serialized name, a colon, then the object's name
        format!("{}:{}", backend_name(self.backend), self.name)
    }
}

/// Returns the lowercase name a backend carries in an object id
///
/// # Arguments
///
/// * `backend` - The backend to name
#[cfg(feature = "scylla-utils")]
#[must_use]
pub fn backend_name(backend: SchemaBackend) -> &'static str {
    // the same lowercase names the serialized form uses
    match backend {
        SchemaBackend::Scylla => "scylla",
        SchemaBackend::Elastic => "elastic",
        SchemaBackend::Redis => "redis",
        SchemaBackend::S3 => "s3",
        SchemaBackend::Unknown => "unknown",
    }
}

/// Every object an older release created that this build no longer declares, in the order they
/// were retired (append-only)
///
/// A developer who stops creating an object adds its entry in the same change. The class
/// table's entry for the object carries the same data class.
#[cfg(feature = "scylla-utils")]
pub const LEGACY: &[LegacyDef] = &[
    LegacyDef {
        backend: SchemaBackend::Scylla,
        kind: LegacyKind::Table,
        name: "events",
        class: crate::models::schema::DataClass::Runtime,
        created_by: &["2026-10-v01", "2026-10-v02"],
        used_by: &[],
        retirement: "a table of trigger events that nothing reads or writes; setup no longer \
                     creates it",
    },
    LegacyDef {
        backend: SchemaBackend::Scylla,
        kind: LegacyKind::View,
        name: "entities_by_name",
        class: crate::models::schema::DataClass::Derived,
        created_by: &["2026-10-v01", "2026-10-v02"],
        used_by: &[],
        retirement: "a view of the entities table that nothing reads; setup no longer creates it",
    },
];

/// Look up the legacy entry for an object
///
/// # Arguments
///
/// * `backend` - The object's backend
/// * `name` - The object's name
#[cfg(feature = "scylla-utils")]
#[must_use]
pub fn legacy_def(backend: SchemaBackend, name: &str) -> Option<&'static LegacyDef> {
    // entries are unique by backend and name, which the registry tests check
    LEGACY
        .iter()
        .find(|entry| entry.backend == backend && entry.name == name)
}

/// Whether an object is registered as legacy
///
/// # Arguments
///
/// * `backend` - The object's backend
/// * `name` - The object's name
#[cfg(feature = "scylla-utils")]
#[must_use]
pub fn is_legacy(backend: SchemaBackend, name: &str) -> bool {
    // registered means an entry exists
    legacy_def(backend, name).is_some()
}

#[cfg(test)]
mod tests {
    use super::{Enumeration, LAYOUTS, SchemaBackend};
    use std::collections::BTreeSet;

    /// The Redis families the API, thoradm and the search-streamer write
    const REDIS_FAMILIES: &[&str] = &[
        "users",
        "groups",
        "images",
        "pipelines",
        "network_policies",
        "jobs",
        "reactions",
        "streams",
        "system",
        "workers",
        "events",
        "search_events",
        "search_streamer_init",
        "oauth",
        "vendors",
        "cursors",
        "census_samples",
        "census_repos",
        "census_commitish",
        "census_tags",
        "census_tags_case_insensitive",
        "census_entities",
        "census_associations",
    ];

    /// The S3 bucket roles Thorium stores objects under
    const S3_ROLES: &[&str] = &[
        "files",
        "repos",
        "results",
        "attachments",
        "ephemeral",
        "reaction_cache",
        "graphics",
    ];

    /// Every Redis family and S3 bucket role has exactly one layout, at its baseline version (the
    /// pipelines layout is written at version 2 and still read at version 1), and the cursors
    /// layout matches the version the cursor keys carry
    #[test]
    fn layouts_cover_every_family_once() {
        // each family is declared once per backend
        let mut seen = BTreeSet::new();
        for layout in LAYOUTS {
            assert!(
                seen.insert((layout.backend, layout.family)),
                "{:?} family {} is declared twice",
                layout.backend,
                layout.family
            );
        }
        // the declared families are exactly the Redis families and the S3 roles
        let families = |backend: SchemaBackend| {
            LAYOUTS
                .iter()
                .filter(|layout| layout.backend == backend)
                .map(|layout| layout.family)
                .collect::<BTreeSet<_>>()
        };
        assert_eq!(
            families(SchemaBackend::Redis),
            REDIS_FAMILIES.iter().copied().collect::<BTreeSet<&str>>()
        );
        assert_eq!(
            families(SchemaBackend::S3),
            S3_ROLES.iter().copied().collect::<BTreeSet<&str>>()
        );
        assert!(
            LAYOUTS
                .iter()
                .all(|layout| matches!(layout.backend, SchemaBackend::Redis | SchemaBackend::S3))
        );
        for layout in LAYOUTS {
            // users start at layout 2, after the example's development-only layout 1, and cursors
            // at 2, the versioned keys this build writes; pipelines write layout 2 (a tool for
            // each stage) and still read layout 1; the rest at 1
            let baseline = match layout.family {
                "users" | "cursors" => 2,
                _ => 1,
            };
            let version = if layout.family == "pipelines" {
                2
            } else {
                baseline
            };
            assert_eq!(layout.version, version, "{} version", layout.family);
            assert_eq!(layout.reads_from, baseline, "{} reads_from", layout.family);
            // Redis families say how they are enumerated, S3 roles are enumerated by prefix
            match layout.backend {
                SchemaBackend::Redis => assert!(layout.enumerate.is_some(), "{}", layout.family),
                _ => assert!(layout.enumerate.is_none(), "{}", layout.family),
            }
        }
        // the cursors layout is the version the cursor keys are written under
        #[cfg(feature = "api")]
        assert_eq!(
            super::REDIS_CURSORS_LAYOUT.version,
            crate::models::backends::db::keys::cursors::LAYOUT_VERSION
        );
    }

    /// The users and groups families enumerate through their index sets, and the users' role
    /// must be filled restrictively by a lossy downgrade
    #[test]
    fn indexed_families_use_their_index_sets() {
        // find a Redis family's layout
        let redis = |family: &str| {
            LAYOUTS
                .iter()
                .find(|layout| layout.backend == SchemaBackend::Redis && layout.family == family)
                .unwrap_or_else(|| panic!("no {family} layout"))
        };
        assert_eq!(
            redis("users").enumerate,
            Some(Enumeration::IndexSet("users"))
        );
        assert_eq!(
            redis("groups").enumerate,
            Some(Enumeration::IndexSet("groups"))
        );
        assert_eq!(redis("users").security_sensitive, &["role"]);
        // census families have no index set, so they are scanned
        assert!(matches!(
            redis("census_samples").enumerate,
            Some(Enumeration::Scan(_))
        ));
    }
}
