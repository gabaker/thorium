//! The identity of this event handler process
//!
//! Every event handler replica owns the events it pops under a worker id so a
//! dead replicas events can be reclaimed. The id is `{hostname}-{8 hex random}`
//! generated once per process; in k8s the hostname is the pod name and the
//! fresh random suffix per boot means a restarted process never collides with
//! its prior self (the reaper and SIGTERM drain recover the old ids events).

use std::sync::LazyLock;
use uuid::Uuid;

/// The max length the sanitized hostname may take in a worker id
///
/// The server validates worker ids against `^[A-Za-z0-9._-]{1,128}$` so the
/// hostname is bounded to leave room for the dash and the 8 char suffix.
const MAX_HOSTNAME_LEN: usize = 119;

/// The worker id for this event handler process
static WORKER_ID: LazyLock<String> = LazyLock::new(build_worker_id);

/// Get this processes worker id
///
/// The id is generated once on first use and is stable for the life of this
/// process.
pub fn worker_id() -> &'static str {
    // return our process wide worker id
    &WORKER_ID
}

/// Build a worker id in the form `{hostname}-{8 hex random}`
fn build_worker_id() -> String {
    // get this hosts name; in k8s this is the pod name
    let raw = gethostname::gethostname();
    // worker ids ride into redis keys so flatten the hostname onto the
    // charset the server side validation allows
    let hostname = sanitize_hostname(&raw.to_string_lossy());
    // take 8 random hex chars from a v4 uuid as this processes suffix
    let random = Uuid::new_v4().simple().to_string();
    // combine our hostname and random suffix into a worker id
    format!("{hostname}-{}", &random[..8])
}

/// Sanitize a hostname onto the servers worker id charset
///
/// # Arguments
///
/// * `raw` - The raw hostname to sanitize
fn sanitize_hostname(raw: &str) -> String {
    // replace any char the server side worker id validation rejects
    let mut clean = raw
        .chars()
        .map(|ch| {
            // keep chars in [A-Za-z0-9._-] and flatten the rest to dashes
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>();
    // bound the hostname so the full id stays within the servers 128 char cap
    clean.truncate(MAX_HOSTNAME_LEN);
    // an empty hostname still needs some prefix to stay a valid id
    if clean.is_empty() {
        clean.push_str("unknown");
    }
    clean
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The worker id must be `{hostname}-{8 hex}` on the servers charset and
    /// stable across calls within one process
    #[test]
    fn worker_id_format() {
        // get this processes worker id
        let id = worker_id();
        // the id must be stable across calls within one process
        assert_eq!(id, worker_id());
        // the id must fit the servers `^[A-Za-z0-9._-]{1,128}$` validation
        assert!(!id.is_empty() && id.len() <= 128);
        assert!(
            id.chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
        );
        // the id must end in a dash separated 8 char hex suffix
        let (hostname, suffix) = id.rsplit_once('-').expect("worker id has no dash");
        assert!(!hostname.is_empty());
        assert_eq!(suffix.len(), 8);
        assert!(suffix.chars().all(|ch| ch.is_ascii_hexdigit()));
    }

    /// Hostnames with chars outside the servers charset must be flattened
    #[test]
    fn sanitize_hostname_flattens_bad_chars() {
        // chars outside [A-Za-z0-9._-] must become dashes
        assert_eq!(sanitize_hostname("bad host!"), "bad-host-");
        // allowed chars must pass through untouched
        assert_eq!(sanitize_hostname("pod-0.ns_x"), "pod-0.ns_x");
        // an empty hostname must still produce a valid prefix
        assert_eq!(sanitize_hostname(""), "unknown");
        // overlong hostnames must be bounded so the id stays under 128 chars
        let long = "a".repeat(500);
        assert_eq!(sanitize_hostname(&long).len(), MAX_HOSTNAME_LEN);
    }
}
