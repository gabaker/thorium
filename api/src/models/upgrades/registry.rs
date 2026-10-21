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
