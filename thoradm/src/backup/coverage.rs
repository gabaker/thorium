//! The coverage registry: every resource Thorium stores and how backup and restore treat it
//!
//! Each Scylla table and view, Redis key family, and S3 bucket role has one entry, classified as
//! backed up (by a named component) or excluded (with a reason an admin can read). The
//! completeness tests fail when Thorium stores something no entry classifies, and a known gap in
//! today's coverage is stated on its entry until the change that closes it.

use crate::args::BackupComponents;
use std::fmt::Write;

/// The store a registry entry describes
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
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
}

/// Whether a resource is part of what a restore must bring back
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Category {
    /// Data users created or depend on; backed up and restored
    Core,
    /// Data a restore rebuilds from other backed-up data (census streams, `system_info`)
    Derived,
    /// Runtime state that is invalid after a restore (reactions, jobs, logs, events, caches)
    Ephemeral,
}

impl Category {
    /// Returns the category's name for humans
    #[must_use]
    pub fn label(self) -> &'static str {
        // the admin guide writes categories in capitals
        match self {
            Category::Core => "CORE",
            Category::Derived => "DERIVED",
            Category::Ephemeral => "EPHEMERAL",
        }
    }
}

/// How the backup treats one resource
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Coverage {
    /// Copied by this backup component and written back by restore
    BackedUp(BackupComponents),
    /// Deliberately not backed up
    Excluded {
        /// Why, in words an admin can read; shown in the restore preflight and the admin docs
        reason: &'static str,
    },
}

/// The columns of a backed-up table, split by whether its archive carries them
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct ColumnCoverage {
    /// The columns the archive struct carries in the current format version
    pub archived: &'static [&'static str],
    /// The columns it doesn't carry, each with the reason
    pub excluded: &'static [(&'static str, &'static str)],
}

/// One resource Thorium stores, and how backup and restore treat it
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct CoverageEntry {
    /// The store the resource lives in
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
    /// Whether the resource is backed up, and by which component
    pub coverage: Coverage,
    /// The resource's category
    pub category: Category,
    /// A known gap in how today's backup or restore covers this resource, stated until the
    /// change that closes it
    pub gap: Option<&'static str>,
}

/// The columns format 2 added to the archives, by table; a format-1 backup doesn't carry them, so
/// its restore leaves them unset
pub const FORMAT_2_COLUMNS: &[(&str, &str)] = &[
    ("results", "tool_version"),
    ("results", "entities"),
    ("results_stream", "tool_version"),
    ("repos_list", "default_checkout"),
    ("repos_list", "earliest"),
    ("associations", "submissions_source"),
    ("associations", "submissions_other"),
];

/// Builds a backed-up table entry
///
/// # Arguments
///
/// * `name` - The table name
/// * `component` - The component that backs it up
/// * `archived` - The columns its archive carries
/// * `excluded` - The columns it doesn't carry, with reasons
/// * `gap` - A known gap in its coverage
const fn table(
    name: &'static str,
    component: BackupComponents,
    archived: &'static [&'static str],
    excluded: &'static [(&'static str, &'static str)],
    gap: Option<&'static str>,
) -> CoverageEntry {
    // a backed-up table is core data with its column split
    CoverageEntry {
        store: Store::ScyllaTable,
        name,
        prefixes: &[],
        base_table: None,
        columns: Some(ColumnCoverage { archived, excluded }),
        coverage: Coverage::BackedUp(component),
        category: Category::Core,
        gap,
    }
}

/// Builds a view entry, backed up through its base table
///
/// # Arguments
///
/// * `name` - The view name
/// * `base` - The base table
/// * `component` - The component that backs up the base table
const fn view(
    name: &'static str,
    base: &'static str,
    component: BackupComponents,
) -> CoverageEntry {
    // a view is rebuilt from its base table, so it shares the base table's component
    CoverageEntry {
        store: Store::ScyllaView,
        name,
        prefixes: &[],
        base_table: Some(base),
        columns: None,
        coverage: Coverage::BackedUp(component),
        category: Category::Core,
        gap: None,
    }
}

