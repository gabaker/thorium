//! Tests of the applied report a pass row stores: its bounds and its defaults

use super::{
    AppliedReport, MAX_REPORT_BYTES, MAX_REPORT_NOTES, MAX_REPORT_SKIPPED, MAX_REPORT_STRING,
    PassCheckpoint, ReportCounts, ReportNote, SkippedItem,
};

/// A report with `skipped` skipped items and `notes` notes whose strings are `width` bytes long
///
/// # Arguments
///
/// * `skipped` - How many skipped items
/// * `notes` - How many notes
/// * `width` - How long each string is
fn report(skipped: usize, notes: usize, width: usize) -> AppliedReport {
    // totals that count more than the lists hold, as a real pass's do
    let text = |what: &str, n: usize| format!("{what}{n}{}", "x".repeat(width));
    AppliedReport {
        groups: [(
            "g".to_owned(),
            ReportCounts {
                changed: 3,
                ..ReportCounts::default()
            },
        )]
        .into(),
        skipped_total: skipped as u64 + 7,
        skipped: (0..skipped)
            .map(|n| SkippedItem {
                group: text("g", n),
                item: text("i", n),
                reason: text("r", n),
            })
            .collect(),
        warnings_total: notes as u64,
        notes: (0..notes)
            .map(|n| ReportNote {
                group: text("g", n),
                item: text("i", n),
                note: text("n", n),
                warning: true,
            })
            .collect(),
        mappings_total: 42,
    }
}

/// Every string is cut to its bound, on a character boundary
#[test]
fn strings_are_cut_on_a_character_boundary() {
    // a string of two-byte characters that a byte cut would split
    let mut long = report(1, 1, 0);
    long.skipped[0].item = "é".repeat(300);
    long.notes[0].note = "é".repeat(300);
    long.bound();
    for text in [&long.skipped[0].item, &long.notes[0].note] {
        assert!(text.len() <= MAX_REPORT_STRING, "{}", text.len());
        assert!(text.chars().all(|c| c == 'é'));
    }
}

/// The skipped list and the notes are capped, and the totals still count everything
#[test]
fn the_lists_are_capped_and_the_totals_kept() {
    // more of each than the report lists
    let mut big = report(MAX_REPORT_SKIPPED + 100, MAX_REPORT_NOTES + 100, 4);
    big.bound();
    assert_eq!(big.skipped.len(), MAX_REPORT_SKIPPED);
    assert_eq!(big.notes.len(), MAX_REPORT_NOTES);
    assert_eq!(big.skipped_total, (MAX_REPORT_SKIPPED + 100) as u64 + 7);
    assert_eq!(big.warnings_total, (MAX_REPORT_NOTES + 100) as u64);
    assert_eq!(big.mappings_total, 42);
}

/// Over budget, the notes go first and then the skipped list is trimmed, and the totals stay
#[test]
fn an_over_budget_report_drops_notes_before_trimming_skipped() {
    // full-width strings make the capped lists bigger than the stored budget
    let mut big = report(MAX_REPORT_SKIPPED, MAX_REPORT_NOTES, MAX_REPORT_STRING);
    big.bound();
    let json = serde_json::to_string(&big).unwrap();
    assert!(json.len() <= MAX_REPORT_BYTES, "{} bytes", json.len());
    assert_eq!(big.notes.len(), 0);
    assert!(!big.skipped.is_empty() && big.skipped.len() < MAX_REPORT_SKIPPED);
    assert_eq!(big.skipped_total, MAX_REPORT_SKIPPED as u64 + 7);
    assert_eq!(big.groups["g"].changed, 3);
    // a report that fits keeps its notes
    let mut small = report(10, 10, 8);
    small.bound();
    assert_eq!((small.skipped.len(), small.notes.len()), (10, 10));
}

/// The summary keeps the counts and totals and no list or name
#[test]
fn the_summary_drops_every_list() {
    // the lists never reach a check result
    let summary = report(5, 5, 8).summary();
    assert_eq!(summary.groups["g"].changed, 3);
    assert_eq!((summary.skipped_total, summary.warnings), (12, 5));
    let json = serde_json::to_string(&summary).unwrap();
    assert!(!json.contains("i0"), "{json}");
}

/// A pass row or checkpoint written before these fields existed still reads
#[test]
fn older_rows_without_a_report_still_read() {
    // an empty report and a checkpoint without per-group counts
    let empty: AppliedReport = serde_json::from_str("{}").unwrap();
    assert_eq!(empty, AppliedReport::default());
    let old: PassCheckpoint = serde_json::from_str(r#"{"chain":["a"],"slices":2}"#).unwrap();
    assert!(old.report_counts.is_empty());
    assert_eq!(old.slices, 2);
}

/// A status report carries a check Job's report beside a record result, and a report written
/// before the field existed reads without one
#[test]
fn a_status_report_carries_a_check_report() {
    use super::{CheckReport, PassCounts, StatusReport};
    // a report as an older thoradm printed it, with no check
    let old = serde_json::json!({
        "status": {"state": "no_keyspace"},
        "catalog": {"latest": "2026-10-v02", "hash": "abc"},
    });
    let parsed: StatusReport = serde_json::from_value(old).expect("an older report parses");
    assert_eq!(parsed.check, None);
    // a report carrying a check report round trips, and one without leaves the field out
    let check = CheckReport {
        migration: "fixture-theme".to_owned(),
        counts: PassCounts {
            read: 3,
            migratable: 2,
            ..PassCounts::default()
        },
        families: Vec::new(),
        rows: std::collections::BTreeMap::new(),
        estimated_seconds: Some(4),
        reasons: Vec::new(),
        more: 0,
        wait: None,
        summary: None,
        loss: None,
    };
    let with = StatusReport {
        check: Some(check.clone()),
        ..parsed.clone()
    };
    let json = serde_json::to_value(&with).expect("serializes");
    assert_eq!(json["check"]["migration"], "fixture-theme");
    assert_eq!(
        serde_json::from_value::<StatusReport>(json).expect("parses"),
        with
    );
    assert!(
        serde_json::to_value(&parsed)
            .expect("serializes")
            .get("check")
            .is_none()
    );
}

/// The work text names the class counts of a pass, so the entry thoradm writes and the one the
/// operator records say the same thing
#[test]
fn work_text_names_the_class_counts() {
    use super::PassCounts;
    let counts = PassCounts {
        read: 10,
        current: 4,
        migratable: 3,
        unrecognized: 2,
        error: 1,
        written: 3,
        ..PassCounts::default()
    };
    assert_eq!(
        counts.work_text(),
        "10 objects read: 4 current, 3 migratable, 2 unrecognized, 1 error; 3 written"
    );
}
