//! The class table: every object Thorium stores, its data class, and how backup covers it
//!
//! Each Scylla table and view, Redis key family, and S3 bucket role has one [`ClassEntry`] in
//! [`CLASSES`], and each Redis layout family in [`LAYOUTS`](crate::models::upgrades::LAYOUTS) has
//! one [`FamilyClass`] in [`REDIS_FAMILIES`]. The schema-change classifier reads an object's class
//! from here, and `thoradm backup` reads its coverage (backed up or excluded, the reason, the known
//! gap, and a table's archived columns) from here; thoradm keeps only the mapping from a backed-up
//! table or bucket role to the backup component that carries it.
//!
//! Adding an object means adding its entry here: a table or view to [`CLASSES`] (a backed-up table
//! with the columns its archive carries), a Redis key family to [`CLASSES`], a Redis layout family
//! to [`REDIS_FAMILIES`], and a bucket role to [`CLASSES`]. A backed-up table or bucket role also
//! needs its backup component in thoradm's `COMPONENTS`, and thoradm fails to build until it has
//! one. The completeness tests fail on a declared object, layout family, or bucket role without an
//! entry, and on an entry naming an object that doesn't exist.

use serde::{Deserialize, Serialize};

use crate::models::upgrades::SchemaBackend;

/// What a layout change of a stored object needs
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DataClass {
    /// Precious data, backed up: a layout change needs a `Data` step with a migration and an
    /// approval
    Core,
    /// Data rebuilt from other data, which a restore rebuilds: a layout change needs a `Data` step
    /// that rebuilds it, with an approval
    Derived,
    /// Data where every write sets a TTL, so a layout change needs no step and old data ages out
    ///
    /// The developer who puts an object in this class promises that every write to it sets a TTL;
    /// nothing checks the promise. If it is wrong, old-layout data outlives its TTL.
    Expiring,
    /// Working state with no TTL guarantee: a layout change needs a `Data` step that purges or
    /// converts it, with an approval; nothing drains it on its own
    Runtime,
}

/// The store a class table entry describes
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Store {
    /// A Scylla table, by name
    ScyllaTable,
    /// A Scylla materialized view, by name
    ScyllaView,
    /// A Redis key family, as a pattern relative to the namespace
    Redis,
    /// An S3 bucket role from the config, optionally narrowed to key prefixes
    S3,
}

impl Store {
    /// Returns the store's name for humans
    #[must_use]
    pub fn label(self) -> &'static str {
        // name each store the way the admin guide does
        match self {
            Store::ScyllaTable => "Scylla table",
            Store::ScyllaView => "Scylla view",
            Store::Redis => "Redis",
            Store::S3 => "S3 bucket",
        }
    }

    /// Returns the schema backend the store belongs to
    #[must_use]
    pub fn backend(self) -> SchemaBackend {
        // tables and views are both Scylla objects
        match self {
            Store::ScyllaTable | Store::ScyllaView => SchemaBackend::Scylla,
            Store::Redis => SchemaBackend::Redis,
            Store::S3 => SchemaBackend::S3,
        }
    }
}

/// Whether `thoradm backup` carries an object
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BackupCoverage {
    /// Copied by a backup and written back by restore
    BackedUp,
    /// Deliberately not backed up
    Excluded {
        /// Why, in words an admin can read; shown in the restore preflight and the admin docs
        reason: &'static str,
    },
}

/// The columns of a backed-up table, split by whether its archive carries them
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ColumnCoverage {
    /// The columns the archive struct carries in the current format version
    pub archived: &'static [&'static str],
    /// The columns it doesn't carry, each with the reason
    pub excluded: &'static [(&'static str, &'static str)],
}

/// One object Thorium stores: its data class and how backup and restore treat it
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ClassEntry {
    /// The store the object lives in
    pub store: Store,
    /// The table or view name, the Redis key pattern after `{ns}:`, or the bucket role
    pub name: &'static str,
    /// For an S3 entry, the key layouts its objects follow (the results bucket's result files and
    /// entity blobs); empty means the bucket's keys follow no layout thoradm reads
    pub prefixes: &'static [&'static str],
    /// For a view, its base table; restore drops and recreates the view with that table
    pub base_table: Option<&'static str>,
    /// For a backed-up table, its archived and excluded columns
    pub columns: Option<ColumnCoverage>,
    /// The object's data class
    pub class: DataClass,
    /// Whether a backup carries the object, and why not when it doesn't
    pub backup: BackupCoverage,
    /// A known gap in how today's backup or restore covers this object, stated until the change
    /// that closes it
    pub gap: Option<&'static str>,
    /// For a table or view, whether only keyspaces older releases created hold it: the API's setup
    /// doesn't create it, so thoradm acts on it where it exists and skips it elsewhere
    pub legacy: bool,
}

