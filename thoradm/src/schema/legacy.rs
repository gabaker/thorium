//! `thoradm schema legacy`: lists the legacy objects older releases created and the objects no
//! command can drop
//!
//! The command is read-only. It prints two sections and a count, as text or, with `--json`, as one
//! JSON document. It exits 0 when it lists, 2 when the config can't be used, 3 when Scylla is
//! unavailable, and 4 when a read fails in a way a retry may get past.

use scylla::client::session::Session;
use std::fmt::Write;
use thorium::Conf;
use thorium::models::schema::{
    Droppable, LegacyError, LegacyListing, LegacySources, LegacyStatus, ProbeError, inspect,
};
use thorium::models::upgrades::LegacyNeed;

use crate::Error;
use crate::args::{Args, LegacySchema};
use crate::outcome::RunOutcome;

/// Convert a legacy error into thoradm's error, whose exit code scripts and Jobs act on
///
/// # Arguments
///
/// * `err` - The legacy error
pub(super) fn legacy_error(err: &LegacyError) -> Error {
    match err {
        // a failed read or statement may succeed when retried
        LegacyError::Probe(ProbeError::Query { .. }) | LegacyError::Query { .. } => {
            Error::Transient(err.to_string())
        }
        // a refusal is final, a definition that can't be read is not worth retrying, and a drop
        // that happened but wasn't recorded must not be repeated blindly
        LegacyError::Refused(_)
        | LegacyError::Probe(ProbeError::Unreadable { .. })
        | LegacyError::NotRecorded { .. } => Error::Generic(err.to_string()),
    }
}

/// Connect to Scylla, as an unavailable-backend error when it can't be reached
///
/// # Arguments
///
/// * `conf` - The Thorium config naming Scylla
pub(super) async fn connect(conf: &Conf) -> Result<Session, Error> {
    // an unreachable Scylla exits with the code for an unavailable backend
    crate::shared::scylla::get_client(conf)
        .await
        .map_err(|err| Error::Unavailable(format!("Scylla can't be reached: {err}")))
}

