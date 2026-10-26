//! Showing what an edit changes: per-field changes and a YAML text diff

use colored::Colorize;
use serde_json::Value;
use similar::{ChangeTag, TextDiff};
use thorium::Error;

use super::editable::FieldChange;
use crate::utils;

/// Append an object key to a dotted path
///
/// # Arguments
///
/// * `path` - The path so far
/// * `key` - The key to append
fn join_key(path: &str, key: &str) -> String {
    // the first key has no dot before it
    if path.is_empty() {
        key.to_string()
    } else {
        format!("{path}.{key}")
    }
}

/// Collect the leaf changes between two values
///
/// Objects are compared key by key. Arrays of the same length are compared element by element,
/// and arrays of different lengths are reported whole.
///
/// # Arguments
///
/// * `path` - The path of the values being compared
/// * `old` - The value before the edit
/// * `new` - The value after the edit
/// * `changes` - Where the changes are collected
fn collect(path: &str, old: &Value, new: &Value, changes: &mut Vec<FieldChange>) {
    match (old, new) {
        // nothing changed
        (old, new) if old == new => {}
        // compare objects key by key, in sorted order
        (Value::Object(old_map), Value::Object(new_map)) => {
            let mut keys: Vec<&String> = old_map.keys().chain(new_map.keys()).collect();
            keys.sort_unstable();
            keys.dedup();
            for key in keys {
                collect(
                    &join_key(path, key),
                    old_map.get(key).unwrap_or(&Value::Null),
                    new_map.get(key).unwrap_or(&Value::Null),
                    changes,
                );
            }
        }
        // compare arrays of the same length element by element
        (Value::Array(old_items), Value::Array(new_items))
            if old_items.len() == new_items.len() =>
        {
            for (index, (old_item, new_item)) in old_items.iter().zip(new_items).enumerate() {
                collect(&format!("{path}[{index}]"), old_item, new_item, changes);
            }
        }
        // anything else is one change at this path
        (old, new) => changes.push(FieldChange {
            path: path.to_string(),
            old: old.clone(),
            new: new.clone(),
        }),
    }
}

/// List the fields that differ between two views
///
/// # Arguments
///
/// * `old` - The view before the edit
/// * `new` - The view after the edit
#[must_use]
pub fn diff_views(old: &Value, new: &Value) -> Vec<FieldChange> {
    // walk both documents from the root
    let mut changes = Vec::new();
    collect("", old, new, &mut changes);
    changes
}

/// Render a unified diff of two views as YAML, colored when output is a terminal
///
/// # Arguments
///
/// * `old` - The view before the edit
/// * `new` - The view after the edit
/// * `order` - The curated top-level key order for the YAML
pub fn render_diff(old: &Value, new: &Value, order: &[&str]) -> Result<String, Error> {
    // render both views as YAML in the same order
    let old_yaml = utils::curated_yaml(old, order)?;
    let new_yaml = utils::curated_yaml(new, order)?;
    // diff the lines with a little context
    let diff = TextDiff::from_lines(&old_yaml, &new_yaml);
    let mut lines: Vec<String> = Vec::new();
    for (index, group) in diff.grouped_ops(2).iter().enumerate() {
        // separate distant hunks
        if index > 0 {
            lines.push("  ...".to_string());
        }
        for op in group {
            for change in diff.iter_changes(op) {
                let line = change.value().trim_end_matches('\n');
                lines.push(match change.tag() {
                    ChangeTag::Delete => format!("- {line}").red().to_string(),
                    ChangeTag::Insert => format!("+ {line}").green().to_string(),
                    ChangeTag::Equal => format!("  {line}"),
                });
            }
        }
    }
    // end the diff with a newline when there is one
    let mut out = lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    Ok(out)
}
