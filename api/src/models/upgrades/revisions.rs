//! Revision ids and the errors planning or recording an upgrade can hit

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

use super::latest;

/// The pattern every revision id matches: a release id, optionally with a pre-release suffix
pub const REVISION_PATTERN: &str = r"^[0-9]{4}-[0-9]{2}-v[0-9]{2}(-pre\.[a-z0-9]+(-[a-z0-9]+)*)?$";

/// The length of a release id (`YYYY-MM-vNN`)
const RELEASE_ID_LEN: usize = 11;

/// What separates a pre-release id's base release from its change
const PRERELEASE_SEPARATOR: &str = "-pre.";

/// The longest revision id, so the id always fits in the state `ConfigMap`'s revision label
const MAX_REVISION_LEN: usize = 63;

/// A revision id such as `2026-10-v02`, or a pre-release id such as `2026-10-v02-pre.theme`
///
/// Ids are zero padded so sorting them as strings sorts them in upgrade order; a pre-release id
/// sorts after its base and before the next release.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RevisionId(String);

impl RevisionId {
    /// Get this revision id as a string
    #[must_use]
    pub fn as_str(&self) -> &str {
        // borrow the validated id
        &self.0
    }

    /// Whether this is a pre-release id (`<base>-pre.<change>`) from a feature branch
    #[must_use]
    pub fn is_prerelease(&self) -> bool {
        // only pre-release ids are longer than a release id
        self.0.len() > RELEASE_ID_LEN
    }

    /// Get the release this revision is based on: itself for a release, or the base release of a
    /// pre-release id
    #[must_use]
    pub fn base(&self) -> RevisionId {
        // a release id is the first part of every valid id
        RevisionId(self.0[..RELEASE_ID_LEN].to_owned())
    }

    /// Count the months since year 0 of the year and month this revision's id names
    #[must_use]
    pub fn months(&self) -> u32 {
        // the year and month are digits the parser already checked
        let number = |range: std::ops::Range<usize>| {
            self.0[range]
                .bytes()
                .fold(0, |total, digit| total * 10 + u32::from(digit - b'0'))
        };
        number(0..4) * 12 + number(5..7) - 1
    }
}

/// Check that a pre-release change name is kebab case: lowercase letters and digits in words
/// joined by single dashes
///
/// # Arguments
///
/// * `change` - The change name after `-pre.`
fn valid_change(change: &str) -> bool {
    // every dash-separated word must be non empty and only lowercase letters and digits
    change.split('-').all(|word| {
        !word.is_empty()
            && word
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    })
}

impl FromStr for RevisionId {
    /// A revision id that isn't `YYYY-MM-vNN` fails with an [`UpgradeError`]
    type Err = UpgradeError;

    /// Parse a revision id, rejecting anything that isn't `YYYY-MM-vNN` or
    /// `YYYY-MM-vNN-pre.<change>`
    ///
    /// # Arguments
    ///
    /// * `raw` - The string to parse
    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        // split the id into its year, month, and number
        let bytes = raw.as_bytes();
        let digits = |range: std::ops::Range<usize>| bytes[range].iter().all(u8::is_ascii_digit);
        // check every part of the release id is where it should be
        let release = bytes.len() >= RELEASE_ID_LEN
            && digits(0..4)
            && bytes[4] == b'-'
            && digits(5..7)
            && bytes[7] == b'-'
            && bytes[8] == b'v'
            && digits(9..11)
            && (b"01".as_slice()..=b"12".as_slice()).contains(&&bytes[5..7]);
        // anything after the release id must be a pre-release suffix with a kebab case change
        let suffix = raw.get(RELEASE_ID_LEN..).unwrap_or_default();
        let suffix_ok = suffix.is_empty()
            || suffix
                .strip_prefix(PRERELEASE_SEPARATOR)
                .is_some_and(valid_change);
        if release && suffix_ok && raw.len() <= MAX_REVISION_LEN {
            Ok(RevisionId(raw.to_owned()))
        } else {
            Err(UpgradeError::InvalidRevision(raw.to_owned()))
        }
    }
}

impl TryFrom<String> for RevisionId {
    /// An owned string that isn't `YYYY-MM-vNN` fails with an [`UpgradeError`]
    type Error = UpgradeError;

    /// Parse a revision id from an owned string
    ///
    /// # Arguments
    ///
    /// * `raw` - The string to parse
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        // validate the id the same way as a borrowed string
        raw.parse()
    }
}

impl From<RevisionId> for String {
    /// Get the string form of a revision id
    ///
    /// # Arguments
    ///
    /// * `id` - The revision id to convert
    fn from(id: RevisionId) -> Self {
        // hand back the inner id
        id.0
    }
}

impl fmt::Display for RevisionId {
    /// Write this revision id
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // write the id as is
        f.write_str(&self.0)
    }
}

