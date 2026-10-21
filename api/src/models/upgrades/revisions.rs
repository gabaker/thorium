//! Revision ids and the errors planning or recording an upgrade can hit

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

use super::latest;

/// The pattern every revision id matches
pub const REVISION_PATTERN: &str = r"^[0-9]{4}-[0-9]{2}-v[0-9]{2}$";

/// A revision id such as `2026-10-v02`
///
/// Ids are zero padded so sorting them as strings sorts them in upgrade order.
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
}

impl FromStr for RevisionId {
    /// A revision id that isn't `YYYY-MM-vNN` fails with an [`UpgradeError`]
    type Err = UpgradeError;

    /// Parse a revision id, rejecting anything that isn't `YYYY-MM-vNN`
    ///
    /// # Arguments
    ///
    /// * `raw` - The string to parse
    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        // split the id into its year, month, and number
        let bytes = raw.as_bytes();
        let digits = |range: std::ops::Range<usize>| bytes[range].iter().all(u8::is_ascii_digit);
        // check every part of the id is where it should be
        let valid = bytes.len() == 11
            && digits(0..4)
            && bytes[4] == b'-'
            && digits(5..7)
            && bytes[7] == b'-'
            && bytes[8] == b'v'
            && digits(9..11)
            && (b"01".as_slice()..=b"12".as_slice()).contains(&&bytes[5..7]);
        if valid {
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
    /// A revision id isn't `YYYY-MM-vNN`
    InvalidRevision(String),
    /// A revision id is well formed but not in this build's catalog
    UnknownRevision(String),
    /// The cluster is at a newer revision than this build knows about
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
    /// The target revision is older than the revision the cluster is at
    Backwards {
        /// The revision the cluster is at
        current: RevisionId,
        /// The requested target revision
        target: RevisionId,
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
            Self::InvalidRevision(raw) => {
                write!(f, "Revision {raw:?} is not a revision id like 2026-10-v02")
            }
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
                 build's latest revision {latest}; deploy a Thorium build that knows \
                 revision {current} (downgrades are not supported)"
            ),
            // state written in a newer layout
            Self::SchemaAhead { found, supported } => write!(
                f,
                "This cluster's upgrade state (schema version {found}) was written by a newer \
                 Thorium operator than this one (which reads up to schema version {supported}); \
                 downgrades are not supported, so deploy that newer Thorium operator again"
            ),
            // a target behind the cluster's revision
            Self::Backwards { current, target } => write!(
                f,
                "The target revision {target} is older than this cluster's revision \
                 {current}; upgrades never move backwards"
            ),
            // a pre-release from a branch this build doesn't carry
            Self::UnknownPrerelease { current, base } => write!(
                f,
                "This cluster is at pre-release revision {current}, which this Thorium build \
                 doesn't know; the image of the branch that applied it must revert it to \
                 {base} first"
            ),
            // a target the revision graph can't reach
            Self::NoPath { from, to } => write!(
                f,
                "This Thorium build's catalog has no path from revision {from} to revision {to}"
            ),
            // a downgrade across a step that can't be undone
            Self::Irreversible { revision, step } => write!(
                f,
                "Step {step} of revision {revision} can't be undone; restore a backup taken \
                 before it instead of downgrading"
            ),
            // a cluster older than this build's support floor
            Self::BelowMinimum {
                current,
                minimum,
                intermediate,
            } => write!(
                f,
                "This cluster is at revision {current}, below this Thorium build's minimum \
                 supported revision {minimum}; install release {intermediate} first, then \
                 follow the table of valid upgrade paths"
            ),
        }
    }
}

impl std::error::Error for UpgradeError {}