/// Render the revisions that use an object, or `none`
///
/// # Arguments
///
/// * `status` - The object's status
fn used_by(status: &LegacyStatus) -> String {
    // an object nothing uses says so
    if status.used_by.is_empty() {
        return "none".to_owned();
    }
    // each revision with whether its use can be rebuilt
    status
        .used_by
        .iter()
        .map(|(revision, need)| match need {
            LegacyNeed::Rebuildable => format!("{revision} (rebuildable)"),
            LegacyNeed::Required => format!("{revision} (required)"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Render one registered object's block of the first section
///
/// # Arguments
///
/// * `text` - The text to append to
/// * `status` - The object's status
fn render_status(text: &mut String, status: &LegacyStatus) {
    // the id with its kind and class, then what the keyspace holds
    let _ = writeln!(
        text,
        "  {}  ({}, class {:?})",
        status.id,
        status.kind.label(),
        status.class
    );
    let present = if status.present { "yes" } else { "no" };
    let _ = writeln!(text, "    present:     {present}");
    let _ = writeln!(text, "    rows:        {}", status.rows);
    let _ = writeln!(text, "    created by:  {}", status.created_by.join(", "));
    let _ = writeln!(text, "    used by:     {}", used_by(status));
    // one line per downgrade target
    for target in &status.downgrades {
        let _ = writeln!(
            text,
            "    downgrade to {}: {}",
            target.revision,
            target.outcome.describe(status.kind)
        );
    }
    // and whether it may be dropped, with the reason when not
    match &status.droppable {
        Droppable::Yes => text.push_str("    droppable:   yes\n"),
        Droppable::No { reason } => {
            let _ = writeln!(text, "    droppable:   no: {reason}");
        }
    }
}

/// Render a listing as the text `thoradm schema legacy` prints
///
/// # Arguments
///
/// * `listing` - What the keyspace holds
#[must_use]
pub fn render(listing: &LegacyListing) -> String {
    // section 1: one block per registered object
    let mut text = String::from("Legacy objects\n");
    for status in &listing.objects {
        render_status(&mut text, status);
    }
    // section 2: live objects named in past snapshots that nothing registers
    text.push_str("\nNot registered (cannot be dropped by this command)\n");
    if listing.not_registered.is_empty() {
        text.push_str("  none\n");
    }
    for object in &listing.not_registered {
        let _ = writeln!(text, "  scylla:{}  ({})", object.name, object.kind);
        let _ = writeln!(
            text,
            "    in the snapshots of:  {}",
            object.snapshots.join(", ")
        );
        text.push_str(
            "    why it cannot be dropped:  it is live and named in past snapshots, is not \
             declared, and has no LEGACY entry, so this command does not know whether any \
             revision needs it\n",
        );
        text.push_str(
            "    next step:  ask a developer to register it as legacy or declare it again\n",
        );
    }
    // the count of objects nothing names
    text.push('\n');
    match listing.unmanaged_extras {
        0 => text.push_str("no unmanaged extra objects\n"),
        1 => text.push_str("1 unmanaged extra object (thoradm schema diff lists it)\n"),
        count => {
            let _ = writeln!(
                text,
                "{count} unmanaged extra objects (thoradm schema diff lists them)"
            );
        }
    }
    text
}

/// List the legacy objects the keyspace holds
///
/// # Arguments
///
/// * `cmd` - How to print the listing
/// * `args` - The arguments thoradm was started with
pub(super) async fn handle(cmd: &LegacySchema, args: &Args) -> Result<RunOutcome, Error> {
    // the config must load before anything is contacted
    let conf = super::load_conf(args)?;
    let session = connect(&conf).await?;
    // read the keyspace against what this build knows
    let listing = inspect(&session, &conf, &LegacySources::production())
        .await
        .map_err(|err| legacy_error(&err))?;
    // print it as asked
    if cmd.json {
        println!("{}", serde_json::to_string_pretty(&listing)?);
    } else {
        print!("{}", render(&listing));
    }
    Ok(RunOutcome::Complete)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use thorium::models::schema::{
        LegacyError, LegacyListing, LegacyRefusal, LegacySources, NotRegistered, ProbeError,
        classify,
    };
    use thorium::models::upgrades::SchemaBackend;

    use super::{legacy_error, render};
    use crate::Error;

    /// Build a listing over the real registry for a keyspace holding both objects, a table a past
    /// snapshot names, and one table no snapshot names
    fn listing() -> LegacyListing {
        // both exemplars live with rows, plus an unregistered table in no snapshot
        let sources = LegacySources::production();
        let live = [
            ("events", "table"),
            ("entities_by_name", "view"),
            ("hand_made", "table"),
        ]
        .iter()
        .map(|(name, kind)| ((*name).to_owned(), (*kind).to_owned()))
        .collect::<BTreeMap<_, _>>();
        let rows = BTreeMap::from([
            ("events".to_owned(), 18204),
            ("entities_by_name".to_owned(), 412_907),
        ]);
        let mut listing = classify(&sources, &live, &rows);
        // a table a past snapshot names
        listing.not_registered.push(NotRegistered {
            backend: SchemaBackend::Scylla,
            name: "tool_runs".to_owned(),
            kind: "table".to_owned(),
            snapshots: vec!["2026-10-v01".to_owned(), "2026-10-v02".to_owned()],
        });
        listing
    }

    /// The text has the two sections, a block per registered object, and the count line
    #[test]
    fn the_text_has_two_sections_and_a_count() {
        let text = render(&listing());
        // the expected lines, joined, with the trailing newline the command prints
        let expected = [
            "Legacy objects",
            "  scylla:events  (table, class Runtime)",
            "    present:     yes",
            "    rows:        18204",
            "    created by:  2026-10-v01, 2026-10-v02",
            "    used by:     none",
            "    downgrade to 2026-10-v03: recreated (it does not use this object)",
            "    downgrade to 2026-10-v02: recreated (it does not use this object)",
            "    downgrade to 2026-10-v01: recreated (it does not use this object)",
            "    droppable:   yes",
            "  scylla:entities_by_name  (view, class Derived)",
            "    present:     yes",
            "    rows:        412907",
            "    created by:  2026-10-v01, 2026-10-v02",
            "    used by:     none",
            "    downgrade to 2026-10-v03: recreated (it does not use this object)",
            "    downgrade to 2026-10-v02: recreated (it does not use this object)",
            "    downgrade to 2026-10-v01: recreated (it does not use this object)",
            "    droppable:   yes",
            "",
            "Not registered (cannot be dropped by this command)",
            "  scylla:tool_runs  (table)",
            "    in the snapshots of:  2026-10-v01, 2026-10-v02",
            "    why it cannot be dropped:  it is live and named in past snapshots, is not declared, and has no LEGACY entry, so this command does not know whether any revision needs it",
            "    next step:  ask a developer to register it as legacy or declare it again",
            "",
            "1 unmanaged extra object (thoradm schema diff lists it)",
        ]
        .join("\n")
            + "\n";
        assert_eq!(text, expected);
    }

    /// An empty section 2 says none, and the count line follows the number of extras
    #[test]
    fn empty_sections_say_so() {
        let mut empty = listing();
        empty.not_registered.clear();
        empty.unmanaged_extras = 0;
        let text = render(&empty);
        assert!(
            text.contains("Not registered (cannot be dropped by this command)\n  none\n"),
            "{text}"
        );
        assert!(text.ends_with("no unmanaged extra objects\n"), "{text}");
        empty.unmanaged_extras = 3;
        assert!(
            render(&empty)
                .ends_with("3 unmanaged extra objects (thoradm schema diff lists them)\n")
        );
    }

    /// A refusal exits 1, a failed read or statement 4, and a drop that wasn't recorded 1
    #[test]
    fn legacy_errors_map_to_exit_codes() {
        let refused = legacy_error(&LegacyError::Refused(LegacyRefusal::NoRecord));
        assert_eq!(refused.exit_code(), 1);
        let query = legacy_error(&LegacyError::Query {
            statement: "DROP".to_owned(),
            error: "timed out".to_owned(),
        });
        assert_eq!(query.exit_code(), 4);
        let probe = legacy_error(&LegacyError::Probe(ProbeError::Unreadable {
            object: "x".to_owned(),
            reason: "y".to_owned(),
        }));
        assert_eq!(probe.exit_code(), 1);
        let unrecorded = legacy_error(&LegacyError::NotRecorded {
            id: "scylla:events".to_owned(),
            error: "timed out".to_owned(),
        });
        assert_eq!(unrecorded.exit_code(), 1);
        // an unreachable backend exits 3, apart from a failed read
        assert_eq!(
            Error::Unavailable("Scylla can't be reached".to_owned()).exit_code(),
            3
        );
    }
}
