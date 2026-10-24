//! `thoradm schema diff`: compares the live schema with the recorded revision
//!
//! The command is read-only. It probes Scylla, the configured Elastic indexes, and the recorded
//! Redis and S3 layouts, and prints each difference from the snapshot of the revision
//! `schema_migrations` records, as text or, with `--json`, as the whole report. Objects the legacy
//! registry lists, and those `--acknowledged` names, count as acknowledged. It exits 0 when every
//! backend is `ok`, 7 when an unacknowledged object differs (the report is still printed in full),
//! 3 when Scylla can't be reached, and 4 when a read fails in a way a retry may get past.

use std::fmt::Write;
use thorium::models::schema::{
    Acknowledgement, DriftReport, ObjectDrift, ProbeError, RecordStatus, converged_revision,
    diff_live, read_status,
};
use thorium::models::upgrades::{BackendSchemaStatus, Catalog, DriftKind, backend_name};

use crate::Error;
use crate::args::{Args, DiffSchema};
use crate::outcome::RunOutcome;

/// Convert a probe error into thoradm's error, whose exit code scripts and Jobs act on
///
/// # Arguments
///
/// * `err` - The probe error
pub(super) fn probe_error(err: &ProbeError) -> Error {
    match err {
        // a failed read may succeed when retried
        ProbeError::Query { statement, error } => {
            Error::Transient(format!("the probe failed to read {statement}: {error}"))
        }
        // a definition that can't be read, or a revision without a snapshot, won't change
        ProbeError::Unreadable { object, reason } => {
            Error::Generic(format!("the probe can't read {object}: {reason}"))
        }
    }
}

/// Parse a list of acknowledgements in the `drift_acknowledged` form
///
/// # Arguments
///
/// * `json` - The list as JSON
/// * `source` - What the list came from, for the error message
pub(super) fn parse_acknowledgements(
    json: &str,
    source: &str,
) -> Result<Vec<Acknowledgement>, Error> {
    // a list that doesn't parse is the caller's to fix
    serde_json::from_str(json).map_err(|err| {
        Error::Usage(format!(
            "{source} isn't a list of acknowledgements ({{backend, object, fingerprint}}): {err}"
        ))
    })
}

/// Name a kind of drift the way the report prints it
///
/// # Arguments
///
/// * `kind` - The kind of drift
fn kind_name(kind: DriftKind) -> &'static str {
    // the serialized lowercase names
    match kind {
        DriftKind::Missing => "missing",
        DriftKind::Changed => "changed",
        DriftKind::Unexpected => "unexpected",
        DriftKind::Unknown => "unknown",
    }
}

/// Describe one drifted object on one line, with its fields on the lines after it
///
/// # Arguments
///
/// * `object` - The drifted object
fn describe(object: &ObjectDrift) -> String {
    // the kind and the object, with how an acknowledgement stands
    let mut text = format!("  {:<11} {}", kind_name(object.kind), object.object);
    if object.acknowledged {
        text.push_str(" (acknowledged)");
    } else if object.lapsed {
        text.push_str(" (its acknowledgement lapsed: the object changed)");
    }
    // the fingerprint an acknowledgement can name
    if let Some(fingerprint) = &object.fingerprint {
        let _ = write!(text, "\n      fingerprint {}", fingerprint.as_str());
    }
    // what each differing field was expected to be and was observed as
    for field in &object.fields {
        let show = |value: &Option<String>| value.clone().unwrap_or_else(|| "absent".to_owned());
        let _ = write!(
            text,
            "\n      {}: expected {}, observed {}",
            field.field,
            show(&field.expected),
            show(&field.observed)
        );
    }
    text
}

/// Render a report as text: the revision and fingerprints, then each backend with its objects
///
/// # Arguments
///
/// * `report` - The drift report
#[must_use]
pub(super) fn render(report: &DriftReport) -> String {
    // the revision and the two deployment fingerprints
    let mut text = format!(
        "revision {} (expected deployment fingerprint {}, observed {})\n",
        report.revision,
        report.expected.as_str(),
        report.observed.as_str()
    );
    // each backend's status, then its objects
    for (backend, found) in &report.backends {
        let status = match found.status {
            BackendSchemaStatus::Ok => "ok",
            BackendSchemaStatus::Drift => "drift",
            BackendSchemaStatus::Unknown => "unknown",
        };
        let _ = writeln!(text, "{:<8} {status}", backend_name(*backend));
        for object in &found.objects {
            let _ = writeln!(text, "{}", describe(object));
        }
    }
    text
}

/// Whether any backend of a report has an unacknowledged difference
///
/// # Arguments
///
/// * `report` - The drift report
fn drifted(report: &DriftReport) -> bool {
    // a backend that isn't ok has one
    report
        .backends
        .values()
        .any(|found| found.status != BackendSchemaStatus::Ok)
}