/// The data class of one Redis layout family, which a layout version bump of the family follows
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct FamilyClass {
    /// The family, as `LayoutDef::family` names it
    pub family: &'static str,
    /// The family's data class: the most demanding class among the keys its layout covers
    pub class: DataClass,
}

/// Builds a backed-up table entry
///
/// # Arguments
///
/// * `name` - The table name
/// * `archived` - The columns its archive carries
/// * `excluded` - The columns it doesn't carry, with reasons
/// * `gap` - A known gap in its coverage
const fn table(
    name: &'static str,
    archived: &'static [&'static str],
    excluded: &'static [(&'static str, &'static str)],
    gap: Option<&'static str>,
) -> ClassEntry {
    // a backed-up table is core data with its column split
    ClassEntry {
        store: Store::ScyllaTable,
        name,
        prefixes: &[],
        base_table: None,
        columns: Some(ColumnCoverage { archived, excluded }),
        class: DataClass::Core,
        backup: BackupCoverage::BackedUp,
        gap,
        legacy: false,
    }
}

/// Builds a view entry, backed up through its base table
///
/// # Arguments
///
/// * `name` - The view name
/// * `base` - The base table
const fn view(name: &'static str, base: &'static str) -> ClassEntry {
    // a view is derived from its base table and rebuilt from it, so it is covered the way the
    // base table is
    ClassEntry {
        store: Store::ScyllaView,
        name,
        prefixes: &[],
        base_table: Some(base),
        columns: None,
        class: DataClass::Derived,
        backup: BackupCoverage::BackedUp,
        gap: None,
        legacy: false,
    }
}

/// Builds an entry for a view only keyspaces older releases created hold, backed up through its
/// base table where it exists
///
/// # Arguments
///
/// * `name` - The view name
/// * `base` - The base table
const fn legacy_view(name: &'static str, base: &'static str) -> ClassEntry {
    // a legacy view is derived data that restore and verification skip where it doesn't exist; it
    // carries the class its `LEGACY` entry gives it
    ClassEntry {
        legacy: true,
        class: DataClass::Derived,
        ..view(name, base)
    }
}

/// Builds an entry for a Redis family or bucket role that a backup carries
///
/// # Arguments
///
/// * `store` - The store the object lives in
/// * `name` - The Redis pattern or bucket role
/// * `gap` - A known gap in its coverage
const fn backed_up(store: Store, name: &'static str, gap: Option<&'static str>) -> ClassEntry {
    // backed-up Redis families and buckets are core data
    ClassEntry {
        store,
        name,
        prefixes: &[],
        base_table: None,
        columns: None,
        class: DataClass::Core,
        backup: BackupCoverage::BackedUp,
        gap,
        legacy: false,
    }
}

/// Builds an entry for an object that isn't backed up
///
/// # Arguments
///
/// * `store` - The store the object lives in
/// * `name` - The table name, Redis pattern, or bucket role
/// * `class` - The object's data class
/// * `reason` - Why it isn't backed up
/// * `gap` - A known gap, when the object should be backed up and isn't yet
const fn excluded(
    store: Store,
    name: &'static str,
    class: DataClass,
    reason: &'static str,
    gap: Option<&'static str>,
) -> ClassEntry {
    // an excluded object carries the reason it is left out
    ClassEntry {
        store,
        name,
        prefixes: &[],
        base_table: None,
        columns: None,
        class,
        backup: BackupCoverage::Excluded { reason },
        gap,
        legacy: false,
    }
}

/// The reason runtime Redis state of reactions and jobs isn't backed up
const RUNTIME: &str =
    "runtime state of reactions, jobs, workers, or services, invalid after a restore";

