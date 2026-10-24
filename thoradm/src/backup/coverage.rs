//! The coverage registry: every resource Thorium stores and how backup and restore treat it
//!
//! The resources, their data classes, and their coverage (backed up or excluded, the reason, a
//! known gap, a table's archived columns) come from the api crate's class table,
//! [`CLASSES`]; this registry adds the backup component that carries each backed-up resource.
//! A backed-up table or bucket role names its component in [`COMPONENTS`], a view is carried by
//! its base table's component, and every backed-up Redis family by the Redis backup; thoradm
//! fails to build when a backed-up table or bucket role has no component. The completeness tests
//! fail when Thorium stores something no entry classifies.

use crate::args::BackupComponents;
use std::fmt::Write;
use std::ops::Deref;
use thorium::models::schema::{BackupCoverage, CLASSES, ClassEntry, DataClass};

pub use thorium::models::schema::{ColumnCoverage, Store};

/// The group a resource's data class is shown under in the admin docs and the restore preflight
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Category {
    /// Data users created or depend on; backed up and restored
    Core,
    /// Data a restore rebuilds from other backed-up data (census streams, `system_info`)
    Derived,
    /// Runtime state that is invalid after a restore (reactions, jobs, logs, events, caches),
    /// whether or not every write of it sets a TTL
    Ephemeral,
}

impl Category {
    /// Returns the group a data class is shown under: `Expiring` and `Runtime` alike are
    /// ephemeral
    ///
    /// # Arguments
    ///
    /// * `class` - The data class
    #[must_use]
    pub const fn of(class: DataClass) -> Category {
        // state that expires and state that doesn't are both invalid after a restore
        match class {
            DataClass::Core => Category::Core,
            DataClass::Derived => Category::Derived,
            DataClass::Expiring | DataClass::Runtime => Category::Ephemeral,
        }
    }

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

/// One resource Thorium stores, and how backup and restore treat it
///
/// It dereferences to the resource's class table entry for its store, name, columns, and gap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoverageEntry {
    /// The resource's class table entry
    pub info: ClassEntry,
    /// Whether the resource is backed up, and by which component
    pub coverage: Coverage,
    /// The group the resource's data class is shown under
    pub category: Category,
}

impl Deref for CoverageEntry {
    type Target = ClassEntry;

    /// Returns the resource's class table entry
    fn deref(&self) -> &ClassEntry {
        // the entry holds everything but the component
        &self.info
    }
}

/// The tables the backup's schema record, `schema.json`, carries instead of a table archive
pub const RECORD_TABLES: [&str; 2] = ["schema_migrations", "schema_migration_issues"];

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

/// The backup component that carries each backed-up table and bucket role
///
/// Views are carried by their base table's component and backed-up Redis families by
/// [`BackupComponents::Redis`], so neither is listed.
pub const COMPONENTS: &[(Store, &str, BackupComponents)] = &[
    (
        Store::ScyllaTable,
        "samples_list",
        BackupComponents::SamplesList,
    ),
    (Store::ScyllaTable, "s3_ids", BackupComponents::S3Ids),
    (Store::ScyllaTable, "comments", BackupComponents::Comments),
    (Store::ScyllaTable, "results", BackupComponents::Results),
    (
        Store::ScyllaTable,
        "results_stream",
        BackupComponents::ResultsStream,
    ),
    (Store::ScyllaTable, "tags", BackupComponents::Tags),
    (Store::ScyllaTable, "repo_data", BackupComponents::RepoData),
    (Store::ScyllaTable, "repos_list", BackupComponents::RepoList),
    (
        Store::ScyllaTable,
        "commitishes",
        BackupComponents::Commitish,
    ),
    (
        Store::ScyllaTable,
        "commitish_list",
        BackupComponents::CommitishList,
    ),
    (
        Store::ScyllaTable,
        "network_policies",
        BackupComponents::NetworkPolicies,
    ),
    (Store::ScyllaTable, "entities", BackupComponents::Entities),
    (
        Store::ScyllaTable,
        "associations",
        BackupComponents::Associations,
    ),
    (
        Store::ScyllaTable,
        "notifications",
        BackupComponents::Notifications,
    ),
    (Store::ScyllaTable, "nodes", BackupComponents::Nodes),
    (Store::S3, "files", BackupComponents::S3IdsObjects),
    (Store::S3, "repos", BackupComponents::S3IdsObjects),
    (
        Store::S3,
        "attachments",
        BackupComponents::CommentAttachments,
    ),
    (Store::S3, "results", BackupComponents::ResultFiles),
    (Store::S3, "graphics", BackupComponents::Graphics),
];

/// Returns whether two names are equal, in a constant context
///
/// # Arguments
///
/// * `left` - One name
/// * `right` - The other name
const fn same_name(left: &str, right: &str) -> bool {
    // compare the names byte by byte
    let (left, right) = (left.as_bytes(), right.as_bytes());
    // names of different lengths can't be equal
    if left.len() != right.len() {
        return false;
    }
    // walk both names, stopping at the first byte that differs
    let mut index = 0;
    while index < left.len() {
        if left[index] != right[index] {
            return false;
        }
        index += 1;
    }
    // every byte matched
    true
}