/// Compare the live schema with the recorded revision's snapshot
///
/// # Arguments
///
/// * `cmd` - How to report the drift
/// * `args` - The arguments thoradm was started with
pub(super) async fn handle(cmd: &DiffSchema, args: &Args) -> Result<RunOutcome, Error> {
    // a config or an acknowledgement file that can't be used is the caller's to fix
    let conf = super::load_conf(args)?;
    let acks = match &cmd.acknowledged {
        Some(path) => {
            let json = std::fs::read_to_string(path)
                .map_err(|err| Error::Usage(format!("can't read {}: {err}", path.display())))?;
            parse_acknowledgements(&json, &path.display().to_string())?
        }
        None => Vec::new(),
    };
    // read the record, which names the revision to compare with
    let session = super::legacy::connect(&conf).await?;
    let status = read_status(&session, &conf, &Catalog::PRODUCTION)
        .await
        .map_err(|err| Error::Transient(err.to_string()))?;
    let RecordStatus::Recorded { record, .. } = status else {
        return Err(Error::Generic(format!(
            "there is no recorded revision to compare with ({status:?}); run `thoradm schema \
             apply` first"
        )));
    };
    // probe the live backends and compare them with that revision's snapshot
    // an apply that ran ahead of the record converged to the next revision's schema
    let revision = converged_revision(
        &Catalog::PRODUCTION,
        &record.revision,
        &record.schema.deployment,
    );
    let report = diff_live(&session, &conf, &revision, &acks)
        .await
        .map_err(|err| probe_error(&err))?;
    // print the whole report
    if cmd.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", render(&report));
    }
    // drift is the one outcome besides success that still prints a full report, and it exits with
    // a code of its own so a script can tell it from a usage error
    if drifted(&report) {
        Err(Error::Drift(format!(
            "the live schema differs from revision {} (the report is above)",
            report.revision
        )))
    } else {
        Ok(RunOutcome::Complete)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use thorium::models::schema::{
        BackendDrift, DriftReport, FieldDrift, ObjectDrift, RevisionSnapshot,
    };
    use thorium::models::upgrades::{BackendSchemaStatus, DriftKind, SchemaBackend, latest};

    use super::{drifted, parse_acknowledgements, render};

    /// Build a report with a changed column on Scylla and nothing on Redis
    fn report() -> DriftReport {
        // a changed column with both types, and a Redis backend that matches
        let snapshot = RevisionSnapshot::declared(latest());
        let changed = ObjectDrift {
            backend: SchemaBackend::Scylla,
            object: "samples_list.uploaded".to_owned(),
            kind: DriftKind::Changed,
            fields: vec![FieldDrift {
                field: "uploaded".to_owned(),
                expected: Some("timestamp".to_owned()),
                observed: Some("text".to_owned()),
            }],
            acknowledged: false,
            lapsed: false,
            fingerprint: None,
        };
        DriftReport {
            revision: snapshot.revision,
            expected: snapshot.deployment.clone(),
            observed: snapshot.deployment,
            backends: BTreeMap::from([
                (
                    SchemaBackend::Scylla,
                    BackendDrift {
                        status: BackendSchemaStatus::Drift,
                        objects: vec![changed],
                    },
                ),
                (
                    SchemaBackend::Redis,
                    BackendDrift {
                        status: BackendSchemaStatus::Ok,
                        objects: Vec::new(),
                    },
                ),
            ]),
        }
    }

    /// The text names the revision, each backend's status, and each object with its fields
    #[test]
    fn the_text_lists_each_backend_and_object() {
        // render a report with one changed column
        let text = render(&report());
        assert!(text.starts_with("revision "), "{text}");
        assert!(text.contains("scylla   drift\n"), "{text}");
        assert!(
            text.contains("  changed     samples_list.uploaded\n"),
            "{text}"
        );
        assert!(
            text.contains("uploaded: expected timestamp, observed text"),
            "{text}"
        );
        assert!(text.contains("redis    ok\n"), "{text}");
    }

    /// Any backend that isn't `ok` makes the report drift
    #[test]
    fn a_backend_that_isnt_ok_is_drift() {
        // the report drifts, and clearing the one drifting backend clears it
        let mut found = report();
        assert!(drifted(&found));
        found
            .backends
            .get_mut(&SchemaBackend::Scylla)
            .unwrap()
            .status = BackendSchemaStatus::Ok;
        assert!(!drifted(&found));
    }

    /// A list that isn't acknowledgements is a usage error naming its source
    #[test]
    fn a_bad_acknowledgement_list_is_a_usage_error() {
        // an object instead of a list
        let err = parse_acknowledgements("{}", "ack.json").expect_err("refused");
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("ack.json"), "{err}");
        // a list parses
        let parsed =
            parse_acknowledgements(r#"[{"backend": "scylla", "object": "x"}]"#, "ack.json")
                .expect("parses");
        assert_eq!(parsed.len(), 1);
    }
}
