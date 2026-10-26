//! Every data migration thoradm knows about
//!
//! A new migration is appended to [`MIGRATIONS`]; every `thoradm_migration` a catalog step names
//! must be registered here. Every migration runs only through the runner: its entry names the
//! [`MigrationRunner`] that reads, converts, and writes its objects, and the runner supplies the
//! slices, checkpoints, counts, and issue recording.

use super::elastic_rebuild::ElasticRebuild;
use async_trait::async_trait;
use std::fmt;
use thorium::models::schema::{
    self as schema, Batch, MigrateError, MigrationRunner, Outcome, RunContext, RunMode, Slice,
};
use thorium::models::upgrades::Record;

/// The backend a data migration changes
#[derive(Debug, Clone, Copy)]
pub enum Backend {
    /// Scylla tables
    Scylla,
    /// Elasticsearch
    Elastic,
    /// Redis families
    Redis,
    /// S3 buckets
    S3,
}

/// What thoradm can do with a registered migration
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Support {
    /// Nothing yet: the migration is performed by hand, and every command reports its manual
    /// procedure
    No,
    /// `migrate check` classifies what it would touch, while `run` and `verify` refuse, so a
    /// catalog can show an admin the scope of a step whose run isn't built yet
    CheckOnly,
    /// `check`, `run`, and `verify` all work
    Yes,
}

/// A data migration thoradm knows about
#[derive(Clone, Copy)]
pub struct MigrationDef {
    /// The id of this migration, referenced by catalog steps
    pub id: &'static str,
    /// The backend this migration changes
    pub backend: Backend,
    /// What thoradm can do with this migration
    pub support: Support,
    /// The runner every run, check, and verification of this migration goes through
    pub runner: &'static dyn MigrationRunner,
    /// The pass key: the family a declared transform rewrites, or this migration's id; the pass
    /// row's id and the partition of its issue rows
    pub pass: &'static str,
    /// The step ids of the transforms a pass of this migration runs, in order
    pub chain: &'static [&'static str],
    /// The group limit when no catalog step sets one for this migration
    pub group_max: u64,
    /// The families the migration covers, named in its check report
    pub families: &'static [&'static str],
    /// Whether a table the migration writes has views, which lowers its write rate
    pub has_views: bool,
}

impl fmt::Debug for MigrationDef {
    /// Write the migration's identity without its runner
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // the runner is a trait object, so only the plain fields are shown
        f.debug_struct("MigrationDef")
            .field("id", &self.id)
            .field("backend", &self.backend)
            .field("support", &self.support)
            .field("pass", &self.pass)
            .finish_non_exhaustive()
    }
}

/// The runner of a migration thoradm can't perform yet; every method says so
pub struct Unbuilt;

#[async_trait]
impl MigrationRunner for Unbuilt {
    /// Say the migration isn't built
    ///
    /// # Arguments
    ///
    /// * `n` - How many slices were asked for
    /// * `ctx` - The run's context
    async fn slices(&self, n: u32, ctx: &RunContext) -> Result<Vec<Slice>, MigrateError> {
        // nothing can be split until the migration is built
        let _ = (n, ctx);
        Err(MigrateError::NotImplemented(
            "this migration isn't built".to_owned(),
        ))
    }

    /// Say the migration isn't built
    ///
    /// # Arguments
    ///
    /// * `slice` - The slice to read
    /// * `after` - The last position done
    /// * `ctx` - The run's context
    async fn next_batch(
        &self,
        slice: &Slice,
        after: &serde_json::Value,
        ctx: &RunContext,
    ) -> Result<Batch, MigrateError> {
        // nothing can be read until the migration is built
        let _ = (slice, after, ctx);
        Err(MigrateError::NotImplemented(
            "this migration isn't built".to_owned(),
        ))
    }

    /// Say the migration isn't built
    ///
    /// # Arguments
    ///
    /// * `record` - The stored record
    /// * `mode` - `Run` or `Check`
    /// * `ctx` - The run's context
    async fn process(
        &self,
        record: &Record,
        mode: RunMode,
        ctx: &RunContext,
    ) -> Result<Outcome, MigrateError> {
        // nothing can be classified until the migration is built
        let _ = (record, mode, ctx);
        Err(MigrateError::NotImplemented(
            "this migration isn't built".to_owned(),
        ))
    }

    /// Say the migration isn't built
    ///
    /// # Arguments
    ///
    /// * `ctx` - The run's context
    async fn finish(&self, ctx: &RunContext) -> Result<(), MigrateError> {
        // nothing can be finished until the migration is built
        let _ = ctx;
        Err(MigrateError::NotImplemented(
            "this migration isn't built".to_owned(),
        ))
    }
}

/// Drop the Elastic indexes whose mappings differ from the declared ones and the init session keys of
/// their sources, so the search-streamer recreates and reingests them
const ELASTIC_REBUILD: MigrationDef = MigrationDef {
    id: schema::REBUILD_MIGRATION,
    backend: Backend::Elastic,
    support: Support::Yes,
    runner: &ElasticRebuild,
    pass: schema::REBUILD_MIGRATION,
    chain: &[schema::REBUILD_MIGRATION],
    group_max: 5,
    families: &["elastic indexes"],
    has_views: false,
};