/// Builds an entry for a resource backed up by a component
///
/// # Arguments
///
/// * `store` - The store the resource lives in
/// * `name` - The Redis pattern or bucket role
/// * `component` - The component that backs it up
/// * `gap` - A known gap in its coverage
const fn backed_up(
    store: Store,
    name: &'static str,
    component: BackupComponents,
    gap: Option<&'static str>,
) -> CoverageEntry {
    // backed-up Redis families and buckets are core data
    CoverageEntry {
        store,
        name,
        prefixes: &[],
        base_table: None,
        columns: None,
        coverage: Coverage::BackedUp(component),
        category: Category::Core,
        gap,
    }
}

/// Builds an entry for a resource that isn't backed up
///
/// # Arguments
///
/// * `store` - The store the resource lives in
/// * `name` - The table name, Redis pattern, or bucket role
/// * `category` - The resource's category
/// * `reason` - Why it isn't backed up
/// * `gap` - A known gap, when the resource should be backed up and isn't yet
const fn excluded(
    store: Store,
    name: &'static str,
    category: Category,
    reason: &'static str,
    gap: Option<&'static str>,
) -> CoverageEntry {
    // an excluded resource has no component
    CoverageEntry {
        store,
        name,
        prefixes: &[],
        base_table: None,
        columns: None,
        coverage: Coverage::Excluded { reason },
        category,
        gap,
    }
}

/// The reason runtime Redis state of reactions and jobs isn't backed up
const RUNTIME: &str =
    "runtime state of reactions, jobs, workers, or services, invalid after a restore";

