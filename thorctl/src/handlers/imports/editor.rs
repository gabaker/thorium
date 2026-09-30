//! Editor integration for resolving merge conflicts
//!
//! Handles creating temporary files with conflict markers, opening the user's
//! editor, validating the resolved YAML, and presenting error recovery options.

use colored::Colorize;
use serde::de::DeserializeOwned;
use similar::{ChangeTag, TextDiff};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use thorium::{CtlConf, Error};
use uuid::Uuid;

/// The editor named by `$VISUAL` or `$EDITOR`, read once per process
///
/// Returns `None` when neither is set to a non-empty value.
fn env_editor() -> Option<&'static str> {
    static ENV_EDITOR: OnceLock<Option<String>> = OnceLock::new();
    ENV_EDITOR
        .get_or_init(|| {
            ["VISUAL", "EDITOR"]
                .into_iter()
                .filter_map(|var| std::env::var(var).ok())
                .map(|value| value.trim().to_string())
                .find(|value| !value.is_empty())
        })
        .as_deref()
}

/// Resolve the editor command to use
///
/// Order of precedence: an explicit `--editor` override, then a configured
/// `default_editor` that differs from the built-in default, then `$VISUAL`, then
/// `$EDITOR`, and finally the built-in default (`vi`). A config that was never
/// changed carries the built-in default, so it defers to the environment.
///
/// # Arguments
///
/// * `editor_override` - An optional editor command from a `--editor` flag
/// * `conf` - The Thorctl config, whose `default_editor` is the fallback
pub(crate) fn resolve_editor<'a>(editor_override: Option<&'a str>, conf: &'a CtlConf) -> &'a str {
    // an explicit override always wins
    if let Some(editor) = editor_override {
        return editor;
    }
    // a customized config value wins over the environment
    if conf.default_editor != thorium::client::conf::default_default_editor() {
        return &conf.default_editor;
    }
    // otherwise honor $VISUAL/$EDITOR before falling back to the built-in default
    env_editor().unwrap_or(&conf.default_editor)
}

/// Split an editor command into its program and arguments
///
/// Follows simple shell-words rules so settings like `code --wait` or
/// `"/opt/My Editor/bin/edit" -w` work: whitespace separates words, single
/// quotes are literal, and double quotes group words. Outside single quotes a
/// backslash escapes the next character, except on Windows where backslashes are
/// path separators and are kept literally. A command that names an existing file
/// as a whole (a path with unquoted spaces) is used as-is.
///
/// # Arguments
///
/// * `command` - The editor command to split
fn split_command(command: &str) -> Result<Vec<String>, Error> {
    // a whole-string path to an existing program is never split
    if Path::new(command).is_file() {
        return Ok(vec![command.to_string()]);
    }
    let backslash_escapes = !cfg!(windows);
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_word = false;
    let mut chars = command.chars();
    while let Some(c) = chars.next() {
        match c {
            // whitespace ends the current word
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut current));
                    in_word = false;
                }
            }
            // single quotes are literal until the closing quote
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(inner) => current.push(inner),
                        None => {
                            return Err(Error::new(format!(
                                "Unterminated single quote in editor command '{command}'"
                            )));
                        }
                    }
                }
            }
            // double quotes group words, allowing escaped quotes and backslashes
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') if backslash_escapes => match chars.next() {
                            Some(escaped @ ('"' | '\\')) => current.push(escaped),
                            Some(other) => {
                                current.push('\\');
                                current.push(other);
                            }
                            None => break,
                        },
                        Some(inner) => current.push(inner),
                        None => {
                            return Err(Error::new(format!(
                                "Unterminated double quote in editor command '{command}'"
                            )));
                        }
                    }
                }
            }
            // a backslash escapes the next character
            '\\' if backslash_escapes => {
                in_word = true;
                if let Some(escaped) = chars.next() {
                    current.push(escaped);
                }
            }
            // any other character extends the current word
            other => {
                in_word = true;
                current.push(other);
            }
        }
    }
    // keep the final word
    if in_word {
        words.push(current);
    }
    if words.is_empty() {
        return Err(Error::new("The editor command is empty"));
    }
    Ok(words)
}