/// Returns the component [`COMPONENTS`] lists for a table or bucket role, failing the build when
/// it lists none
///
/// # Arguments
///
/// * `store` - The resource's store
/// * `name` - The table name or bucket role
const fn listed_component(store: Store, name: &str) -> BackupComponents {
    // find the resource's row
    let mut index = 0;
    while index < COMPONENTS.len() {
        let (listed_store, listed_name, component) = COMPONENTS[index];
        // the row is the one with the same store and name
        if listed_store as u8 == store as u8 && same_name(listed_name, name) {
            return component;
        }
        index += 1;
    }
    // a resource with no row fails the build, so a new backed-up resource can't be forgotten
    panic!("a backed-up table or bucket role in the class table has no component in COMPONENTS")
}

/// Returns how the backup treats a class table entry
///
/// # Arguments
///
/// * `info` - The resource's class table entry
const fn coverage_of(info: &ClassEntry) -> Coverage {
    // an excluded resource keeps its reason
    if let BackupCoverage::Excluded { reason } = info.backup {
        return Coverage::Excluded { reason };
    }
    // a backed-up resource is carried by its store's component
    let component = match (info.store, info.base_table) {
        (Store::Redis, _) => BackupComponents::Redis,
        (Store::ScyllaView, Some(base)) => listed_component(Store::ScyllaTable, base),
        (Store::ScyllaView, None) => panic!("a backed-up view in the class table has no base"),
        (store, _) => listed_component(store, info.name),
    };
    Coverage::BackedUp(component)
}

/// Builds the registry from the class table, in the class table's order
const fn build() -> [CoverageEntry; CLASSES.len()] {
    // start every slot from the first entry, then fill each in order
    let first = CoverageEntry {
        info: CLASSES[0],
        coverage: coverage_of(&CLASSES[0]),
        category: Category::of(CLASSES[0].class),
    };
    let mut entries = [first; CLASSES.len()];
    let mut index = 1;
    while index < CLASSES.len() {
        entries[index] = CoverageEntry {
            info: CLASSES[index],
            coverage: coverage_of(&CLASSES[index]),
            category: Category::of(CLASSES[index].class),
        };
        index += 1;
    }
    entries
}

/// Every resource Thorium stores, built from the class table at compile time; the completeness
/// tests fail on anything not listed
pub const COVERAGE: &[CoverageEntry] = &build();

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

    /// Returns whether a view is one only keyspaces older releases created hold
    ///
    /// # Arguments
    ///
    /// * `view` - The view name
    #[must_use]
    pub fn is_legacy_view(view: &str) -> bool {
        // a view entry of that name marked legacy
        COVERAGE
            .iter()
            .any(|entry| entry.store == Store::ScyllaView && entry.name == view && entry.legacy)
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
             Generated from thoradm's coverage registry by `cargo run -p thoradm --example coverage_page`, and printed by the installed binary with `thoradm backup coverage`.\n\n\
             A backed-up row or record that names an S3 object the bucket lacks (a broken link) fails that object by default. \
             With `thoradm backup new --accept-broken-links` the backup records the broken link in failures.jsonl, keeps the row as the cluster holds it, and goes on; \
             a restore of a backup whose only records are accepted broken links needs no `--accept-failures`.\n\n\
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
            if entry.legacy {
                resource.push_str(", only in keyspaces older releases created");
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

    /// Renders the registry as a JSON array, one object per entry in registry order
    ///
    /// Each object has the fields `store`, `name`, `prefixes`, `base_table`, `classification`
    /// (`backed-up` or `excluded`), `component` (set when backed up), `reason` (set when
    /// excluded), `category`, and `gap`; a field that doesn't apply is `null`.
    pub fn render_json() -> Result<String, serde_json::Error> {
        // describe every entry in registry order
        let entries: Vec<JsonEntry> = COVERAGE.iter().map(JsonEntry::from).collect();
        // serialize them as one pretty-printed array
        let mut json = serde_json::to_string_pretty(&entries)?;
        json.push('\n');
        Ok(json)
    }
}

/// One registry entry as `thoradm backup coverage --json` prints it
#[derive(serde::Serialize)]
struct JsonEntry {
    /// The store the resource lives in
    store: Store,
    /// The table or view name, the Redis key pattern, or the bucket role
    name: &'static str,
    /// The key layouts an S3 entry's objects follow
    prefixes: &'static [&'static str],
    /// A view's base table
    base_table: Option<&'static str>,
    /// `backed-up` or `excluded`
    classification: &'static str,
    /// The component that backs the resource up, when it is backed up
    component: Option<String>,
    /// Why the resource is excluded, when it is
    reason: Option<&'static str>,
    /// The resource's category
    category: Category,
    /// A known gap in how today's backup or restore covers the resource
    gap: Option<&'static str>,
}