/// Every resource Thorium stores; the completeness tests fail on anything not listed here
pub const COVERAGE: &[CoverageEntry] = &[
    // Scylla tables that are backed up
    table(
        "samples_list",
        BackupComponents::SamplesList,
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
    table(
        "s3_ids",
        BackupComponents::S3Ids,
        &["type", "id", "sha256"],
        &[],
        None,
    ),
    table(
        "comments",
        BackupComponents::Comments,
        &[
            "group", "sha256", "uploaded", "id", "author", "comment", "files",
        ],
        &[],
        None,
    ),
    table(
        "results",
        BackupComponents::Results,
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
        BackupComponents::ResultsStream,
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
        BackupComponents::Tags,
        &[
            "type", "group", "year", "bucket", "key", "value", "uploaded", "item",
        ],
        &[
            ("key_lower", "recomputed from key on restore"),
            ("value_lower", "recomputed from value on restore"),
        ],
        None,
    ),
    table(
        "repo_data",
        BackupComponents::RepoData,
        &["repo", "hash"],
        &[],
        None,
    ),
    table(
        "repos_list",
        BackupComponents::RepoList,
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
        BackupComponents::Commitish,
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
        BackupComponents::CommitishList,
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
        BackupComponents::NetworkPolicies,
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
        BackupComponents::Entities,
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
        BackupComponents::Associations,
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
        BackupComponents::Notifications,
        &["kind", "key", "created", "id", "msg", "level", "ban_id"],
        &[],
        None,
    ),
    table(
        "nodes",
        BackupComponents::Nodes,
        &["cluster", "node", "health", "heart_beat", "resources"],
        &[],
        None,
    ),
    // Scylla tables that are not backed up
    excluded(
        Store::ScyllaTable,
        "logs",
        Category::Ephemeral,
        "stage logs of reactions, which are runtime state and expire by TTL",
        None,
    ),
    excluded(
        Store::ScyllaTable,
        "events",
        Category::Ephemeral,
        "trigger events consumed by the event handler, which expire by TTL",
        None,
    ),
    // Scylla views, rebuilt from their base tables
    view("samples", "samples_list", BackupComponents::SamplesList),
    view("s3_sha256s", "s3_ids", BackupComponents::S3Ids),
    view("comments_by_id", "comments", BackupComponents::Comments),
    view(
        "results_auth",
        "results_stream",
        BackupComponents::ResultsStream,
    ),
    view(
        "results_ids",
        "results_stream",
        BackupComponents::ResultsStream,
    ),
    view("tags_by_item", "tags", BackupComponents::Tags),
    view("tags_case_insensitive", "tags", BackupComponents::Tags),
    view("repos", "repos_list", BackupComponents::RepoList),
    view(
        "committed_repo_data",
        "commitish_list",
        BackupComponents::CommitishList,
    ),
    view(
        "network_policies_by_name",
        "network_policies",
        BackupComponents::NetworkPolicies,
    ),
    view(
        "network_policies_default",
        "network_policies",
        BackupComponents::NetworkPolicies,
    ),
    view("entities_by_id", "entities", BackupComponents::Entities),
    view("entities_by_name", "entities", BackupComponents::Entities),
    // Redis families in the Redis backup
    backed_up(Store::Redis, "users", BackupComponents::Redis, None),
    backed_up(Store::Redis, "user_data:*", BackupComponents::Redis, None),
    backed_up(Store::Redis, "user_groups:*", BackupComponents::Redis, None),
    backed_up(
        Store::Redis,
        "users_token_map",
        BackupComponents::Redis,
        None,
    ),
    backed_up(
        Store::Redis,
        "users_email_map",
        BackupComponents::Redis,
        None,
    ),
    backed_up(
        Store::Redis,
        "oauth:alias_to_user:*",
        BackupComponents::Redis,
        None,
    ),
    backed_up(Store::Redis, "analysts", BackupComponents::Redis, None),
    backed_up(Store::Redis, "groups", BackupComponents::Redis, None),
    backed_up(Store::Redis, "groups_data:*", BackupComponents::Redis, None),
    backed_up(
        Store::Redis,
        "groups_direct:*",
        BackupComponents::Redis,
        None,
    ),
    backed_up(
        Store::Redis,
        "groups_metagroups:*",
        BackupComponents::Redis,
        None,
    ),
    backed_up(
        Store::Redis,
        "groups_combined:*",
        BackupComponents::Redis,
        None,
    ),
    backed_up(Store::Redis, "images:*", BackupComponents::Redis, None),
    backed_up(Store::Redis, "image_data:*", BackupComponents::Redis, None),
    backed_up(
        Store::Redis,
        "image_used_by:*",
        BackupComponents::Redis,
        None,
    ),
    backed_up(Store::Redis, "pipelines:*", BackupComponents::Redis, None),
    backed_up(
        Store::Redis,
        "pipeline_data:*",
        BackupComponents::Redis,
        None,
    ),
    backed_up(
        Store::Redis,
        "system_settings",
        BackupComponents::Redis,
        None,
    ),
    backed_up(
        Store::Redis,
        "scoped_token_map",
        BackupComponents::Redis,
        None,
    ),
    backed_up(
        Store::Redis,
        "scoped_tokens:*",
        BackupComponents::Redis,
        None,
    ),
    backed_up(Store::Redis, "netpols:*", BackupComponents::Redis, None),
    // Redis state rebuilt from other data
    excluded(
        Store::Redis,
        "netpol_used_by:*",
        Category::Derived,
        "rebuilt by the Redis restore from the restored images' network policies",
        None,
    ),
    excluded(
        Store::Redis,
        "census:*",
        Category::Derived,
        "counts rebuilt from the Scylla tables by thoradm census, which restore runs",
        None,
    ),
    excluded(
        Store::Redis,
        "system_info",
        Category::Derived,
        "re-initialized by restore",
        None,
    ),
    excluded(
        Store::Redis,
        "system_keys",
        Category::Ephemeral,
        "written at setup and read by nothing",
        None,
    ),
    // Redis runtime state
    excluded(
        Store::Redis,
        "reactions:*",
        Category::Ephemeral,
        RUNTIME,
        None,
    ),
    excluded(
        Store::Redis,
        "reactions_group:*",
        Category::Ephemeral,
        RUNTIME,
        None,
    ),
    excluded(
        Store::Redis,
        "reaction_data:*",
        Category::Ephemeral,
        RUNTIME,
        None,
    ),
    excluded(
        Store::Redis,
        "reaction_jobs:*",
        Category::Ephemeral,
        RUNTIME,
        None,
    ),
    excluded(
        Store::Redis,
        "reaction_tags:*",
        Category::Ephemeral,
        RUNTIME,
        None,
    ),
    excluded(
        Store::Redis,
        "reaction_cache:*",
        Category::Ephemeral,
        RUNTIME,
        None,
    ),
    excluded(
        Store::Redis,
        "sub_reactions:*",
        Category::Ephemeral,
        RUNTIME,
        None,
    ),
    excluded(
        Store::Redis,
        "generators:*",
        Category::Ephemeral,
        RUNTIME,
        None,
    ),
    excluded(
        Store::Redis,
        "stage_logs:*",
        Category::Ephemeral,
        RUNTIME,
        None,
    ),
    excluded(Store::Redis, "logs:*", Category::Ephemeral, RUNTIME, None),
    excluded(
        Store::Redis,
        "job_queue:*",
        Category::Ephemeral,
        RUNTIME,
        None,
    ),
    excluded(
        Store::Redis,
        "job_data:*",
        Category::Ephemeral,
        RUNTIME,
        None,
    ),
    excluded(
        Store::Redis,
        "workers:*",
        Category::Ephemeral,
        RUNTIME,
        None,
    ),
    excluded(
        Store::Redis,
        "worker_data:*",
        Category::Ephemeral,
        RUNTIME,
        None,
    ),
    excluded(
        Store::Redis,
        "streams:*",
        Category::Ephemeral,
        RUNTIME,
        None,
    ),
    excluded(
        Store::Redis,
        "event-handler:*",
        Category::Ephemeral,
        RUNTIME,
        None,
    ),
    excluded(
        Store::Redis,
        "search-streamer:*",
        Category::Ephemeral,
        RUNTIME,
        None,
    ),
    excluded(
        Store::Redis,
        "cursors:*",
        Category::Ephemeral,
        RUNTIME,
        None,
    ),
    excluded(
        Store::Redis,
        "image_ttc:*",
        Category::Ephemeral,
        RUNTIME,
        None,
    ),
    excluded(
        Store::Redis,
        "oauth:context:*",
        Category::Ephemeral,
        "OAuth login contexts, which expire",
        None,
    ),
    excluded(
        Store::Redis,
        "oauth:registration_sessions:*",
        Category::Ephemeral,
        "OAuth registration sessions, which expire",
        None,
    ),
    excluded(
        Store::Redis,
        "oauth:link_token:*",
        Category::Ephemeral,
        "OAuth account-link tokens, which expire",
        None,
    ),
    // S3 bucket roles
    backed_up(Store::S3, "files", BackupComponents::S3IdsObjects, None),
    backed_up(Store::S3, "repos", BackupComponents::S3IdsObjects, None),
    backed_up(
        Store::S3,
        "attachments",
        BackupComponents::CommentAttachments,
        None,
    ),
    CoverageEntry {
        store: Store::S3,
        name: "results",
        prefixes: &["{id}/{file}", "{id}/__thorium_entities/{kind}.json"],
        base_table: None,
        columns: None,
        coverage: Coverage::BackedUp(BackupComponents::ResultFiles),
        category: Category::Core,
        gap: None,
    },
    backed_up(Store::S3, "graphics", BackupComponents::Graphics, None),
    excluded(
        Store::S3,
        "ephemeral",
        Category::Ephemeral,
        "per-reaction scratch files",
        None,
    ),
    excluded(
        Store::S3,
        "reaction_cache",
        Category::Ephemeral,
        "per-reaction cache files",
        None,
    ),
];

/// Returns whether a registry name or pattern matches a resource's name
///
/// A `*` in a pattern stands for one or more `:`-separated key segments; every other name must
/// match exactly.
///
/// # Arguments
///
/// * `pattern` - The registry entry's name or pattern
/// * `name` - The resource's name (a Redis key without its namespace)
#[must_use]
pub fn pattern_matches(pattern: &str, name: &str) -> bool {
    // without a wildcard the names must be equal
    let Some(prefix) = pattern.strip_suffix('*') else {
        return pattern == name;
    };
    // with one, the name must continue the prefix with at least one non-empty segment
    name.strip_prefix(prefix)
        .is_some_and(|rest| !rest.is_empty() && rest.split(':').all(|segment| !segment.is_empty()))
}

/// Lookups other code makes against the registry
pub struct Registry;

impl Registry {
    /// Returns the entry whose name or pattern matches a resource, if any
    ///
    /// # Arguments
    ///
    /// * `store` - The store the resource lives in
    /// * `name` - The table or view name, the Redis key without the namespace, or the bucket role
    #[must_use]
    pub fn classify(store: Store, name: &str) -> Option<&'static CoverageEntry> {
        // find the single entry whose name or key pattern matches
        COVERAGE
            .iter()
            .find(|entry| entry.store == store && pattern_matches(entry.name, name))
    }

    /// Returns every entry whose name or pattern matches a resource
    ///
    /// # Arguments
    ///
    /// * `store` - The store the resource lives in
    /// * `name` - The table or view name, the Redis key without the namespace, or the bucket role
    pub fn matches(store: Store, name: &str) -> impl Iterator<Item = &'static CoverageEntry> + '_ {
        // keep every entry of the store that matches
        COVERAGE
            .iter()
            .filter(move |entry| entry.store == store && pattern_matches(entry.name, name))
    }

    /// Returns whether a column of a table is carried by today's backup archives
    ///
    /// # Arguments
    ///
    /// * `table` - The Scylla table name
    /// * `column` - The column name
    #[must_use]
    pub fn column_backed_up(table: &str, column: &str) -> bool {
        // an unknown or excluded table backs up none of its columns
        let Some(entry) = Self::classify(Store::ScyllaTable, table) else {
            return false;
        };
        // a backed-up table backs up exactly its archived columns
        match (entry.coverage, entry.columns) {
            (Coverage::BackedUp(_), Some(columns)) => columns.archived.contains(&column),
            _ => false,
        }
    }

    /// Returns the views restore drops and recreates for a table
    ///
    /// # Arguments
    ///
    /// * `table` - The base table being restored
    pub fn views_of(table: &str) -> impl Iterator<Item = &'static str> + '_ {
        // every view entry whose base is this table
        COVERAGE
            .iter()
            .filter(move |entry| {
                entry.store == Store::ScyllaView && entry.base_table == Some(table)
            })
            .map(|entry| entry.name)
    }

    /// Returns every excluded entry, for the restore preflight and the admin docs
    pub fn excluded() -> impl Iterator<Item = &'static CoverageEntry> {
        // keep only the entries marked excluded
        COVERAGE
            .iter()
            .filter(|entry| matches!(entry.coverage, Coverage::Excluded { .. }))
    }

    /// Returns every backed-up entry of a store
    ///
    /// # Arguments
    ///
    /// * `store` - The store to list
    pub fn backed_up(store: Store) -> impl Iterator<Item = &'static CoverageEntry> {
        // keep the store's entries a component backs up
        COVERAGE.iter().filter(move |entry| {
            entry.store == store && matches!(entry.coverage, Coverage::BackedUp(_))
        })
    }

    /// Renders the registry as the Markdown coverage page, one row per entry in registry order
    #[must_use]
    pub fn render_page() -> String {
        // open the page with its explanation and table header
        let mut page = String::from(
            "# Backup Coverage\n\nEvery resource Thorium stores and how `thoradm backup` treats it. \
             Generated from thoradm's coverage registry by `cargo run -p thoradm --example coverage_page`.\n\n\
             | Store | Resource | Classification | Component or reason | Category | Known gap |\n\
             |---|---|---|---|---|---|\n",
        );
        // one row per entry, in registry order
        for entry in COVERAGE {
            // name the resource, with its prefixes or base table where they matter
            let mut resource = format!("`{}`", entry.name);
            if !entry.prefixes.is_empty() {
                let prefixes: Vec<String> =
                    entry.prefixes.iter().map(|p| format!("`{p}`")).collect();
                let _ = write!(resource, " ({})", prefixes.join(", "));
            }
            if let Some(base) = entry.base_table {
                let _ = write!(resource, " (view of `{base}`)");
            }
            // say how it's covered
            let (classification, detail) = match entry.coverage {
                Coverage::BackedUp(component) => ("Backed up", format!("`{component}`")),
                Coverage::Excluded { reason } => ("Excluded", reason.to_owned()),
            };
            let _ = writeln!(
                page,
                "| {} | {resource} | {classification} | {detail} | {} | {} |",
                entry.store.label(),
                entry.category.label(),
                entry.gap.unwrap_or("")
            );
        }
        page
    }
}

