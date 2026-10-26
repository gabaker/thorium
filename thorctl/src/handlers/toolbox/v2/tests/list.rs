//! Tests for listing manifests

use super::super::list::{ListView, entries};
use super::{CLEAN_V2, SMALL_LEGACY, assert_golden, load_text};
use crate::args::toolbox::ListKind;
use crate::utils::output::{OutputFormat, render};

/// The list output matches its golden files in each format
#[tokio::test]
async fn list_output_matches_golden_files() {
    // list the clean fixture and the legacy fixture
    let clean = load_text(CLEAN_V2).await.expect("parse");
    let legacy = load_text(SMALL_LEGACY).await.expect("parse");
    let clean_view = ListView(entries(&clean.manifest, None));
    let legacy_view = ListView(entries(&legacy.manifest, None));
    // each format renders the documented output
    assert_golden(
        "list.table.golden",
        &render(&clean_view, OutputFormat::Table, false).expect("render"),
    );
    assert_golden(
        "list.human.golden",
        &render(&clean_view, OutputFormat::Human, false).expect("render"),
    );
    assert_golden(
        "list.json.golden",
        &render(&clean_view, OutputFormat::Json, false).expect("render"),
    );
    assert_golden(
        "list.legacy.table.golden",
        &render(&legacy_view, OutputFormat::Table, false).expect("render"),
    );
    // limiting to one kind keeps only that kind
    let tools = entries(&clean.manifest, Some(ListKind::Tools));
    assert_eq!(tools.len(), 3);
    assert!(tools.iter().all(|entry| entry.kind == "tool"));
    assert_eq!(
        entries(&clean.manifest, Some(ListKind::Builds))[0].versions,
        ["1"]
    );
}
