//! The arguments for printing, hashing, applying, and checking Thorium's declared schema

use clap::Parser;
use std::path::PathBuf;
use thorium::models::upgrades::SchemaBackend;

/// The schema subcommands
#[derive(Parser, Debug, Clone)]
pub enum SchemaSubCommands {
    /// Print the declared schema, as CQL or in canonical form
    #[clap(version, author)]
    Print(PrintSchema),
    /// Print the declared fingerprints
    #[clap(version, author)]
    Hash(HashSchema),
    /// Create the keyspace and `schema_migrations`, then converge every declared object
    #[clap(version, author)]
    Apply(ApplySchema),
    /// Compare the live schema with the recorded revision's snapshot (exits 7 on drift)
    #[clap(version, author)]
    Diff(DiffSchema),
    /// Print the deployment's record, a capped drift summary, and thoradm's catalog identity on
    /// one `THORIUM-RESULT` line (read-only)
    #[clap(version, author)]
    Status(StatusSchema),
    /// Compare each declared materialized view with its base table by token range (read-only)
    #[clap(version, author)]
    VerifyViews(VerifyViews),
    /// List the legacy objects older releases created, and the objects no command can drop
    /// (read-only)
    #[clap(version, author)]
    Legacy(LegacySchema),
    /// Drop registered legacy objects: a dry run unless `--execute` is given
    #[clap(version, author)]
    DropLegacy(DropLegacy),
}

/// Print the declared schema
#[derive(Parser, Debug, Clone)]
pub struct PrintSchema {
    /// Only print this backend
    #[clap(long, value_enum)]
    pub backend: Option<SchemaBackend>,
    /// Print the canonical JSON form instead of CQL
    #[clap(long)]
    pub canonical: bool,
}

/// Print the declared fingerprints
#[derive(Parser, Debug, Clone)]
pub struct HashSchema {
    /// Only print this backend's fingerprint
    #[clap(long, value_enum)]
    pub backend: Option<SchemaBackend>,
}

/// Apply the declared schema
// each switch is one command-line flag, so a field per flag reads as the command line does
#[allow(clippy::struct_excessive_bools)]
#[derive(Parser, Debug, Clone)]
pub struct ApplySchema {
    /// Print the statements without executing them
    #[clap(long)]
    pub dry_run: bool,
    /// Wait up to this many seconds for Scylla to accept connections
    #[clap(long)]
    pub wait_for_scylla: Option<u64>,
    /// Record this revision when `schema_migrations` holds none, after probing the live schema
    /// against its snapshot; refused when another is recorded or the schema doesn't match, and
    /// recorded with its source
    #[clap(long)]
    pub seed_revision: Option<String>,
    /// Converge to this older revision's committed snapshot instead of the declared schema: create
    /// the tables, columns, and views it has that the live schema lacks (what a contract step
    /// dropped), and drop nothing a newer revision added; the first step of reversing a contract
    /// step
    #[clap(long, conflicts_with_all = ["seed_revision", "recreate_view"])]
    pub to_revision: Option<String>,
    /// Record the configured partition sizes although sampled rows suggest others (the operator
    /// passes it once the schema-apply step is approved)
    #[clap(long)]
    pub accept_partition_sizes: bool,
    /// Drop this declared materialized view and recreate it from the registry, then wait for its
    /// build; routes reading it fail while it is dropped, then see missing rows until its build
    /// finishes (repeatable)
    #[clap(long)]
    pub recreate_view: Vec<String>,
    /// Recreate views without asking for confirmation (required without a terminal)
    #[clap(long)]
    pub yes: bool,
    /// Take the namespace's writer lease even though another writer holds it; the holder's
    /// workers stop at their next batch
    #[clap(long)]
    pub force_break_lease: bool,
}

/// Compare the live schema with the recorded revision
#[derive(Parser, Debug, Clone)]
pub struct DiffSchema {
    /// Print the drift report as JSON
    #[clap(long)]
    pub json: bool,
    /// A JSON file of acknowledgements in the `drift_acknowledged` form
    #[clap(long)]
    pub acknowledged: Option<PathBuf>,
}

/// Read the deployment's record
#[derive(Parser, Debug, Clone)]
pub struct StatusSchema {
    /// The spec's acknowledgements as JSON, merged with the catalog's before the drift summary is
    /// capped
    #[clap(long)]
    pub acknowledged: Option<String>,
    /// Skip the probe and report only the record
    #[clap(long)]
    pub no_probe: bool,
    /// Also print the full result a Job left in its `job_result` row, by the Job's UID
    #[clap(long)]
    pub result: Option<String>,
    /// Wait up to this many seconds for Scylla to accept connections
    #[clap(long)]
    pub wait_for_scylla: Option<u64>,
}

/// Compare declared materialized views with their base tables
#[derive(Parser, Debug, Clone)]
pub struct VerifyViews {
    /// Only check this declared view
    #[clap(long)]
    pub view: Option<String>,
    /// Check only this many random token ranges instead of scanning every base table in full
    #[clap(long)]
    pub sample: Option<u32>,
}

/// List the legacy objects
#[derive(Parser, Debug, Clone)]
pub struct LegacySchema {
    /// Print the listing as JSON
    #[clap(long)]
    pub json: bool,
}

/// Drop registered legacy objects
#[derive(Parser, Debug, Clone)]
pub struct DropLegacy {
    /// A legacy object to drop, as `<backend>:<name>` (repeatable; there is no default of all)
    #[clap(long, required = true)]
    pub object: Vec<String>,
    /// Drop the objects instead of printing the plan
    #[clap(long)]
    pub execute: bool,
    /// With `--execute`, drop without asking for each object's id to be typed (required without a
    /// terminal)
    #[clap(long)]
    pub confirm: bool,
}
