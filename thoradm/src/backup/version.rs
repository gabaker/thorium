//! The check that thoradm and the API it talks to run the same Thorium version
//!
//! thoradm and the API each read the Redis backup into their own copy of the `Backup` type, so
//! when they come from different versions, fields one side lacks are dropped without a word, and
//! an older API's restore deletes more of Redis than this thoradm says. A backup of the Redis
//! state and a restore that loads it or wipes therefore read the version the API reports in its
//! `thorium-version` header and refuse any but thoradm's own, the full string, unless
//! `--allow-version-mismatch` lets them go ahead; `verify --cluster` only warns.

use serde::{Deserialize, Serialize};
use thorium::{CtlConf, Thorium};
use tracing::warn;

/// The flag that lets a run go ahead against an API of another version
pub const ALLOW_FLAG: &str = "--allow-version-mismatch";

/// What the API reported when its Thorium version wasn't thoradm's
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionMismatch {
    /// thoradm's own version
    pub thoradm: String,
    /// The version the API reported, or `None` when its answer carried no readable version
    pub api: Option<String>,
}

impl VersionMismatch {
    /// Describes the mismatch, naming both versions
    #[must_use]
    pub fn describe(&self) -> String {
        // name the API's version, or say it sent none
        match &self.api {
            Some(api) => format!(
                "the API reports Thorium version {api}, but this thoradm is version {}",
                self.thoradm
            ),
            None => format!(
                "the API reported no version (its answer has no readable thorium-version \
                 header), and this thoradm is version {}",
                self.thoradm
            ),
        }
    }
}

/// Compares the version the API reported with thoradm's, returning the mismatch unless they are
/// the same string
///
/// # Arguments
///
/// * `thoradm` - thoradm's own version
/// * `api` - The version the API reported, if it reported one
#[must_use]
pub fn compare(thoradm: &str, api: Option<&str>) -> Option<VersionMismatch> {
    // only the same full string matches, patch and pre-release included
    if api == Some(thoradm) {
        return None;
    }
    Some(VersionMismatch {
        thoradm: thoradm.to_owned(),
        api: api.map(ToOwned::to_owned),
    })
}

/// Asks the API for its version and compares it with this thoradm's
///
/// An API that can't be asked reports no version, so it counts as a mismatch; why it couldn't be
/// asked is logged at WARN.
///
/// # Arguments
///
/// * `ctl_conf` - The Thorctl config of the API
pub async fn check_api(ctl_conf: &CtlConf) -> Option<VersionMismatch> {
    // ask the API, reading a failure as no version
    let reported = match Thorium::from_ctl_conf(ctl_conf.clone()).await {
        Ok(client) => client.basic.version().await,
        Err(error) => Err(error),
    };
    let api = reported.unwrap_or_else(|error| {
        warn!("Couldn't ask the API for its Thorium version: {error}");
        None
    });
    compare(env!("CARGO_PKG_VERSION"), api.as_deref())
}

/// Checks the API's version before a run that refuses a mismatch: `Ok(None)` when it matches,
/// `Ok(Some)` when `--allow-version-mismatch` lets the run go ahead (logged at WARN), and
/// otherwise the refusal, naming both versions and the flag
///
/// # Arguments
///
/// * `ctl_conf` - The Thorctl config of the API
/// * `allow` - Whether `--allow-version-mismatch` was given
/// * `run` - What the run does, for the messages (`back up the Redis state`)
pub async fn enforce(
    ctl_conf: &CtlConf,
    allow: bool,
    run: &str,
) -> Result<Option<VersionMismatch>, String> {
    // a matching API needs nothing more
    let Some(mismatch) = check_api(ctl_conf).await else {
        return Ok(None);
    };
    // the flag lets the run go ahead, with a warning
    if allow {
        warn!(
            "{}; going ahead to {run} because of {ALLOW_FLAG}, so Redis records or fields one \
             version lacks may be lost",
            mismatch.describe()
        );
        return Ok(Some(mismatch));
    }
    Err(format!(
        "{}; refusing to {run}, since thoradm and the API each read the Redis backup with their \
         own version of it and drop what theirs lacks; run the thoradm of the API's version, or \
         pass {ALLOW_FLAG} to {run} anyway",
        mismatch.describe()
    ))
}

/// Logs a mismatch between the API's version and thoradm's at WARN, for a run that goes on
/// either way
///
/// # Arguments
///
/// * `ctl_conf` - The Thorctl config of the API
/// * `run` - What the run does, for the message (`verify the cluster`)
pub async fn warn_on_mismatch(ctl_conf: &CtlConf, run: &str) {
    // a mismatch only warns
    if let Some(mismatch) = check_api(ctl_conf).await {
        warn!(
            "{}; going on to {run}, but the API's Redis state is read through a different \
             version than thoradm's, so differences may come from the versions",
            mismatch.describe()
        );
    }
}

/// Says what a run that `--allow-version-mismatch` let go ahead may have lost, for its report
///
/// # Arguments
///
/// * `mismatch` - The mismatch the flag accepted
#[must_use]
pub fn accepted(mismatch: &VersionMismatch) -> String {
    // name both versions and the flag
    format!(
        "{} ({ALLOW_FLAG}); Redis records or fields one version lacks may not have been restored",
        mismatch.describe()
    )
}

#[cfg(test)]
mod tests {
    use super::{VersionMismatch, compare};

    /// Only the same full string matches: another patch, a pre-release, a prefix, a shorter
    /// version, surrounding space, or no version at all is a mismatch naming what the API sent
    #[test]
    fn only_the_exact_version_matches() {
        // the same string matches
        assert_eq!(compare("1.8.1", Some("1.8.1")), None);
        // anything else is a mismatch naming both
        for api in ["1.8.2", "1.8.1-rc1", "v1.8.1", "1.8", " 1.8.1", "1.9.1", ""] {
            assert_eq!(
                compare("1.8.1", Some(api)),
                Some(VersionMismatch {
                    thoradm: "1.8.1".to_owned(),
                    api: Some(api.to_owned()),
                }),
                "{api:?}"
            );
        }
        // no version is a mismatch too
        assert_eq!(
            compare("1.8.1", None),
            Some(VersionMismatch {
                thoradm: "1.8.1".to_owned(),
                api: None,
            })
        );
    }

    /// The description names thoradm's version and the API's, or says the API reported none
    #[test]
    fn the_description_names_both_versions() {
        // a reported version is named beside thoradm's
        let named = compare("1.8.1", Some("1.7.0")).unwrap().describe();
        assert!(
            named.contains("1.8.1") && named.contains("1.7.0"),
            "{named}"
        );
        // a missing one is said to be missing
        let missing = compare("1.8.1", None).unwrap().describe();
        assert!(
            missing.contains("1.8.1") && missing.contains("no version"),
            "{missing}"
        );
    }
}