// ─── Merge Conflict Generation ───────────────────────────────────────────────

/// Generate a string with git-style merge conflict markers showing the
/// differences between two text representations
///
/// Backs every editor merge view (YAML, JSON, TOML, and plain text); the marker
/// labels are supplied by the caller so the two sides read accurately for the
/// flow in use (import vs. on-disk export).
///
/// # Arguments
///
/// * `current` - The text representing the current/left side
/// * `incoming` - The text representing the incoming/right side
/// * `current_label` - The label for the current side's conflict marker
/// * `incoming_label` - The label for the incoming side's conflict marker
pub fn generate_conflict_view(
    current: &str,
    incoming: &str,
    current_label: &str,
    incoming_label: &str,
) -> String {
    let diff = TextDiff::from_lines(current, incoming);
    let mut output = String::new();
    // buffer for collecting consecutive changed lines
    let mut current_lines: Vec<&str> = Vec::new();
    let mut incoming_lines: Vec<&str> = Vec::new();
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Equal => {
                // flush any buffered conflict before writing the equal line
                flush_conflict(
                    &mut output,
                    &mut current_lines,
                    &mut incoming_lines,
                    current_label,
                    incoming_label,
                );
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
    flush_conflict(
        &mut output,
        &mut current_lines,
        &mut incoming_lines,
        current_label,
        incoming_label,
    );
    output
}

/// Flush buffered conflict lines into the output string with git-style markers.
/// Drains `current_lines` and `incoming_lines` in place; does nothing if both
/// are empty.
///
/// # Arguments
///
/// * `output` - The output string to append conflict markers and lines to
/// * `current_lines` - Buffered lines from the current/left side
/// * `incoming_lines` - Buffered lines from the incoming/right side
/// * `current_label` - The label for the current side's conflict marker
/// * `incoming_label` - The label for the incoming side's conflict marker
fn flush_conflict(
    output: &mut String,
    current_lines: &mut Vec<&str>,
    incoming_lines: &mut Vec<&str>,
    current_label: &str,
    incoming_label: &str,
) {
    if current_lines.is_empty() && incoming_lines.is_empty() {
        return;
    }
    output.push_str("<<<<<<< ");
    output.push_str(current_label);
    output.push('\n');
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
    output.push_str(">>>>>>> ");
    output.push_str(incoming_label);
    output.push('\n');
}

/// Check if the content contains any unresolved merge conflict markers.
/// Returns the 1-based line number of the first conflict marker found, if any.
///
/// Markers are only recognized at the start of a line, exactly as
/// [`generate_conflict_view`] writes them. A `<<<<<<<` or `>>>>>>>` line is always
/// a leftover marker, but a `=======` line only counts inside an open
/// `<<<<<<<` block, so content that legitimately contains a line of seven `=`
/// (e.g. a Markdown heading underline) is not flagged.
///
/// # Arguments
///
/// * `content` - The file content to scan for conflict markers
fn find_conflict_markers(content: &str) -> Option<usize> {
    for (line_num, line) in content.lines().enumerate() {
        // an open or close marker starting a line is always a leftover marker
        if line.starts_with("<<<<<<<") || line.starts_with(">>>>>>>") {
            return Some(line_num + 1);
        }
    }
    // a separator is only a marker between an open and a close marker, and any open
    // marker was already reported above, so a lone separator is content
    None
}

// ─── Editor Loop ─────────────────────────────────────────────────────────────

/// A parser error the editor loop can report with an optional source location
///
/// Abstracts over the formats the editor loop validates (YAML/JSON via
/// `serde_norway`, TOML via `toml`) so [`editor_loop_core`] can surface a
/// line/column when the parser provides one and otherwise fall back to the
/// error's own `Display`.
pub(crate) trait EditorParseError: std::fmt::Display {
    /// The 1-based `(line, column)` of the error, when the parser exposes one
    fn location(&self) -> Option<(usize, usize)>;
}

impl EditorParseError for serde_norway::Error {
    /// Pulls the line/column from the YAML error's location, if present
    fn location(&self) -> Option<(usize, usize)> {
        serde_norway::Error::location(self).map(|loc| (loc.line(), loc.column()))
    }
}

impl EditorParseError for toml::de::Error {
    /// TOML errors render a caret-annotated snippet in their `Display`, so the
    /// loop relies on that rather than a separate line/column
    fn location(&self) -> Option<(usize, usize)> {
        None
    }
}

/// Prompt the user to either retry editing or cancel after a validation error
fn prompt_error_action() -> Result<ErrorAction, Error> {
    let items = &[
        "Edit   - Reopen editor to fix the issue",
        "Cancel - Abandon changes for this resource",
    ];
    // ask which action to take
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

/// Ask whether to reopen the editor after an error, returning `true` to reopen
///
/// Used by callers that hit an error after the editor loop returned (e.g. the
/// server rejecting an update) so the user's edits aren't lost.
pub(crate) fn prompt_reedit() -> Result<bool, Error> {
    Ok(matches!(prompt_error_action()?, ErrorAction::Edit))
}

/// Action the user wants to take after a validation error
enum ErrorAction {
    /// Reopen the editor to fix the issue
    Edit,
    /// Abandon changes for this resource
    Cancel,
}

/// Make a label safe to use as part of a file name
///
/// # Arguments
///
/// * `label` - The label to sanitize
fn file_safe(label: &str) -> String {
    label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Create a new file readable and writable only by the current user
///
/// Fails if the file already exists, so a pre-placed file or symlink can't be
/// used to redirect the write.
///
/// # Arguments
///
/// * `path` - The path of the file to create
/// * `content` - The content to write to the file
fn create_private_file(path: &Path, content: &str) -> Result<(), Error> {
    use std::io::Write;
    // open exclusively, restricting permissions on unix
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    // write the content
    options
        .open(path)
        .and_then(|mut file| file.write_all(content.as_bytes()))
        .map_err(|err| {
            Error::new(format!(
                "Failed to write temporary file '{}': {err}",
                path.display()
            ))
        })
}

/// Create a new directory accessible only by the current user
///
/// # Arguments
///
/// * `path` - The directory to create (must not exist yet)
fn create_private_dir(path: &Path) -> Result<(), Error> {
    // create exclusively, restricting permissions on unix
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(path).map_err(|err| {
        Error::new(format!(
            "Failed to create temporary directory '{}': {err}",
            path.display()
        ))
    })
}

/// An RAII guard over a private temporary edit file
///
/// Each guard owns a fresh directory in the system temp dir, created with
/// user-only permissions, holding one user-only file. Nothing is shared between
/// users or runs, so a second user on the same host is never blocked by
/// another's directory and edited content (which may include secrets such as
/// env values) is not readable by others.
///
/// The editor loop has many exit points (editor launch failure, read failure,
/// parse error, user cancel via an error prompt, and success). Owning the temp
/// path in a guard guarantees it is cleaned up on every path, including the ones
/// that propagate an error with `?`, so no stray edit files are left behind.
struct TempFile {
    /// The private directory holding the file, removed on drop
    dir: PathBuf,
    /// The path to the temporary file
    path: PathBuf,
}

impl TempFile {
    /// Create a private temporary file holding `content`
    ///
    /// # Arguments
    ///
    /// * `label` - A label used in the file name (e.g. "image-group-name")
    /// * `ext` - The file extension (without a dot) so the editor highlights the format
    /// * `content` - The initial file content
    fn create(label: &str, ext: &str, content: &str) -> Result<Self, Error> {
        // make a fresh private directory for this edit session
        let dir = std::env::temp_dir().join(format!("thorium-edit-{}", Uuid::new_v4()));
        create_private_dir(&dir)?;
        // build the guard first so the directory is removed even if the write fails
        let temp = TempFile {
            path: dir.join(format!("{}.{ext}", file_safe(label))),
            dir,
        };
        create_private_file(&temp.path, content)?;
        Ok(temp)
    }

    /// Returns the path of the guarded temporary file
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempFile {
    /// Removes the temporary directory and file, ignoring errors since cleanup is
    /// best-effort
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Save edited content to a new user-only file so it survives a failed update
///
/// Returns the path the content was written to.
///
/// # Arguments
///
/// * `content` - The edited content to save
/// * `label` - A label used in the file name
/// * `ext` - The file extension (without a dot)
pub(crate) fn save_recovery_file(content: &str, label: &str, ext: &str) -> Result<PathBuf, Error> {
    let path = std::env::temp_dir().join(format!(
        "thorium-edit-{}-{}.{ext}",
        file_safe(label),
        Uuid::new_v4()
    ));
    create_private_file(&path, content)?;
    Ok(path)
}

/// Run the editor command on a file and wait for it to exit
///
/// # Arguments
///
/// * `editor` - The editor command (split into program and arguments)
/// * `path` - The file to open
async fn run_editor(editor: &str, path: &Path) -> Result<(), Error> {
    // split the command so editors that need flags (e.g. `code --wait`) work
    let words = split_command(editor)?;
    let (program, args) = words
        .split_first()
        .ok_or_else(|| Error::new("The editor command is empty"))?;
    // launch the editor and wait for it
    let status = tokio::process::Command::new(program)
        .args(args)
        .arg(path)
        .status()
        .await
        .map_err(|err| Error::new(format!("Unable to open editor '{editor}': {err}")))?;
    // a failing editor aborts the edit
    if !status.success() {
        return Err(match status.code() {
            Some(code) => Error::new(format!("Editor '{editor}' exited with error code: {code}")),
            None => Error::new(format!("Editor '{editor}' exited with error!")),
        });
    }
    Ok(())
}

/// The core editor loop: write `content` to a temp file, open the user's editor, and
/// on each save run `parse` to validate. On unresolved conflict markers or a parse
/// error it prints a helpful message (with line/column when the error carries a
/// location) and prompts the user to reopen the editor or cancel.
///
/// `parse` receives the saved text and returns any [`EditorParseError`] on failure so
/// the loop can surface its location. [`editor_loop`] and the other entry points are
/// thin wrappers over this.
///
/// # Arguments
///
/// * `content` - The initial file content (e.g., YAML with merge conflict markers)
/// * `label` - A label used when naming the temp file (e.g., "image-group-name")
/// * `editor` - The editor command to open
/// * `ext` - The temp file extension (without a dot), so the editor highlights the
///   edited format (e.g., "yml", "json", "toml", "md")
/// * `parse` - Validates the saved text, returning the value to hand back
///
/// # Returns
///
/// The parsed value, or `None` if the user cancelled.
async fn editor_loop_core<R, E: EditorParseError>(
    content: &str,
    label: &str,
    editor: &str,
    ext: &str,
    parse: impl Fn(&str) -> Result<R, E>,
) -> Result<Option<R>, Error> {
    // own the temp file in a guard so every exit below (including `?` from the
    // error prompts) removes it rather than leaking it
    let temp = TempFile::create(label, ext, content)?;
    loop {
        // open the editor and wait for the user to finish
        run_editor(editor, temp.path()).await?;
        // read back the file
        let resolved = tokio::fs::read_to_string(temp.path())
            .await
            .map_err(|err| Error::new(format!("Failed to read temporary file: {err}")))?;
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
                    return Ok(None);
                }
            }
        }
        // validate via the caller's parse function
        match parse(&resolved) {
            Ok(parsed) => {
                // valid — the guard cleans up the temp file as it drops
                return Ok(Some(parsed));
            }
            Err(err) => {
                // surface the location when the parser exposes one; otherwise the
                // error's own Display carries the detail (e.g. TOML's caret snippet)
                if let Some((line, column)) = err.location() {
                    eprintln!(
                        "{} Parse error at line {}, column {}: {}",
                        "Error:".bright_red().bold(),
                        line.to_string().bright_yellow(),
                        column.to_string().bright_yellow(),
                        err,
                    );
                } else {
                    eprintln!("{} Parse error: {}", "Error:".bright_red().bold(), err);
                }
                // reopen the editor on the next loop iteration, or give up
                if let ErrorAction::Cancel = prompt_error_action()? {
                    return Ok(None);
                }
            }
        }
    }
}

/// Like [`editor_loop`], but also returns the exact text the user saved
///
/// Lets a caller reopen the editor with the user's own text (comments and all)
/// if something fails after the edit, e.g. the server rejecting the update.
///
/// # Arguments
///
/// * `content` - The initial file content
/// * `label` - A label used when naming the temp file
/// * `editor` - The editor command to open
pub(crate) async fn editor_loop_with_text<T>(
    content: &str,
    label: &str,
    editor: &str,
) -> Result<Option<(T, String)>, Error>
where
    T: DeserializeOwned,
{
    editor_loop_core(content, label, editor, "yml", |resolved| {
        serde_norway::from_str::<T>(resolved).map(|parsed| (parsed, resolved.to_string()))
    })
    .await
}

/// Open a file in the user's editor with a validation loop, deserializing the result
/// to `T` on success (catches YAML syntax + schema errors). Returns `None` if the user
/// cancelled. See [`editor_loop_core`].
///
/// # Arguments
///
/// * `content` - The initial file content (e.g., YAML with merge conflict markers)
/// * `label` - A label used when naming the temp file
/// * `editor` - The editor command to open
pub async fn editor_loop<T>(content: &str, label: &str, editor: &str) -> Result<Option<T>, Error>
where
    T: DeserializeOwned,
{
    editor_loop_core(content, label, editor, "yml", |resolved| {
        serde_norway::from_str::<T>(resolved)
    })
    .await
}

/// Like [`editor_loop`], but validates the edited content as `T` (typed, with
/// line/column errors) while returning the full edited document as a
/// `serde_json::Value` — every field the user kept, not just `T`'s serialized fields.
/// Used by [`review_config_in_editor`] so scaffolded configs stay complete.
///
/// # Arguments
///
/// * `content` - The initial file content to edit
/// * `label` - A label used when naming the temp file
/// * `editor` - The editor command to open
pub(crate) async fn editor_loop_validated<T>(
    content: &str,
    label: &str,
    editor: &str,
) -> Result<Option<serde_json::Value>, Error>
where
    T: DeserializeOwned,
{
    editor_loop_core(content, label, editor, "yml", |resolved| {
        // validate against the typed config first (its error carries the line/column),
        // then re-parse the same text into a Value so every field is preserved
        serde_norway::from_str::<T>(resolved)?;
        serde_norway::from_str::<serde_json::Value>(resolved)
    })
    .await
}

/// Open an editor on a git-style conflict view between two text blobs, returning the
/// resolved text (or `None` if the user cancelled)
///
/// Generic over the save-time validator's error type so callers can validate the
/// merged result as YAML/JSON, TOML, or not at all (see [`EditorParseError`]). The
/// resolved text is returned verbatim — the caller decides where it lands.
///
/// # Arguments
///
/// * `current` - The current/left side of the merge (e.g., the on-disk file)
/// * `incoming` - The incoming/right side of the merge (e.g., the freshly exported file)
/// * `current_label` - The conflict-marker label for the current side
/// * `incoming_label` - The conflict-marker label for the incoming side
/// * `label` - A label used when naming the temp file
/// * `editor` - The editor command to open
/// * `ext` - The temp file extension (without a dot) for editor highlighting
/// * `validate` - Validates the saved text on each save
#[allow(clippy::too_many_arguments)]
pub(crate) async fn merge_in_editor<E: EditorParseError>(
    current: &str,
    incoming: &str,
    current_label: &str,
    incoming_label: &str,
    label: &str,
    editor: &str,
    ext: &str,
    validate: impl Fn(&str) -> Result<(), E>,
) -> Result<Option<String>, Error> {
    // build the conflict-marked document the user resolves in the editor
    let conflict = generate_conflict_view(current, incoming, current_label, incoming_label);
    // run the shared loop, handing back the resolved text once it validates
    editor_loop_core(&conflict, label, editor, ext, |resolved| {
        validate(resolved).map(|()| resolved.to_string())
    })
    .await
}

/// Open a config in the user's editor for review, validating it against the typed
/// request `T`, and return the (possibly edited) config as curated, pretty JSON.
///
/// The config is presented as curated YAML for editing and validated as `T` (so bad
/// enum/type values are reported with line/column, with an Edit/Cancel retry). The
/// **full edited document** is written back in curated order — every field, not just
/// `T`'s serialized fields — so scaffolded configs stay complete. If the user cancels,
/// the unchanged default is returned (still in curated order). Shared by `toolbox
/// init`/`export` and `images`/`pipelines export`'s opt-in `--review` pass.
///
/// # Arguments
///
/// * `json_config` - The config to review, as a JSON string
/// * `label` - A label used to name the temporary edit file
/// * `editor` - The editor command to open (resolve via [`resolve_editor`])
/// * `order` - The curated top-level key order (see [`crate::utils::curated_yaml`])
pub(crate) async fn review_config_in_editor<T>(
    json_config: &str,
    label: &str,
    editor: &str,
    order: &[&str],
) -> Result<String, Error>
where
    T: DeserializeOwned,
{
    // parse the config and render it as curated YAML for editing
    let value: serde_json::Value = serde_json::from_str(json_config)
        .map_err(|e| Error::new(format!("Failed to parse config for editor review: {e}")))?;
    let yaml = crate::utils::curated_yaml(&value, order)
        .map_err(|e| Error::new(format!("Failed to convert config to YAML: {e}")))?;
    // let the user review it, keeping the default on cancel
    match editor_loop_validated::<T>(&yaml, label, editor).await? {
        // write the edited document in curated order
        Some(resolved) => crate::utils::curated_json(&resolved, order),
        // cancelled: write the unchanged default, still in curated order
        None => crate::utils::curated_json(&value, order),
    }
}

/// Unit tests for editor command splitting and conflict marker detection
#[cfg(test)]
mod tests {
    use super::*;

    /// Plain commands and commands with flags split on whitespace
    #[test]
    fn splits_flags() {
        assert_eq!(split_command("vi").unwrap(), vec!["vi"]);
        assert_eq!(
            split_command("code --wait").unwrap(),
            vec!["code", "--wait"]
        );
        assert_eq!(
            split_command("  emacsclient   -t ").unwrap(),
            vec!["emacsclient", "-t"]
        );
    }

    /// Quotes group words containing spaces
    #[test]
    fn splits_quotes() {
        assert_eq!(
            split_command("\"/opt/My Editor/edit\" -w").unwrap(),
            vec!["/opt/My Editor/edit", "-w"]
        );
        assert_eq!(
            split_command("'my editor' --flag='a b'").unwrap(),
            vec!["my editor", "--flag=a b"]
        );
    }

    /// Unterminated quotes and empty commands are errors
    #[test]
    fn rejects_bad_commands() {
        assert!(split_command("\"code --wait").is_err());
        assert!(split_command("'code").is_err());
        assert!(split_command("   ").is_err());
    }

    /// Generated conflict markers are detected
    #[test]
    fn finds_generated_markers() {
        let view = generate_conflict_view("a: 1\n", "a: 2\n", "Current", "Incoming");
        assert_eq!(find_conflict_markers(&view), Some(1));
    }

    /// A lone line of seven '=' is content, not a marker, even at column 0
    #[test]
    fn ignores_lone_separator() {
        assert_eq!(
            find_conflict_markers("description: |\n  Usage\n  =======\n"),
            None
        );
        assert_eq!(find_conflict_markers("Usage\n=======\ntext\n"), None);
    }

    /// Indented marker-like lines are content
    #[test]
    fn ignores_indented_markers() {
        assert_eq!(
            find_conflict_markers("text: |\n  <<<<<<< not a marker\n"),
            None
        );
    }
}