/// Every object Thorium stores, in the order the backup coverage page lists them; the
/// completeness tests fail on anything not listed here
pub const CLASSES: &[ClassEntry] = &[
    // Scylla tables that are backed up
    table(
        "samples_list",
        &[
            "group",
            "year",
            "bucket",
            "sha256",
            "sha1",
            "md5",
            "id",
            "name",
            "description",
            "submitter",
            "origin",
            "uploaded",
        ],
        &[],
        None,
    ),
    table("s3_ids", &["type", "id", "sha256"], &[], None),
    table(
        "comments",
        &[
            "group", "sha256", "uploaded", "id", "author", "comment", "files",
        ],
        &[],
        None,
    ),
    table(
        "results",
        &[
            "id",
            "tool",
            "cmd",
            "uploaded",
            "result",
            "files",
            "display_type",
            "children",
            "tool_version",
            "entities",
        ],
        &[],
        None,
    ),
    table(
        "results_stream",
        &[
            "kind",
            "group",
            "year",
            "bucket",
            "key",
            "tool",
            "display_type",
            "uploaded",
            "cmd",
            "id",
            "tool_version",
        ],
        &[],
        None,
    ),
    table(
        "tags",
        &[
            "type", "group", "year", "bucket", "key", "value", "uploaded", "item",
        ],
        &[
            ("key_lower", "recomputed from key on restore"),
            ("value_lower", "recomputed from value on restore"),
        ],
        None,
    ),
    table("repo_data", &["repo", "hash"], &[], None),
    table(
        "repos_list",
        &[
            "group",
            "year",
            "bucket",
            "uploaded",
            "id",
            "creator",
            "name",
            "provider",
            "scheme",
            "url",
            "user",
            "default_checkout",
            "earliest",
        ],
        &[],
        None,
    ),
    table(
        "commitishes",
        &[
            "kind",
            "group",
            "repo",
            "key",
            "data",
            "repo_data",
            "timestamp",
        ],
        &[],
        None,
    ),
    table(
        "commitish_list",
        &[
            "kind",
            "group",
            "year",
            "bucket",
            "repo",
            "timestamp",
            "key",
            "repo_data",
        ],
        &[],
        None,
    ),
    table(
        "network_policies",
        &[
            "group",
            "name",
            "id",
            "k8s_name",
            "created",
            "ingress",
            "egress",
            "forced_policy",
            "default_policy",
        ],
        &[],
        None,
    ),
    table(
        "entities",
        &[
            "kind",
            "group",
            "year",
            "bucket",
            "created",
            "id",
            "name",
            "submitter",
            "kind_data",
            "description",
            "image",
        ],
        &[],
        None,
    ),
    table(
        "associations",
        &[
            "group",
            "year",
            "bucket",
            "created",
            "direction",
            "kind",
            "source",
            "target",
            "submitter",
            "extra_source",
            "extra_target",
            "submissions_source",
            "submissions_other",
        ],
        &[],
        None,
    ),
    table(
        "notifications",
        &["kind", "key", "created", "id", "msg", "level", "ban_id"],
        &[],
        None,
    ),
    table(
        "nodes",
        &["cluster", "node", "health", "heart_beat", "resources"],
        &[],
        None,
    ),
    // Scylla tables that are not backed up
    excluded(
        Store::ScyllaTable,
        "logs",
        DataClass::Runtime,
        "stage logs of reactions, which are runtime state and expire by TTL",
        None,
    ),
    ClassEntry {
        legacy: true,
        ..excluded(
            Store::ScyllaTable,
            "events",
            DataClass::Runtime,
            "a table of trigger events nothing reads or writes",
            None,
        )
    },
    excluded(
        Store::ScyllaTable,
        "schema_migrations",
        DataClass::Core,
        "the deployment's schema record: its applied fingerprints, layout versions, revision and \
         applied history (one row per entry, uncapped), and the settings that shape stored data; \
         carried in the backup's schema record, `schema.json`, instead of a table archive, and \
         written back by restore, which keeps the restoring cluster's data-shaping settings and \
         leaves out the Job results, which expire",
        None,
    ),
    excluded(
        Store::ScyllaTable,
        "schema_migration_issues",
        DataClass::Core,
        "the checkpoints and issue groups of data migration passes; carried in the backup's \
         schema record, `schema.json`, instead of a table archive, and written back by restore",
        None,
    ),
    // Scylla views, rebuilt from their base tables
    view("samples", "samples_list"),
    view("s3_sha256s", "s3_ids"),
    view("comments_by_id", "comments"),
    view("results_auth", "results_stream"),
    view("results_ids", "results_stream"),
    view("tags_by_item", "tags"),
    view("tags_case_insensitive", "tags"),
    view("repos", "repos_list"),
    view("committed_repo_data", "commitish_list"),
    view("network_policies_by_name", "network_policies"),
    view("network_policies_default", "network_policies"),
    view("entities_by_id", "entities"),
    legacy_view("entities_by_name", "entities"),
    // Redis families in the Redis backup
    backed_up(Store::Redis, "users", None),
    backed_up(Store::Redis, "user_data:*", None),
    backed_up(Store::Redis, "user_groups:*", None),
    backed_up(Store::Redis, "users_token_map", None),
    backed_up(Store::Redis, "users_email_map", None),
    backed_up(Store::Redis, "oauth:alias_to_user:*", None),
    backed_up(Store::Redis, "analysts", None),
    backed_up(Store::Redis, "groups", None),
    backed_up(Store::Redis, "groups_data:*", None),
    backed_up(Store::Redis, "groups_direct:*", None),
    backed_up(Store::Redis, "groups_metagroups:*", None),
    backed_up(Store::Redis, "groups_combined:*", None),
    backed_up(Store::Redis, "images:*", None),
    backed_up(Store::Redis, "image_data:*", None),
    backed_up(Store::Redis, "image_used_by:*", None),
    backed_up(Store::Redis, "pipelines:*", None),
    backed_up(Store::Redis, "pipeline_data:*", None),
    backed_up(Store::Redis, "system_settings", None),
    backed_up(Store::Redis, "scoped_token_map", None),
    backed_up(Store::Redis, "scoped_tokens:*", None),
    backed_up(Store::Redis, "netpols:*", None),
    // Redis state rebuilt from other data
    excluded(
        Store::Redis,
        "netpol_used_by:*",
        DataClass::Derived,
        "rebuilt by the Redis restore from the restored images' network policies",
        None,
    ),
    excluded(
        Store::Redis,
        "census:*",
        DataClass::Derived,
        "counts rebuilt from the Scylla tables by thoradm census, which restore runs",
        None,
    ),
    excluded(
        Store::Redis,
        "system_info",
        DataClass::Derived,
        "re-initialized by restore",
        None,
    ),
    excluded(
        Store::Redis,
        "system_keys",
        DataClass::Runtime,
        "written at setup and read by nothing",
        None,
    ),
    // Redis runtime state
    excluded(
        Store::Redis,
        "reactions:*",
        DataClass::Runtime,
        RUNTIME,
        None,
    ),
    excluded(
        Store::Redis,
        "reactions_group:*",
        DataClass::Runtime,
        RUNTIME,
        None,
    ),
    excluded(
        Store::Redis,
        "reaction_data:*",
        DataClass::Runtime,
        RUNTIME,
        None,
    ),
    excluded(
        Store::Redis,
        "reaction_jobs:*",
        DataClass::Runtime,
        RUNTIME,
        None,
    ),
    excluded(
        Store::Redis,
        "reaction_tags:*",
        DataClass::Runtime,
        RUNTIME,
        None,
    ),
    excluded(
        Store::Redis,
        "reaction_cache:*",
        DataClass::Runtime,
        RUNTIME,
        None,
    ),
    excluded(
        Store::Redis,
        "sub_reactions:*",
        DataClass::Runtime,
        RUNTIME,
        None,
    ),
    excluded(
        Store::Redis,
        "generators:*",
        DataClass::Runtime,
        RUNTIME,
        None,
    ),
    excluded(
        Store::Redis,
        "stage_logs:*",
        DataClass::Runtime,
        RUNTIME,
        None,
    ),
    excluded(Store::Redis, "logs:*", DataClass::Runtime, RUNTIME, None),
    excluded(
        Store::Redis,
        "job_queue:*",
        DataClass::Runtime,
        RUNTIME,
        None,
    ),
    excluded(
        Store::Redis,
        "job_data:*",
        DataClass::Runtime,
        RUNTIME,
        None,
    ),
    excluded(Store::Redis, "workers:*", DataClass::Runtime, RUNTIME, None),
    excluded(
        Store::Redis,
        "worker_data:*",
        DataClass::Runtime,
        RUNTIME,
        None,
    ),
    excluded(Store::Redis, "streams:*", DataClass::Runtime, RUNTIME, None),
    excluded(
        Store::Redis,
        "event-handler:*",
        DataClass::Runtime,
        RUNTIME,
        None,
    ),
    excluded(
        Store::Redis,
        "search-streamer:*",
        DataClass::Runtime,
        RUNTIME,
        None,
    ),
    excluded(
        Store::Redis,
        "cursors:*",
        DataClass::Expiring,
        RUNTIME,
        None,
    ),
    excluded(
        Store::Redis,
        "schema-writer",
        DataClass::Runtime,
        "the namespace's writer lease, held only while a writing thoradm command runs",
        None,
    ),
    excluded(
        Store::Redis,
        "rebucket:*",
        DataClass::Runtime,
        "the progress of a census rebuild, valid only for the run that wrote it",
        None,
    ),
    excluded(
        Store::Redis,
        "image_ttc:*",
        DataClass::Runtime,
        RUNTIME,
        None,
    ),
    excluded(
        Store::Redis,
        "oauth:context:*",
        DataClass::Expiring,
        "OAuth login contexts, which expire",
        None,
    ),
    excluded(
        Store::Redis,
        "oauth:registration_sessions:*",
        DataClass::Expiring,
        "OAuth registration sessions, which expire",
        None,
    ),
    excluded(
        Store::Redis,
        "oauth:link_token:*",
        DataClass::Expiring,
        "OAuth account-link tokens, which expire",
        None,
    ),
    // S3 bucket roles
    backed_up(Store::S3, "files", None),
    backed_up(Store::S3, "repos", None),
    backed_up(Store::S3, "attachments", None),
    ClassEntry {
        store: Store::S3,
        name: "results",
        prefixes: &["{id}/{file}", "{id}/__thorium_entities/{kind}.json"],
        base_table: None,
        columns: None,
        backup: BackupCoverage::BackedUp,
        class: DataClass::Core,
        gap: None,
        legacy: false,
    },
    backed_up(Store::S3, "graphics", None),
    excluded(
        Store::S3,
        "ephemeral",
        DataClass::Runtime,
        "per-reaction scratch files",
        None,
    ),
    excluded(
        Store::S3,
        "reaction_cache",
        DataClass::Runtime,
        "per-reaction cache files",
        None,
    ),
];

