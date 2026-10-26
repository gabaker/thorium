//! Tests for field-level diffs and the YAML text diff

use serde_json::json;

use crate::handlers::edit::diff::{diff_views, render_diff};
use crate::handlers::edit::flow::is_static_key;

/// Nothing is reported for equal documents
#[test]
fn equal_documents_have_no_changes() {
    let doc = json!({"a": 1, "b": {"c": [1, 2]}});
    let changes = diff_views(&doc, &doc);
    assert!(changes.is_empty(), "{changes:?}");
}

/// Nested changes are reported with dotted paths, in sorted order
#[test]
fn nested_changes_have_dotted_paths() {
    let old = json!({"name": "a", "config": {"max_turns": 3, "mode": "x"}});
    let new = json!({"name": "b", "config": {"max_turns": 5, "mode": "x"}});
    let paths: Vec<_> = diff_views(&old, &new)
        .into_iter()
        .map(|c| (c.path, c.old, c.new))
        .collect();
    assert_eq!(
        paths,
        vec![
            ("config.max_turns".to_string(), json!(3), json!(5)),
            ("name".to_string(), json!("a"), json!("b")),
        ]
    );
}

/// Added and removed keys report null on the missing side
#[test]
fn added_and_removed_keys_report_null() {
    let changes = diff_views(&json!({"a": 1}), &json!({"b": 2}));
    assert_eq!(changes.len(), 2);
    assert_eq!(changes[0].path, "a");
    assert_eq!(changes[0].new, json!(null));
    assert_eq!(changes[1].path, "b");
    assert_eq!(changes[1].old, json!(null));
}

/// Arrays of the same length diff by element and others are reported whole
#[test]
fn arrays_diff_by_element_or_whole() {
    let same_len = diff_views(&json!({"g": ["a", "b"]}), &json!({"g": ["a", "c"]}));
    assert_eq!(same_len.len(), 1);
    assert_eq!(same_len[0].path, "g[1]");
    let longer = diff_views(&json!({"g": ["a"]}), &json!({"g": ["a", "b"]}));
    assert_eq!(longer.len(), 1);
    assert_eq!(longer[0].path, "g");
    assert_eq!(longer[0].new, json!(["a", "b"]));
}

/// The text diff marks removed and added lines and keeps nearby context
#[test]
fn text_diff_marks_lines() {
    let old = json!({"name": "a", "description": "old", "groups": ["g"]});
    let new = json!({"name": "a", "description": "new", "groups": ["g"]});
    let text = render_diff(&old, &new, &["name", "description", "groups"]).unwrap();
    assert!(text.contains("- description: old"), "{text}");
    assert!(text.contains("+ description: new"), "{text}");
    assert!(text.contains("  name: a"), "{text}");
}

/// Marked keys and listed names are read-only
#[test]
fn static_keys() {
    assert!(is_static_key("*id*", &[]));
    assert!(is_static_key("created", &["created"]));
    assert!(!is_static_key("name", &["created"]));
    assert!(!is_static_key("**", &[]));
}