#[cfg(test)]
mod tests {
    use super::{COVERAGE, Category, Coverage, Registry, Store, pattern_matches};
    use crate::args::BackupComponents;
    use strum::IntoEnumIterator;

    /// Builds a name an entry's pattern matches, filling a wildcard with one segment
    ///
    /// # Arguments
    ///
    /// * `pattern` - The entry's name or pattern
    fn sample_name(pattern: &str) -> String {
        // a wildcard stands for at least one segment
        pattern.replace('*', "sample")
    }

    /// Every backed-up entry names a component that runs, and every real component is named
    #[test]
    fn components_and_entries_agree() {
        // collect the components the registry names
        let named: Vec<BackupComponents> = COVERAGE
            .iter()
            .filter_map(|entry| match entry.coverage {
                Coverage::BackedUp(component) => Some(component),
                Coverage::Excluded { .. } => None,
            })
            .collect();
        // filters select components; they never back anything up themselves
        assert!(
            named.iter().all(|component| !component.is_filter()),
            "{named:?}"
        );
        // every component that isn't a filter backs up at least one entry
        for component in BackupComponents::iter().filter(|component| !component.is_filter()) {
            assert!(
                named.contains(&component),
                "no registry entry names {component}"
            );
        }
    }

    /// Every excluded entry gives a reason, and every view's base table has an entry
    #[test]
    fn reasons_and_view_bases_are_present() {
        // check each entry for what its kind needs
        for entry in COVERAGE {
            if let Coverage::Excluded { reason } = entry.coverage {
                assert!(!reason.trim().is_empty(), "{} has no reason", entry.name);
            }
            if let Some(gap) = entry.gap {
                assert!(!gap.trim().is_empty(), "{} has an empty gap", entry.name);
            }
            match entry.store {
                Store::ScyllaView => {
                    // a view is rebuilt from a base table the registry knows
                    let base = entry
                        .base_table
                        .unwrap_or_else(|| panic!("{} has no base", entry.name));
                    let base_entry = Registry::classify(Store::ScyllaTable, base)
                        .unwrap_or_else(|| panic!("{}'s base {base} has no entry", entry.name));
                    assert_eq!(base_entry.coverage, entry.coverage, "{}", entry.name);
                }
                Store::ScyllaTable => {
                    // only backed-up tables list columns, and every backed-up table does
                    let backed_up = matches!(entry.coverage, Coverage::BackedUp(_));
                    assert_eq!(entry.columns.is_some(), backed_up, "{}", entry.name);
                    assert!(entry.base_table.is_none());
                }
                Store::Redis | Store::S3 => {
                    assert!(entry.columns.is_none() && entry.base_table.is_none());
                }
            }
            // a backed-up resource is core data
            if matches!(entry.coverage, Coverage::BackedUp(_)) {
                assert_eq!(entry.category, Category::Core, "{}", entry.name);
            }
        }
    }