/// An error planning or recording an upgrade
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpgradeError {
    /// A revision id isn't `YYYY-MM-vNN` or `YYYY-MM-vNN-pre.<change>`
    InvalidRevision(String),
    /// A revision id is well formed but not in this build's catalog
    UnknownRevision(String),
    /// The cluster is at a newer revision than this build knows about; the newer build must
    /// revert it first (`thoradm migrate revert --to <revision>`)
    AheadOfCatalog {
        /// The revision the cluster is at
        current: RevisionId,
        /// The newest revision this build knows about
        latest: RevisionId,
    },
    /// The recorded upgrade state uses a newer layout than this build reads
    SchemaAhead {
        /// The layout version of the recorded state
        found: u32,
        /// The newest layout version this build reads
        supported: u32,
    },
    /// The cluster is on a pre-release revision this build doesn't know; that branch's image must
    /// revert it to its base first
    UnknownPrerelease {
        /// The pre-release revision the cluster is at
        current: RevisionId,
        /// The release the pre-release is based on
        base: RevisionId,
    },
    /// No path through the revision graph reaches the target
    NoPath {
        /// The revision the cluster is at
        from: RevisionId,
        /// The requested target revision
        to: RevisionId,
    },
    /// A downgrade path crosses an irreversible step; restore a backup instead
    Irreversible {
        /// The revision the step belongs to
        revision: RevisionId,
        /// The irreversible step
        step: String,
    },
    /// A downgrade path takes edges that don't undo a step the deployment finished, as when the
    /// deployment came up one way and the path goes down another; downgrade along the edges the
    /// deployment took, or restore a backup instead
    Unreversed {
        /// The revision the step belongs to
        revision: RevisionId,
        /// The finished step no step on the path undoes
        step: String,
    },
    /// The cluster is below this build's minimum supported revision; install the intermediate
    /// release first, then follow the upgrade-paths table. Checked after `UnknownPrerelease`, so a
    /// pre-release id below the floor gets the revert instruction
    BelowMinimum {
        /// The revision the cluster is at
        current: RevisionId,
        /// The oldest revision this build plans from
        minimum: RevisionId,
        /// The immediate release to install first: the intermediate of the highest floor
        /// `current` is below whose catalog still plans from `current`
        intermediate: String,
    },
}

impl fmt::Display for UpgradeError {
    /// Describe this error
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // describe each error in terms an admin can act on
        match self {
            // a malformed id
            Self::InvalidRevision(raw) => write!(
                f,
                "Revision {raw:?} is not a revision id like 2026-10-v02 or \
                 2026-10-v02-pre.<change>"
            ),
            // a well formed id missing from the catalog
            Self::UnknownRevision(raw) => write!(
                f,
                "Revision {raw} is not known to this Thorium build (latest is {})",
                latest()
            ),
            // a cluster upgraded by a newer build
            Self::AheadOfCatalog { current, latest } => write!(
                f,
                "This cluster is at revision {current}, which is newer than this Thorium \
                 build's latest revision {latest}; with the Thorium build that knows revision \
                 {current} still deployed, revert the cluster to {latest} (target_revision \
                 {latest}, or `thoradm migrate revert --to {latest}`) before deploying this \
                 build; if that build refuses the revert because a step can't be undone, restore \
                 from a backup taken before that step instead"
            ),
            // state written in a newer layout
            Self::SchemaAhead { found, supported } => write!(
                f,
                "This cluster's upgrade state (schema version {found}) was written by a newer \
                 Thorium operator than this one (which reads up to schema version {supported}); \
                 deploy that newer Thorium operator again and revert the cluster with it \
                 (`thoradm migrate revert --to {}`) before deploying this one",
                latest()
            ),
            // a pre-release from a branch this build doesn't carry
            Self::UnknownPrerelease { current, base } => write!(
                f,
                "This cluster is at pre-release revision {current}, which this Thorium build \
                 doesn't know; revert it with the image of the branch that applied it \
                 (`thoradm migrate revert --to {base}`, or target_revision {base}) before \
                 deploying this build"
            ),
            // a target the revision graph can't reach
            Self::NoPath { from, to } => write!(
                f,
                "This Thorium build's catalog has no path from revision {from} to revision {to}"
            ),
            // a downgrade across a step that can't be undone
            Self::Irreversible { revision, step } => write!(
                f,
                "Step {step} of revision {revision} can't be undone, so this downgrade is \
                 refused before anything runs; restore from backup instead (a backup taken \
                 before step {step} ran)"
            ),
            // a downgrade whose edges don't undo a finished step
            Self::Unreversed { revision, step } => write!(
                f,
                "Step {step} of revision {revision} finished on this cluster, but no step on the \
                 path down to the target undoes it, so this downgrade is refused before anything \
                 runs; downgrade along the edges the cluster took, targeting each revision it \
                 passed through in turn, or restore from backup"
            ),
            // a cluster older than this build's support floor
            Self::BelowMinimum {
                current,
                minimum,
                intermediate,
            } => write!(
                f,
                "This cluster is at revision {current}, older than {minimum}, the oldest \
                 revision this Thorium release upgrades from; install Thorium {intermediate} \
                 first and let it finish upgrading, then this release; for older deployments \
                 follow the table of valid upgrade paths in the upgrade guide"
            ),
        }
    }
}

impl std::error::Error for UpgradeError {}
