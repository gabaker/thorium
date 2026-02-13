//! Editor integration for resolving merge conflicts
//!
//! Handles creating temporary files with conflict markers, opening the user's
//! editor, validating the resolved YAML, and presenting error recovery options.

use colored::Colorize;
use similar::{ChangeTag, TextDiff};
use std::io::Write;
use thorium::Error;
use uuid::Uuid;

// ─── Merge Conflict Generation ───────────────────────────────────────────────

/// Generate a YAML string with git-style merge conflict markers showing the
/// differences between two YAML representations
///
/// # Arguments
///
/// * `current_yaml` - The YAML string representing the current Thorium state
/// * `incoming_yaml` - The YAML string representing the incoming manifest state
pub fn generate_conflict_yaml(current_yaml: &str, incoming_yaml: &str) -> String {
    let diff = TextDiff::from_lines(current_yaml, incoming_yaml);
    let mut output = String::new();
    // buffer for collecting consecutive changed lines
    let mut current_lines: Vec<&str> = Vec::new();
    let mut incoming_lines: Vec<&str> = Vec::new();

    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Equal => {
                // flush any buffered conflict before writing the equal line
                flush_conflict(&mut output, &mut current_lines, &mut incoming_lines);
                output.push_str(change.value());
            }
            ChangeTag::Delete => {
                current_lines.push(change.value());
            }
            ChangeTag::Insert => {
                incoming_lines.push(change.value());
            }
        }
    }
    // flush any remaining conflict at the end
    flush_conflict(&mut output, &mut current_lines, &mut incoming_lines);
    output
}

/// Flush buffered conflict lines into the output with git-style markers
fn flush_conflict(output: &mut String, current_lines: &mut Vec<&str>, incoming_lines: &mut Vec<&str>) {
    if current_lines.is_empty() && incoming_lines.is_empty() {
        return;
    }
    output.push_str("<<<<<<< Current (Thorium)\n");
    for line in current_lines.drain(..) {
        output.push_str(line);
        if !line.ends_with('\n') {
            output.push('\n');
        }
    }
    output.push_str("=======\n");
    for line in incoming_lines.drain(..) {
        output.push_str(line);
        if !line.ends_with('\n') {
            output.push('\n');
        }
    }
    output.push_str(">>>>>>> Incoming (Manifest)\n");
}

/// Check if the content contains any unresolved merge conflict markers.
/// Returns the line number of the first conflict marker found, if any.
fn find_conflict_markers(content: &str) -> Option<usize> {
    for (line_num, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("<<<<<<<")
            || trimmed == "======="
            || trimmed.starts_with(">>>>>>>")
        {
            return Some(line_num + 1);
        }
    }
    None
}

// ─── Editor Loop ─────────────────────────────────────────────────────────────

/// Prompt the user to either retry editing or cancel after a validation error
fn prompt_error_action() -> Result<ErrorAction, Error> {
    let items = &[
        "Edit   - Reopen editor to fix the issue",
        "Cancel - Abandon changes for this resource",
    ];
    let selection = dialoguer::Select::new()
        .items(items)
        .default(0)
        .interact()
        .map_err(|err| Error::new(format!("Failed to read user input: {err}")))?;
    Ok(match selection {
        0 => ErrorAction::Edit,
        _ => ErrorAction::Cancel,
    })
}

/// Action the user wants to take after a validation error
enum ErrorAction {
    /// Reopen the editor to fix the issue
    Edit,
    /// Abandon changes for this resource
    Cancel,
}

/// Open a merge conflict file in the user's editor with a validation loop.
/// Displays helpful error messages for unresolved conflict markers or YAML
/// syntax errors, then prompts the user to reopen the editor or cancel.
///
/// # Arguments
///
/// * `content` - The initial content with merge conflict markers
/// * `label` - A label for the temp file (e.g., "image-group-name")
/// * `editor` - The editor command to use
///
/// Returns the validated, resolved YAML content as a string, or `None` if
/// the user chose to cancel
pub fn editor_loop(content: &str, label: &str, editor: &str) -> Result<Option<String>, Error> {
    // create a temp directory
    let temp_dir = std::env::temp_dir().join("thorium");
    std::fs::create_dir_all(&temp_dir).map_err(|err| {
        Error::new(format!(
            "Failed to create temporary directory '{}': {}",
            temp_dir.to_string_lossy(),
            err
        ))
    })?;
    let temp_path = temp_dir.join(format!("merge-{}-{}.yml", label, Uuid::new_v4()));
    // write initial content
    write_temp_file(&temp_path, content)?;
    loop {
        // open the editor
        let status = std::process::Command::new(editor)
            .arg(&temp_path)
            .status()
            .map_err(|err| {
                let _ = std::fs::remove_file(&temp_path);
                Error::new(format!("Unable to open editor '{editor}': {err}"))
            })?;
        if !status.success() {
            let _ = std::fs::remove_file(&temp_path);
            return Err(match status.code() {
                Some(code) => Error::new(format!("Editor '{editor}' exited with error code: {code}")),
                None => Error::new(format!("Editor '{editor}' exited with error!")),
            });
        }
        // read back the file
        let resolved = std::fs::read_to_string(&temp_path).map_err(|err| {
            let _ = std::fs::remove_file(&temp_path);
            Error::new(format!("Failed to read temporary file: {err}"))
        })?;
        // check for unresolved conflict markers
        if let Some(line) = find_conflict_markers(&resolved) {
            eprintln!(
                "{} Unresolved merge conflict marker found at line {}. Please resolve all conflicts before saving.",
                "Error:".bright_red().bold(),
                line.to_string().bright_yellow(),
            );
            match prompt_error_action()? {
                ErrorAction::Edit => continue,
                ErrorAction::Cancel => {
                    let _ = std::fs::remove_file(&temp_path);
                    return Ok(None);
                }
            }
        }
        // validate YAML syntax by attempting to parse as serde_yaml::Value
        match serde_yaml::from_str::<serde_yaml::Value>(&resolved) {
            Ok(_) => {
                // valid YAML — clean up and return
                let _ = std::fs::remove_file(&temp_path);
                return Ok(Some(resolved));
            }
            Err(err) => {
                // extract location info from the error
                let location = err.location();
                if let Some(loc) = location {
                    eprintln!(
                        "{} YAML syntax error at line {}, column {}: {}",
                        "Error:".bright_red().bold(),
                        loc.line().to_string().bright_yellow(),
                        loc.column().to_string().bright_yellow(),
                        err,
                    );
                } else {
                    eprintln!(
                        "{} YAML syntax error: {}",
                        "Error:".bright_red().bold(),
                        err,
                    );
                }
                match prompt_error_action()? {
                    ErrorAction::Edit => continue,
                    ErrorAction::Cancel => {
                        let _ = std::fs::remove_file(&temp_path);
                        return Ok(None);
                    }
                }
            }
        }
    }
}

/// Write content to a temp file, creating or overwriting it
fn write_temp_file(path: &std::path::Path, content: &str) -> Result<(), Error> {
    let mut file = std::fs::File::create(path).map_err(|err| {
        Error::new(format!(
            "Failed to create temporary file '{}': {}",
            path.to_string_lossy(),
            err
        ))
    })?;
    file.write_all(content.as_bytes()).map_err(|err| {
        let _ = std::fs::remove_file(path);
        Error::new(format!("Failed to write temporary file: {err}"))
    })?;
    Ok(())
}