/// A `rebucket-<data type>` migration, which the operator plans itself when a partition size
/// changes while rows of the data type exist
///
/// # Arguments
///
/// * `id` - The migration's id
/// * `runner` - The migration's runner
/// * `families` - The families it covers
macro_rules! rebucket {
    ($id:literal, $runner:expr, $families:expr) => {
        MigrationDef {
            id: $id,
            backend: Backend::Scylla,
            support: Support::Yes,
            runner: &$runner,
            pass: $id,
            chain: &[$id],
            group_max: 5,
            families: $families,
            has_views: true,
        }
    };
}

/// Move files to the bucket the configured size gives, and rebuild their census
const REBUCKET_FILES: MigrationDef = rebucket!("rebucket-files", schema::FILES, &["files"]);

/// Move repos and their commitishes, and rebuild their census
const REBUCKET_REPOS: MigrationDef =
    rebucket!("rebucket-repos", schema::REPOS, &["repos", "commitishes"]);

/// Move results
const REBUCKET_RESULTS: MigrationDef = rebucket!("rebucket-results", schema::RESULTS, &["results"]);

/// Move entities, and rebuild their census
const REBUCKET_ENTITIES: MigrationDef =
    rebucket!("rebucket-entities", schema::ENTITIES, &["entities"]);

/// Move associations, and rebuild their census for both sides
const REBUCKET_ASSOCIATIONS: MigrationDef = rebucket!(
    "rebucket-associations",
    schema::ASSOCIATIONS,
    &["associations"]
);

/// Move tags of every tag type, and rebuild their census
const REBUCKET_TAGS: MigrationDef = rebucket!("rebucket-tags", schema::TAGS, &["tags"]);

/// Move each user's theme from its own field into the user's settings
///
/// The pass key is the users family the step's transform rewrites, which names the pass row and
/// the partition of its issue rows.
const USERS_SETTINGS_THEME: MigrationDef = MigrationDef {
    id: "users-settings-theme",
    backend: Backend::Redis,
    support: Support::Yes,
    runner: &schema::USERS_THEME_UP,
    pass: "users",
    chain: &["users-settings-theme"],
    group_max: 0,
    families: &["users"],
    has_views: false,
};

/// Leave every user in the layout the first Helm revision reads when going back to it from the
/// revision after it: users in layout 2 are skipped and a user stored with a flat theme moves its
/// theme into its settings
const USERS_SETTINGS_THEME_REVERSE: MigrationDef = MigrationDef {
    id: "users-settings-theme-reverse",
    backend: Backend::Redis,
    support: Support::Yes,
    runner: &schema::USERS_THEME_REVERSE,
    pass: "users-settings-theme-reverse",
    chain: &["users-settings-theme-reverse"],
    group_max: 0,
    families: &["users"],
    has_views: false,
};

/// The migrations that move rows to the buckets a changed partition size gives
const REBUCKETS: [MigrationDef; 6] = [
    REBUCKET_FILES,
    REBUCKET_REPOS,
    REBUCKET_RESULTS,
    REBUCKET_ENTITIES,
    REBUCKET_ASSOCIATIONS,
    REBUCKET_TAGS,
];

/// Every data migration thoradm knows about, appended in the order they were added
#[cfg(not(feature = "test-support"))]
pub const MIGRATIONS: &[MigrationDef] = &[
    ELASTIC_REBUILD,
    REBUCKETS[0],
    REBUCKETS[1],
    REBUCKETS[2],
    REBUCKETS[3],
    REBUCKETS[4],
    REBUCKETS[5],
    USERS_SETTINGS_THEME,
    USERS_SETTINGS_THEME_REVERSE,
    super::images_to_tools::IMAGES_TO_TOOLS,
    super::pipelines_to_tool_stages::PIPELINES_TO_TOOL_STAGES,
    super::provenance_to_legacy_tools::PROVENANCE_TO_LEGACY_TOOLS,
    super::drain::DRAIN_REACTIONS,
];

/// Every data migration thoradm knows about, appended in the order they were added, followed by
/// the fixture migrations thoradm's own tests run
#[cfg(feature = "test-support")]
pub const MIGRATIONS: &[MigrationDef] = &[
    ELASTIC_REBUILD,
    REBUCKETS[0],
    REBUCKETS[1],
    REBUCKETS[2],
    REBUCKETS[3],
    REBUCKETS[4],
    REBUCKETS[5],
    USERS_SETTINGS_THEME,
    USERS_SETTINGS_THEME_REVERSE,
    super::images_to_tools::IMAGES_TO_TOOLS,
    super::pipelines_to_tool_stages::PIPELINES_TO_TOOL_STAGES,
    super::provenance_to_legacy_tools::PROVENANCE_TO_LEGACY_TOOLS,
    super::drain::DRAIN_REACTIONS,
    super::fixture::THEME,
    super::fixture::STRICT,
    super::fixture::CHAIN,
    super::fixture::VIEWS,
    super::fixture::PLAIN,
    super::fixture::WAIT,
    super::fixture::CHECKED,
    super::fixture::NOTES,
    super::fixture::LOSSY,
];
