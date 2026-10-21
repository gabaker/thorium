//! What a restore writes to: the backed-up tables and buckets of the cluster in thoradm's config
//!
//! The list comes from the coverage registry, narrowed to the components a backup holds, and
//! every name in it comes from thoradm's config, never from the backup. The empty-target check
//! uses its tables to tell whether a restore would merge into data the target already holds; it
//! examines no bucket.

use scylla::client::session::Session;
use std::collections::BTreeSet;
use std::path::Path;
use strum::IntoEnumIterator;
use thorium::Conf;

use super::coverage::{COVERAGE, Coverage, Store};
use super::manifest::Manifest;
use super::verify::{TABLES, component_table};
use crate::Error;
use crate::args::BackupComponents;

/// The backed-up tables and buckets a restore of one backup writes to
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// The keyspace the tables are in: thoradm's configured namespace
    pub keyspace: String,
    /// The backed-up tables the backup holds
    pub tables: Vec<&'static str>,
    /// The names of the backed-up buckets the backup holds objects for, by thoradm's config
    pub buckets: Vec<String>,
}

/// Returns the bucket name a config uses for a bucket role
///
/// # Arguments
///
/// * `conf` - The config to read
/// * `role` - The bucket role
#[must_use]
pub fn bucket_for(conf: &Conf, role: &str) -> Option<String> {
    // every role the registry lists has a section in the config
    let thorium = &conf.thorium;
    let bucket = match role {
        "files" => &thorium.files.bucket,
        "repos" => &thorium.repos.bucket,
        "attachments" => &thorium.attachments.bucket,
        "results" => &thorium.results.bucket,
        "graphics" => &thorium.graphics.bucket,
        "ephemeral" => &thorium.ephemeral.bucket,
        "reaction_cache" => &thorium.reaction_cache.bucket,
        _ => return None,
    };
    Some(bucket.clone())
}

/// Returns the components a backup holds: its manifest's, or for a backup without one, every
/// component whose files are in the directory
///
/// # Arguments
///
/// * `root` - The backup root
/// * `manifest` - The backup's manifest, if it has one
pub async fn backup_components(
    root: &Path,
    manifest: Option<&Manifest>,
) -> Result<Vec<BackupComponents>, Error> {
    // a manifest lists exactly what the backup ran
    if let Some(manifest) = manifest {
        return Ok(manifest.components.clone());
    }
    // otherwise each table directory, each objects directory, and redis.json name a component
    let mut found = Vec::new();
    for (table, _) in TABLES {
        let dir = root.join(table);
        if !tokio::fs::try_exists(&dir).await? {
            continue;
        }
        for component in BackupComponents::iter().filter(|component| !component.is_filter()) {
            if component_table(component) == Some(*table) {
                found.push(component);
            }
            if component.s3_input().and_then(component_table) == Some(*table)
                && tokio::fs::try_exists(dir.join("objects")).await?
            {
                found.push(component);
            }
        }
    }
    if tokio::fs::try_exists(root.join("redis.json")).await? {
        found.push(BackupComponents::Redis);
    }
    Ok(found)
}

impl Target {
    /// Builds the target a restore of some components writes to, from the registry and a config
    ///
    /// # Arguments
    ///
    /// * `conf` - Thoradm's config, which names the keyspace and buckets
    /// * `components` - The components the backup holds
    #[must_use]
    pub fn new(conf: &Conf, components: &[BackupComponents]) -> Self {
        // every backed-up table and bucket role whose component the backup holds
        let mut tables = Vec::new();
        let mut roles = BTreeSet::new();
        for entry in COVERAGE {
            let Coverage::BackedUp(component) = entry.coverage else {
                continue;
            };
            if !components.contains(&component) {
                continue;
            }
            match entry.store {
                Store::ScyllaTable => tables.push(entry.name),
                Store::S3 => {
                    roles.insert(entry.name);
                }
                Store::ScyllaView | Store::Redis => (),
            }
        }
        // each role's bucket as thoradm's config names it, once when roles share a bucket
        let buckets = roles
            .into_iter()
            .filter_map(|role| bucket_for(conf, role))
            .collect::<BTreeSet<String>>()
            .into_iter()
            .collect();
        Target {
            keyspace: conf.thorium.namespace.clone(),
            tables,
            buckets,
        }
    }

    /// Returns every table of the target that already holds data, by name
    ///
    /// # Arguments
    ///
    /// * `scylla` - A session to the target's Scylla
    pub async fn holding_data(&self, scylla: &Session) -> Result<Vec<String>, Error> {
        let mut holding = Vec::new();
        // a table holds data when it has one row
        for table in &self.tables {
            let query = format!("SELECT * FROM {}.{table} LIMIT 1", self.keyspace);
            let rows = scylla.query_unpaged(query, &[]).await?;
            let found = rows
                .into_rows_result()
                .map_err(|error| Error::new(format!("failed to read {table}: {error}")))?
                .rows_num();
            if found > 0 {
                holding.push(format!("table {}.{table}", self.keyspace));
            }
        }
        Ok(holding)
    }
}