    /// No two entries match the same name or pattern, and every entry matches its own name
    #[test]
    fn entries_never_overlap() {
        // compare every pair of entries in the same store
        for (index, entry) in COVERAGE.iter().enumerate() {
            let name = sample_name(entry.name);
            assert!(
                pattern_matches(entry.name, &name),
                "{} doesn't match itself",
                entry.name
            );
            for other in &COVERAGE[index + 1..] {
                if other.store != entry.store {
                    continue;
                }
                let other_name = sample_name(other.name);
                assert!(
                    !pattern_matches(entry.name, &other_name)
                        && !pattern_matches(other.name, &name),
                    "{} and {} overlap",
                    entry.name,
                    other.name
                );
            }
        }
    }

    /// Wildcards stand for one or more non-empty segments, and other names match exactly
    #[test]
    fn patterns_match_whole_segments() {
        // a wildcard needs at least one segment after its prefix
        assert!(pattern_matches("user_data:*", "user_data:europa"));
        assert!(pattern_matches("streams:*", "streams:system:global:jobs"));
        assert!(!pattern_matches("user_data:*", "user_data:"));
        assert!(!pattern_matches("user_data:*", "user_data"));
        assert!(!pattern_matches("streams:*", "streams:system::jobs"));
        // a name without a wildcard matches only itself
        assert!(pattern_matches("users", "users"));
        assert!(!pattern_matches("users", "users_email_map"));
        assert!(!pattern_matches("groups", "groups_data:minerva"));
    }

