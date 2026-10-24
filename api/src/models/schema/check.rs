//! How the API's startup check judges the live schema against the one this build declares

use scylla::client::session::Session;
use std::collections::BTreeSet;

use super::apply::{ApplyError, applied_fingerprint, check_config};
use super::{ChangeClass, Fingerprint, RevisionSnapshot, SchemaChange, SnapshotError};
use crate::Conf;
use crate::models::upgrades::{Catalog, CatalogEntry, RevisionId};

/// Why the API refused to start against the live schema
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaCheckError {
    /// The keyspace or `schema_migrations` doesn't exist ("run `thoradm schema apply`")
    Missing,
    /// The applied schema is older than this build needs ("the operator hasn't applied revision
    /// X yet")
    Older {
        /// The fingerprint recorded in `schema_migrations`
        applied: Fingerprint,
        /// The fingerprint this build declares
        declared: Fingerprint,
        /// The revision this build needs applied
        revision: RevisionId,
    },
    /// The applied fingerprint is a later revision this build's catalog knows, and the diff from
    /// the declared schema to that revision's snapshot has a non-additive change
    Newer {
        /// The fingerprint recorded in `schema_migrations`
        applied: Fingerprint,
        /// The fingerprint this build declares
        declared: Fingerprint,
        /// The later revision the applied fingerprint belongs to
        revision: RevisionId,
        /// The objects whose difference isn't additive, as `object` or `object.column`
        breaking: Vec<String>,
    },
    /// The applied fingerprint matches no revision in this build's catalog, such as one a newer
    /// build applied before its image was rolled back
    UnknownFingerprint {
        /// The fingerprint recorded in `schema_migrations`
        applied: Fingerprint,
        /// The fingerprint this build declares
        declared: Fingerprint,
    },
    /// A data-shaping setting differs from the recorded one while its data exists
    ConfigChanged {
        /// The setting, such as `thorium.files.partition_size`
        setting: String,
    },
    /// The recorded files-password hash isn't argon2id with `Params::default()`, so it is refused
    /// before any hash is computed
    PasswordHashParams,
    /// `schema_migrations` exists but its record couldn't be read
    Unreadable(String),
}

/// How the API's startup check let it start
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaCheckPass {
    /// The applied fingerprint is the one this build declares
    Current,
    /// The applied fingerprint is a later revision this build's catalog knows, and every
    /// difference from the declared schema to that revision's snapshot is additive; the API starts
    /// and logs a warning naming the revision
    NewerAdditive {
        /// The fingerprint recorded in `schema_migrations`
        applied: Fingerprint,
        /// The later revision the applied fingerprint belongs to
        revision: RevisionId,
    },
}

impl std::fmt::Display for SchemaCheckError {
    /// Write the refusal the API prints when it exits, naming what the admin does next
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // nothing was ever applied
            SchemaCheckError::Missing => write!(
                f,
                "the Thorium schema isn't applied: the keyspace, the schema_migrations table, or \
                 the deployment's record doesn't exist; run `thoradm schema apply`"
            ),
            // the keyspace is behind this build
            SchemaCheckError::Older {
                applied,
                declared,
                revision,
            } => write!(
                f,
                "the applied schema {} is older than the one this build needs, {}: the operator \
                 hasn't applied revision {revision} yet; run `thoradm schema apply` or wait for \
                 the operator's schema apply to finish",
                applied.as_str(),
                declared.as_str()
            ),
            // the keyspace is ahead of this build in a way it can't read
            SchemaCheckError::Newer {
                applied,
                declared,
                revision,
                breaking,
            } => write!(
                f,
                "the applied schema {} is revision {revision}, which changes what this build, \
                 declaring {}, reads incompatibly ({}); roll the deployment forward to a build \
                 that knows it",
                applied.as_str(),
                declared.as_str(),
                breaking.join(", ")
            ),
            // the keyspace is at a schema no revision names
            SchemaCheckError::UnknownFingerprint { applied, declared } => write!(
                f,
                "the applied schema {} is in no revision this build knows, and this build \
                 declares {}; a newer build probably applied it, so roll the deployment forward, \
                 or compare the two with `thoradm schema diff`",
                applied.as_str(),
                declared.as_str()
            ),
            // a data-shaping setting moved; only its name is printed, never a value
            SchemaCheckError::ConfigChanged { setting } => write!(
                f,
                "{setting} differs from the value recorded for this deployment while data shaped \
                 by it exists; set it back to the recorded value"
            ),
            // the recorded password identity is one this build never computes
            SchemaCheckError::PasswordHashParams => write!(
                f,
                "the recorded files password hash isn't argon2id with the default parameters, so \
                 it is refused before any hash is computed"
            ),
            // the record exists but couldn't be read
            SchemaCheckError::Unreadable(error) => {
                write!(f, "the schema record couldn't be read: {error}")
            }
        }
    }
}

impl std::error::Error for SchemaCheckError {}

impl SchemaCheckError {
    /// Turn an apply or data-shaping check failure into a refusal, when it is one
    ///
    /// A changed or contradicted data-shaping setting, or a recorded password hash with other
    /// parameters, is a judgement about the deployment that retrying can't change. Any other
    /// failure, such as a failed query, returns `None`.
    ///
    /// # Arguments
    ///
    /// * `err` - The failure to classify
    #[must_use]
    pub fn from_apply(err: &ApplyError) -> Option<SchemaCheckError> {
        // name every setting that stopped the run, never a value
        let settings = err.stopped_settings();
        match (err, settings.is_empty()) {
            (ApplyError::PasswordHashParams, _) => Some(SchemaCheckError::PasswordHashParams),
            (_, false) => Some(SchemaCheckError::ConfigChanged {
                setting: settings.join(", "),
            }),
            (_, true) => None,
        }
    }