/// Builds the class of one Redis layout family
///
/// # Arguments
///
/// * `family` - The family, as `LayoutDef::family` names it
/// * `class` - Its data class
const fn family(family: &'static str, class: DataClass) -> FamilyClass {
    // pair the family with its class
    FamilyClass { family, class }
}

/// The data class of every Redis layout family in `LAYOUTS`, in the same order
///
/// A family's layout spans the key families its keys use, so its class is the most demanding
/// class among them: a family holding any core key is `Core`.
pub const REDIS_FAMILIES: &[FamilyClass] = &[
    family("users", DataClass::Core),
    family("groups", DataClass::Core),
    family("images", DataClass::Core),
    family("pipelines", DataClass::Core),
    family("network_policies", DataClass::Core),
    family("jobs", DataClass::Runtime),
    family("reactions", DataClass::Runtime),
    family("streams", DataClass::Runtime),
    family("system", DataClass::Core),
    family("workers", DataClass::Runtime),
    family("events", DataClass::Runtime),
    family("search_events", DataClass::Runtime),
    family("search_streamer_init", DataClass::Runtime),
    family("oauth", DataClass::Core),
    family("vendors", DataClass::Core),
    family("cursors", DataClass::Expiring),
    family("census_samples", DataClass::Derived),
    family("census_repos", DataClass::Derived),
    family("census_commitish", DataClass::Derived),
    family("census_tags", DataClass::Derived),
    family("census_tags_case_insensitive", DataClass::Derived),
    family("census_entities", DataClass::Derived),
    family("census_associations", DataClass::Derived),
];

impl DataClass {
    /// Returns the class the class table gives a schema object, if it lists the object
    ///
    /// Scylla tables and views and S3 bucket roles are looked up in [`CLASSES`], Redis layout
    /// families in [`REDIS_FAMILIES`]; Elastic indexes aren't classified.
    ///
    /// # Arguments
    ///
    /// * `backend` - The object's backend
    /// * `name` - The table or view name, the Redis layout family, or the bucket role
    #[must_use]
    pub fn of(backend: SchemaBackend, name: &str) -> Option<DataClass> {
        // pick the list that classifies the backend's objects
        match backend {
            SchemaBackend::Scylla | SchemaBackend::S3 => CLASSES
                .iter()
                .find(|entry| entry.store.backend() == backend && entry.name == name)
                .map(|entry| entry.class),
            SchemaBackend::Redis => REDIS_FAMILIES
                .iter()
                .find(|entry| entry.family == name)
                .map(|entry| entry.class),
            SchemaBackend::Elastic | SchemaBackend::Unknown => None,
        }
    }
}