    /// Every column format 2 added is one its table's archives carry
    #[test]
    fn format_two_columns_are_archived() {
        // each listed column is backed up by its table
        for (table, column) in super::FORMAT_2_COLUMNS {
            assert!(
                Registry::column_backed_up(table, column),
                "{table}.{column} is listed as added by format 2 but isn't archived"
            );
        }
    }

    /// The registry answers which columns are backed up and which views a table has
    #[test]
    fn lookups_answer_from_the_entries() {
        // archived columns are backed up, excluded ones and unknown tables aren't
        assert!(Registry::column_backed_up("results", "children"));
        assert!(Registry::column_backed_up("results", "tool_version"));
        assert!(!Registry::column_backed_up("tags", "key_lower"));
        assert!(!Registry::column_backed_up("logs", "line"));
        assert!(!Registry::column_backed_up("no_such_table", "id"));
        // views are listed by their base table
        let mut views: Vec<&str> = Registry::views_of("tags").collect();
        views.sort_unstable();
        assert_eq!(views, vec!["tags_by_item", "tags_case_insensitive"]);
        assert_eq!(Registry::views_of("nodes").count(), 0);
        // excluded entries include the runtime tables and the ephemeral buckets
        let excluded: Vec<&str> = Registry::excluded().map(|entry| entry.name).collect();
        for name in ["logs", "events", "ephemeral", "reaction_cache", "census:*"] {
            assert!(excluded.contains(&name), "{name} isn't excluded");
        }
    }