    /// Whether retrying the start could change the outcome
    ///
    /// Only a failed read of the schema record can: every other refusal judges the deployment.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        // an unreadable record may be a query that failed for a moment
        matches!(self, SchemaCheckError::Unreadable(_))
    }
}

impl SchemaCheckPass {
    /// The warning the API logs once when it starts, if the check passed with one
    ///
    /// A current schema passes silently. An applied schema from a later revision the build's
    /// catalog knows names that revision, since pods of another build may already run on it.
    #[must_use]
    pub fn warning(&self) -> Option<String> {
        // only a later, additive revision warns
        match self {
            SchemaCheckPass::Current => None,
            SchemaCheckPass::NewerAdditive { applied, revision } => Some(format!(
                "the applied schema {} is revision {revision}, later than this build; starting \
                 because every difference from what this build declares is additive",
                applied.as_str()
            )),
        }
    }
}

/// Judge the fingerprint a keyspace was applied at against what this build declares
///
/// The fingerprint is current when it is the declared one. Otherwise the catalog says which
/// revision it belongs to: an earlier one is refused as not yet applied, a later one the catalog
/// knows starts with a warning when every difference from the declared schema is additive and is
/// refused when any isn't, and one no revision names is refused as unknown.
///
/// # Arguments
///
/// * `catalog` - The revisions this build knows
/// * `snapshots` - Loads a revision's committed snapshot
/// * `declared` - The snapshot of what this build declares
/// * `applied` - The deployment fingerprint recorded in the keyspace
pub fn judge_applied(
    catalog: &Catalog,
    snapshots: &(dyn Fn(&str) -> Option<Result<RevisionSnapshot, SnapshotError>> + Sync),
    declared: &RevisionSnapshot,
    applied: &Fingerprint,
) -> Result<SchemaCheckPass, SchemaCheckError> {
    // the declared fingerprint is the one this build wants
    if *applied == declared.deployment {
        return Ok(SchemaCheckPass::Current);
    }
    // the revision this build needs applied is the newest one whose schema it declares, or the
    // catalog's latest when the declared schema has unreleased changes
    let own = catalog
        .edges
        .iter()
        .filter(|edge| edge.schema == Some(declared.deployment.as_str()))
        .map(CatalogEntry::revision)
        .max()
        .unwrap_or_else(|| catalog.latest());
    // the newest revision the applied fingerprint names, if any does
    let named = catalog
        .edges
        .iter()
        .filter(|edge| edge.schema == Some(applied.as_str()))
        .max_by_key(|edge| edge.revision());
    let unknown = || SchemaCheckError::UnknownFingerprint {
        applied: applied.clone(),
        declared: declared.deployment.clone(),
    };
    let Some(named) = named else {
        return Err(unknown());
    };
    let revision = named.revision();
    // a revision at or before this build's own is one the operator hasn't moved past yet
    if revision <= own {
        return Err(SchemaCheckError::Older {
            applied: applied.clone(),
            declared: declared.deployment.clone(),
            revision: own,
        });
    }
    // a later revision can only be judged from its snapshot
    let Some(Ok(snapshot)) = snapshots(named.id) else {
        return Err(unknown());
    };
    // every difference from what this build declares must be one old code tolerates
    let breaking = SchemaChange::between(&declared.objects, &snapshot.objects)
        .into_iter()
        .filter(|change| change.class != ChangeClass::Additive)
        .map(|change| match change.field {
            Some(field) => format!("{}.{field}", change.object),
            None => change.object,
        })
        .collect::<BTreeSet<_>>();
    if breaking.is_empty() {
        Ok(SchemaCheckPass::NewerAdditive {
            applied: applied.clone(),
            revision,
        })
    } else {
        Err(SchemaCheckError::Newer {
            applied: applied.clone(),
            declared: declared.deployment.clone(),
            revision,
            breaking: breaking.into_iter().collect(),
        })
    }
}

/// Check the live schema before the API prepares a statement
///
/// Reads the applied fingerprint and the data-shaping record from `schema_migrations` and judges
/// them against this build, issuing no DDL and writing nothing.
///
/// # Arguments
///
/// * `session` - The Scylla session to read with
/// * `conf` - The Thorium config
/// * `catalog` - The revisions this build knows
/// * `snapshots` - Loads a revision's committed snapshot
pub async fn check_startup(
    session: &Session,
    conf: &Conf,
    catalog: &Catalog,
    snapshots: &(dyn Fn(&str) -> Option<Result<RevisionSnapshot, SnapshotError>> + Sync),
) -> Result<SchemaCheckPass, SchemaCheckError> {
    // read the applied fingerprint, which nothing is judged without
    let applied = applied_fingerprint(session, conf)
        .await
        .map_err(|err| SchemaCheckError::Unreadable(err.to_string()))?
        .ok_or(SchemaCheckError::Missing)?;
    // a fingerprint is a transparent string, so the recorded text becomes one
    let applied = serde_json::from_value::<Fingerprint>(serde_json::Value::String(applied))
        .map_err(|err| SchemaCheckError::Unreadable(err.to_string()))?;
    // judge it against the schema this build declares
    let declared = RevisionSnapshot::declared(catalog.latest());
    let pass = judge_applied(catalog, snapshots, &declared, &applied)?;
    // then compare the configured data-shaping settings with the record
    check_config(session, conf).await.map_err(|err| {
        // a refused setting is final, and a failed read stays unreadable
        SchemaCheckError::from_apply(&err)
            .unwrap_or_else(|| SchemaCheckError::Unreadable(err.to_string()))
    })?;
    Ok(pass)
}