impl From<&CoverageEntry> for JsonEntry {
    /// Describes a registry entry for the JSON listing
    ///
    /// # Arguments
    ///
    /// * `entry` - The entry to describe
    fn from(entry: &CoverageEntry) -> Self {
        // split the coverage into its classification and the component or reason
        let (classification, component, reason) = match entry.coverage {
            Coverage::BackedUp(component) => ("backed-up", Some(component.to_string()), None),
            Coverage::Excluded { reason } => ("excluded", None, Some(reason)),
        };
        JsonEntry {
            store: entry.store,
            name: entry.name,
            prefixes: entry.prefixes,
            base_table: entry.base_table,
            classification,
            component,
            reason,
            category: entry.category,
            gap: entry.gap,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        COMPONENTS, COVERAGE, Category, Coverage, RECORD_TABLES, Registry, Store, pattern_matches,
    };
    use crate::args::BackupComponents;
    use strum::IntoEnumIterator;
    use thorium::models::schema::{BackupCoverage, CLASSES, DataClass};

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
            // a backed-up resource is core data, except a view, which is derived
            if matches!(entry.coverage, Coverage::BackedUp(_)) {
                let category = if entry.store == Store::ScyllaView {
                    Category::Derived
                } else {
                    Category::Core
                };
                assert_eq!(entry.category, category, "{}", entry.name);
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
        // only the view older releases created is a legacy view
        assert!(Registry::is_legacy_view("entities_by_name"));
        assert!(!Registry::is_legacy_view("entities_by_id"));
        assert!(!Registry::is_legacy_view("events"));
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

    /// The schema record tables aren't archived as tables: each is excluded from the components,
    /// and its reason says the backup's schema record, `schema.json`, carries it, with no gap
    #[test]
    fn the_schema_record_tables_are_carried_by_schema_json() {
        // check each table the schema record lives in
        for name in RECORD_TABLES {
            let entry = Registry::classify(Store::ScyllaTable, name)
                .unwrap_or_else(|| panic!("{name} is unclassified"));
            let Coverage::Excluded { reason } = entry.coverage else {
                panic!("{name} is archived as a table, and the record carries it too");
            };
            assert!(
                reason.contains("`schema.json`"),
                "{name}'s reason doesn't name the record that carries it: {reason}"
            );
            assert_eq!(entry.gap, None, "{name} still names a gap");
        }
    }

    /// The registry is the api crate's class table, entry for entry and in order, with only the
    /// backup component added: an excluded entry keeps the table's reason, a backed-up one gets
    /// its component, every listed component belongs to a backed-up table or bucket role, and
    /// `Expiring` and `Runtime` entries are both shown as `EPHEMERAL`
    #[test]
    fn registry_is_the_class_table_with_components() {
        // one registry entry per class table entry, holding that entry unchanged
        assert_eq!(COVERAGE.len(), CLASSES.len());
        for (entry, class) in COVERAGE.iter().zip(CLASSES) {
            assert_eq!(&entry.info, class, "{}", class.name);
            // the coverage follows the table's backup decision
            match (entry.coverage, class.backup) {
                (Coverage::Excluded { reason }, BackupCoverage::Excluded { reason: listed }) => {
                    assert_eq!(reason, listed, "{}", class.name);
                }
                (Coverage::BackedUp(component), BackupCoverage::BackedUp) => {
                    // a view shares its base table's component, a Redis family the Redis backup's
                    let expected = match (class.store, class.base_table) {
                        (Store::Redis, _) => BackupComponents::Redis,
                        (Store::ScyllaView, Some(base)) => {
                            Registry::classify(Store::ScyllaTable, base)
                                .and_then(|base| match base.coverage {
                                    Coverage::BackedUp(component) => Some(component),
                                    Coverage::Excluded { .. } => None,
                                })
                                .unwrap_or_else(|| panic!("{}'s base isn't backed up", class.name))
                        }
                        (store, _) => COMPONENTS
                            .iter()
                            .find(|(listed, name, _)| *listed == store && *name == class.name)
                            .map_or_else(
                                || panic!("{} has no component", class.name),
                                |(_, _, component)| *component,
                            ),
                    };
                    assert_eq!(component, expected, "{}", class.name);
                }
                (coverage, backup) => panic!("{}: {coverage:?} against {backup:?}", class.name),
            }
            // expiring and runtime state share the ephemeral label
            let label = match class.class {
                DataClass::Core => "CORE",
                DataClass::Derived => "DERIVED",
                DataClass::Expiring | DataClass::Runtime => "EPHEMERAL",
            };
            assert_eq!(entry.category.label(), label, "{}", class.name);
        }
        assert_eq!(
            Category::of(DataClass::Expiring),
            Category::of(DataClass::Runtime)
        );
        // every component row names a backed-up table or bucket role of the class table
        for (store, name, _) in COMPONENTS {
            assert!(
                matches!(store, Store::ScyllaTable | Store::S3),
                "{name} needs no component row"
            );
            let backed_up = CLASSES.iter().any(|class| {
                class.store == *store
                    && class.name == *name
                    && class.backup == BackupCoverage::BackedUp
            });
            assert!(
                backed_up,
                "{name} isn't a backed-up resource of the class table"
            );
        }
    }
}