    /// The coverage page lists every entry with its store, classification, component or reason,
    /// and category, in registry order
    #[test]
    fn coverage_page_lists_every_entry_in_order() {
        // render the page and keep its table rows
        let page = Registry::render_page();
        let rows: Vec<&str> = page
            .lines()
            .filter(|line| line.starts_with("| ") && !line.starts_with("| Store"))
            .collect();
        assert_eq!(rows.len(), COVERAGE.len());
        // each row describes its entry, in the same order as the registry
        for (row, entry) in rows.iter().zip(COVERAGE) {
            let cells: Vec<&str> = row.trim_matches('|').split(" | ").map(str::trim).collect();
            assert_eq!(cells[0], entry.store.label(), "{row}");
            assert!(cells[1].starts_with(&format!("`{}`", entry.name)), "{row}");
            match entry.coverage {
                Coverage::BackedUp(component) => {
                    assert_eq!(cells[2], "Backed up", "{row}");
                    assert_eq!(cells[3], format!("`{component}`"), "{row}");
                }
                Coverage::Excluded { reason } => {
                    assert_eq!(cells[2], "Excluded", "{row}");
                    assert_eq!(cells[3], reason, "{row}");
                }
            }
            assert_eq!(cells[4], entry.category.label(), "{row}");
            assert_eq!(cells[5], entry.gap.unwrap_or(""), "{row}");
        }
    }
}
