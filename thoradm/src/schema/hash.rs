//! `thoradm schema hash`: prints the declared fingerprints

use std::fmt::Write;
use thorium::models::schema::{SchemaObject, fingerprints};
use thorium::models::upgrades::SchemaBackend;

use crate::Error;
use crate::args::{Args, HashSchema};
use crate::outcome::RunOutcome;

/// Print the declared fingerprints
///
/// # Arguments
///
/// * `cmd` - Which backend to print the fingerprint of
/// * `args` - The arguments thoradm was started with
#[expect(
    clippy::unused_async,
    reason = "every schema handler is async so the dispatcher awaits them alike"
)]
pub(super) async fn handle(cmd: &HashSchema, args: &Args) -> Result<RunOutcome, Error> {
    // the fingerprints come from this build alone, so no config or backend is read
    let _ = args;
    print!("{}", hash_text(cmd.backend)?);
    Ok(RunOutcome::Complete)
}

/// Render the declared fingerprints: one `<name> <fingerprint>` line for the deployment and
/// each backend, or only the named backend's fingerprint
///
/// # Arguments
///
/// * `backend` - The one backend to print, if any
fn hash_text(backend: Option<SchemaBackend>) -> Result<String, Error> {
    // fingerprint the declared schema the way `schema apply` records it
    let (deployment, backends) = fingerprints(&SchemaObject::declared());
    // one backend prints only its fingerprint
    if let Some(backend) = backend {
        return backends
            .get(&backend)
            .map(|fingerprint| format!("{}\n", fingerprint.as_str()))
            .ok_or_else(|| Error::Usage(format!("no {backend:?} schema is declared")));
    }
    // otherwise the deployment's line comes first, then each backend's in order
    let mut text = format!("deployment {}\n", deployment.as_str());
    for (backend, fingerprint) in &backends {
        let name = serde_json::to_value(backend)?;
        let name = name.as_str().unwrap_or_default();
        writeln!(text, "{name} {}", fingerprint.as_str())
            .map_err(|err| Error::Generic(err.to_string()))?;
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use thorium::models::schema::RevisionSnapshot;
    use thorium::models::upgrades::{SchemaBackend, latest};

    use super::hash_text;

    /// The deployment line is the fingerprint `schema apply` records, and every backend has a line
    #[test]
    fn hash_prints_the_declared_fingerprints() {
        // the declared snapshot holds the fingerprints apply records
        let snapshot = RevisionSnapshot::declared(latest());
        let text = hash_text(None).expect("the hash renders");
        let mut expected = vec![format!("deployment {}", snapshot.deployment.as_str())];
        for (backend, name) in [
            (SchemaBackend::Scylla, "scylla"),
            (SchemaBackend::Elastic, "elastic"),
            (SchemaBackend::Redis, "redis"),
            (SchemaBackend::S3, "s3"),
        ] {
            expected.push(format!("{name} {}", snapshot.backends[&backend].as_str()));
        }
        assert_eq!(text.lines().collect::<Vec<_>>(), expected);
        // one backend prints only its fingerprint
        assert_eq!(
            hash_text(Some(SchemaBackend::Redis)).expect("the hash renders"),
            format!("{}\n", snapshot.backends[&SchemaBackend::Redis].as_str())
        );
    }
}
