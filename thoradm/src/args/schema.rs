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
    /// Compare the live schema with the recorded revision's snapshot (exits 2 on drift)
    #[clap(version, author)]
    Diff(DiffSchema),
    /// Print the deployment's record, a capped drift summary, and thoradm's catalog identity on
    /// one `THORIUM-RESULT` line (read-only)
    #[clap(version, author)]
    Status(StatusSchema),
    /// Compare each declared materialized view with its base table by token range (read-only)
    #[clap(version, author)]
    VerifyViews(VerifyViews),
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
    /// Record the configured partition sizes although sampled rows suggest others (the operator
    /// passes it once the schema-apply step is approved)
    #[clap(long)]
    pub accept_partition_sizes: bool,
    /// Record a changed `earliest.<type>` setting instead of refusing it
    #[clap(long)]
    pub accept_config: Vec<String>,
    /// Drop this declared materialized view and recreate it from the registry, then wait for its
    /// build; routes reading it fail until the build finishes (repeatable)
    #[clap(long)]
    pub recreate_view: Vec<String>,
    /// Recreate views without asking for confirmation (required without a terminal)
    #[clap(long)]
    pub yes: bool,
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
