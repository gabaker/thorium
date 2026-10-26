//! The shared edit flow: choose a source, validate, diff, confirm, guard, apply, report
//!
//! [`edit_with`] and [`update_with`] are the entry points resource commands call. Both read the
//! resource, build a candidate view from the sources the user gave (the editor, `--file`,
//! `--patch`, `--set`, `--unset`), check it in layers (syntax, view type, the resource's own
//! checks) before any request is sent, show the changes, ask for confirmation when a person
//! edited in an editor, re-read the resource to make sure it did not change meanwhile, and send
//! the update.

use colored::Colorize;
use serde_json::Value;
use std::cell::RefCell;
use std::fmt;
use std::path::PathBuf;
use thorium::{CtlConf, Error};

use super::diff::{diff_views, render_diff};
use super::editable::{EditOutcome, EditStatus, Editable, FieldChange};
use super::seams::Seams;
use crate::args::helpers::EditArgs;
use crate::handlers::imports::editor::{
    EditorParseError, ErrorAction, editor_loop_asking, prompt_error_action_with, resolve_editor,
    save_recovery_file,
};
use crate::utils::output::OutputFormat;
use crate::utils::patch::{PathSeg, merge_patch};
use crate::utils::{self, spec_io};

/// Which command is running the flow
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// `edit`: opens an editor unless a non-interactive source was given
    Edit,
    /// `update`: never opens an editor and needs a non-interactive source
    Update,
}

/// A failure while checking an edited view, with the position to show when there is one
#[derive(Debug)]
struct ViewError {
    /// What is wrong
    message: String,
    /// The 1-based line and column of the problem in the edited text
    location: Option<(usize, usize)>,
}

impl fmt::Display for ViewError {
    /// Write the message alone; the location is shown separately by the editor loop
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl EditorParseError for ViewError {
    /// The line and column of a syntax or schema error
    fn location(&self) -> Option<(usize, usize)> {
        self.location
    }
}

/// How an attempt to apply an edited view failed
enum Failure {
    /// The edit may be fixed in the editor and tried again
    Retry(Error),
    /// The edit cannot be applied as it stands
    Fatal(Error),
}

/// Whether a top-level view key is read-only
///
/// # Arguments
///
/// * `key` - The top-level key
/// * `listed` - The resource's list of read-only field names
#[must_use]
pub fn is_static_key(key: &str, listed: &[&str]) -> bool {
    // keys written *like-this* and listed names are read-only
    (key.len() > 2 && key.starts_with('*') && key.ends_with('*')) || listed.contains(&key)
}

/// Remove the read-only top-level keys from a view document
///
/// # Arguments
///
/// * `view` - The view document
/// * `listed` - The resource's list of read-only field names
fn without_static(view: &Value, listed: &[&str]) -> Value {
    // only objects have keys to remove
    let Value::Object(map) = view else {
        return view.clone();
    };
    Value::Object(
        map.iter()
            .filter(|(key, _)| !is_static_key(key, listed))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    )
}

/// List the read-only keys whose values differ between two view documents
///
/// # Arguments
///
/// * `original` - The view as fetched
/// * `edited` - The edited view
/// * `listed` - The resource's list of read-only field names
fn changed_static(original: &Value, edited: &Value, listed: &[&str]) -> Vec<String> {
    // only objects have keys to compare
    let (Value::Object(original), Value::Object(edited)) = (original, edited) else {
        return Vec::new();
    };
    // collect the read-only keys that differ, including removed ones
    let mut changed: Vec<String> = original
        .keys()
        .chain(edited.keys())
        .filter(|key| is_static_key(key, listed))
        .filter(|key| original.get(*key) != edited.get(*key))
        .cloned()
        .collect();
    changed.sort_unstable();
    changed.dedup();
    changed
}

/// Whether editor text holds nothing but comments and blank lines
///
/// # Arguments
///
/// * `text` - The text read back from the editor
fn is_blank(text: &str) -> bool {
    text.lines()
        .all(|line| line.trim().is_empty() || line.trim_start().starts_with('#'))
}

/// Convert a resource view into a document
///
/// # Arguments
///
/// * `view` - The view to convert
fn to_document<V: serde::Serialize>(view: &V) -> Result<Value, Error> {
    serde_json::to_value(view).map_err(|err| Error::new(format!("Failed to serialize view: {err}")))
}

/// Build the candidate view document from the non-interactive sources
///
/// # Arguments
///
/// * `args` - The edit flags
/// * `original` - The view as fetched
/// * `listed` - The resource's list of read-only field names
fn candidate_document(args: &EditArgs, original: &Value, listed: &[&str]) -> Result<Value, Error> {
    // start from the file when given, otherwise from the current view
    let mut document = match &args.file {
        Some(source) => spec_io::read_document(source, "--file")?,
        None => original.clone(),
    };
    // apply each merge patch in order
    for patch in &args.patch {
        merge_patch(&mut document, &spec_io::parse_patch(patch)?);
    }
    // apply each set and unset in the order they were typed
    for op in &args.ops {
        // read-only fields cannot be set from the command line
        if let Some(PathSeg::Key(key)) = op.top_level() {
            let read_only =
                is_static_key(key, listed) || original.get(format!("*{key}*")).is_some();
            if read_only {
                return Err(Error::new(format!(
                    "'{key}' is read-only and cannot be edited"
                )));
            }
        }
        op.apply(&mut document)?;
    }
    Ok(document)
}

/// Where a candidate view came from
enum Source {
    /// A person edited the view in an editor
    Editor,
    /// The command line (`--file`, `--patch`, `--set`, `--unset`) described the change
    Flags,
}

/// The state of one attempt to apply an edit
struct Attempt<'a, E: Editable> {
    /// What talks to Thorium
    client: &'a E::Client,
    /// Which resource is being edited
    target: &'a E::Target,
    /// The resource as it was first read
    data: &'a E::Data,
    /// The view of the resource as it was first read
    original: &'a Value,
    /// The edit flags
    args: &'a EditArgs,
    /// The terminal state and scripted answers
    seams: &'a Seams,
}

impl<E: Editable> Attempt<'_, E> {
    /// Show, confirm, guard and apply an edited view
    ///
    /// # Arguments
    ///
    /// * `view` - The edited and validated view
    /// * `source` - Where the view came from
    async fn apply(&self, view: E::View, source: &Source) -> Result<EditOutcome, Failure> {
        let listed = E::static_fields();
        let id = E::id(self.data);
        let name = E::name(self.data);
        let edited = to_document(&view).map_err(Failure::Fatal)?;
        // read-only fields are ignored on save, so say so if they were changed
        let ignored = changed_static(self.original, &edited, listed);
        if !ignored.is_empty() {
            eprintln!(
                "{} Ignoring edits to read-only field(s): {}",
                "Warning:".bright_yellow(),
                ignored.join(", ")
            );
        }
        // compare only the editable fields
        let before = without_static(self.original, listed);
        let after = without_static(&edited, listed);
        let diff = diff_views(&before, &after);
        let outcome = |status: EditStatus, diff: Vec<FieldChange>| EditOutcome {
            kind: E::KIND.to_string(),
            id: id.clone(),
            name: name.clone(),
            status,
            diff,
            resource: None,
            saved_to: None,
        };
        // let the resource turn the view into a request; nothing to send means nothing changed
        let Some(update) = E::calculate_update(self.client, self.data, view)
            .await
            .map_err(Failure::Retry)?
        else {
            return Ok(outcome(EditStatus::NoChanges, Vec::new()));
        };
        // show the changes
        self.show_diff(&before, &after, &name)
            .map_err(Failure::Fatal)?;
        // a dry run stops here
        if self.args.dry_run {
            return Ok(outcome(EditStatus::DryRun, diff));
        }
        // a person who edited in an editor confirms what they are about to apply
        if matches!(source, Source::Editor) && !self.args.skip_confirm {
            let question = format!(
                "Apply these changes to {} '{name}'?",
                E::KIND.to_lowercase()
            );
            if !self.seams.confirm(&question).map_err(Failure::Fatal)? {
                return Ok(outcome(EditStatus::Cancelled, diff));
            }
        }
        // make sure the resource did not change since it was read
        self.guard(&name).await.map_err(Failure::Fatal)?;
        // send the update
        E::send_update(self.client, self.target, self.data, &update)
            .await
            .map_err(Failure::Retry)?;
        Ok(outcome(EditStatus::Updated, diff))
    }

    /// Print the changes, on stdout for people and stderr when stdout carries a document
    ///
    /// # Arguments
    ///
    /// * `before` - The editable fields before the edit
    /// * `after` - The editable fields after the edit
    /// * `name` - The resource's name
    fn show_diff(&self, before: &Value, after: &Value, name: &str) -> Result<(), Error> {
        let text = format!(
            "Changes to {} '{name}':\n{}",
            E::KIND.to_lowercase(),
            render_diff(before, after, E::FIELD_ORDER)?
        );
        // keep stdout clean when it carries a JSON or YAML result
        if matches!(
            self.args.format,
            Some(OutputFormat::Json | OutputFormat::Yaml)
        ) {
            eprint!("{text}");
        } else {
            print!("{text}");
        }
        Ok(())
    }

    /// Refuse to apply when the resource changed since it was read, unless forced
    ///
    /// # Arguments
    ///
    /// * `name` - The resource's name
    async fn guard(&self, name: &str) -> Result<(), Error> {
        // a test or script may change the resource here
        self.seams.run_before_apply()?;
        // read the resource again and compare its view with the first read
        let fresh = E::fetch(self.client, self.target).await?;
        let now = to_document(&E::to_view(&fresh))?;
        if now != *self.original && !self.args.force {
            return Err(Error::new(format!(
                "{} '{name}' changed remotely; re-run or pass --force",
                E::KIND
            )));
        }
        Ok(())
    }
}

/// Edit a resource: in an editor, or from `--file`, `--patch`, `--set` and `--unset`
///
/// With no non-interactive source this opens the view in the editor (which needs a terminal).
/// With one it never opens an editor, so `edit` does everything `update` does. The outcome is
/// printed in the requested format and returned.
///
/// # Arguments
///
/// * `client` - What talks to Thorium
/// * `conf` - The Thorctl config (for the default editor)
/// * `target` - Which resource to edit
/// * `args` - The edit flags
pub async fn edit_with<E: Editable>(
    client: &E::Client,
    conf: &CtlConf,
    target: &E::Target,
    args: &EditArgs,
) -> Result<EditOutcome, Error> {
    // pick the editor the user asked for
    let editor = resolve_editor(args.editor.as_deref(), conf);
    run::<E>(client, editor, target, args, Mode::Edit, &Seams::from_env()).await
}

/// Update a resource from `--file`, `--patch`, `--set` and `--unset`, never opening an editor
///
/// # Arguments
///
/// * `client` - What talks to Thorium
/// * `target` - Which resource to update
/// * `args` - The edit flags
pub async fn update_with<E: Editable>(
    client: &E::Client,
    target: &E::Target,
    args: &EditArgs,
) -> Result<EditOutcome, Error> {
    run::<E>(client, "", target, args, Mode::Update, &Seams::from_env()).await
}

/// Run the flow for either command
///
/// # Arguments
///
/// * `client` - What talks to Thorium
/// * `editor` - The editor command (unused by `update`)
/// * `target` - Which resource to edit
/// * `args` - The edit flags
/// * `mode` - Which command is running
/// * `seams` - The terminal state and scripted answers
pub(crate) async fn run<E: Editable>(
    client: &E::Client,
    editor: &str,
    target: &E::Target,
    args: &EditArgs,
    mode: Mode,
    seams: &Seams,
) -> Result<EditOutcome, Error> {
    // decide whether this run needs an editor before reading anything
    let interactive = !args.has_changes();
    if interactive && mode == Mode::Update {
        return Err(Error::new(format!(
            "Nothing to update for {}: pass --file, --set, --unset or --patch",
            E::KIND.to_lowercase()
        )));
    }
    if interactive && !seams.can_edit() {
        return Err(Error::new(format!(
            "Editing {} requires an interactive terminal; pass --file, --set, --unset or --patch to edit without one",
            E::KIND.to_lowercase()
        )));
    }
    // read the resource and snapshot its view
    let data = E::fetch(client, target).await?;
    let original = to_document(&E::to_view(&data))?;
    let attempt = Attempt::<E> {
        client,
        target,
        data: &data,
        original: &original,
        args,
        seams,
    };
    // apply the edit from the editor or from the command line
    let mut outcome = if interactive {
        edit_in_editor::<E>(&attempt, editor).await?
    } else {
        edit_from_flags::<E>(&attempt).await?
    };
    // add the resource's new state when the result is a document
    if outcome.status == EditStatus::Updated
        && matches!(args.format, Some(OutputFormat::Json | OutputFormat::Yaml))
    {
        let fresh = E::fetch(client, target).await?;
        outcome.resource = Some(to_document(&E::to_view(&fresh))?);
    }
    print_outcome(&outcome, args.format)?;
    Ok(outcome)
}

/// Apply an edit described by `--file`, `--patch`, `--set` and `--unset`
///
/// # Arguments
///
/// * `attempt` - The state of this edit
async fn edit_from_flags<E: Editable>(attempt: &Attempt<'_, E>) -> Result<EditOutcome, Error> {
    // build the candidate document and check it as the resource's view
    let document = candidate_document(attempt.args, attempt.original, E::static_fields())?;
    let view: E::View = serde_json::from_value(document)
        .map_err(|err| Error::new(format!("Invalid {} edit: {err}", E::KIND.to_lowercase())))?;
    E::validate(&view, attempt.data)?;
    // apply it, reporting any failure as is
    match attempt.apply(view, &Source::Flags).await {
        Ok(outcome) => Ok(outcome),
        Err(Failure::Retry(err) | Failure::Fatal(err)) => Err(err),
    }
}

/// Apply an edit made in the editor, looping while the user fixes problems
///
/// # Arguments
///
/// * `attempt` - The state of this edit
/// * `editor` - The editor command
async fn edit_in_editor<E: Editable>(
    attempt: &Attempt<'_, E>,
    editor: &str,
) -> Result<EditOutcome, Error> {
    // render the editable view under a header
    let yaml = utils::curated_yaml(attempt.original, E::FIELD_ORDER)
        .map_err(|err| Error::new(format!("Failed to serialize {} to YAML: {err}", E::KIND)))?;
    let mut text = format!("{}{yaml}", E::header(attempt.data));
    let name = E::name(attempt.data);
    let label = format!("{}-{name}", E::KIND.to_lowercase());
    loop {
        // remember the latest text saved so a cancelled edit can be recovered
        let latest = RefCell::new(String::new());
        // let the user edit, checking the view each time they save
        let session = editor_loop_asking(
            &text,
            &label,
            editor,
            "yml",
            |saved| {
                *latest.borrow_mut() = saved.to_string();
                parse_saved::<E>(saved, attempt.data)
            },
            || prompt_error_action_with(attempt.seams),
        )
        .await?;
        let (view, saved) = match session {
            // a buffer with nothing in it cancels the edit
            Some(None) => return Ok(cancelled::<E>(attempt, None)),
            // cancelling after an error abandons text that was never applied
            None => {
                let message = Error::new(format!(
                    "Edit of {} '{name}' was cancelled after an invalid edit",
                    E::KIND.to_lowercase()
                ));
                return Err(recoverable(&latest.borrow(), &label, &message)?);
            }
            Some(Some(parsed)) => parsed,
        };
        // try to apply what they saved
        match attempt.apply(view, &Source::Editor).await {
            Ok(mut outcome) => {
                // keep a declined edit recoverable
                if outcome.status == EditStatus::Cancelled {
                    let path = save_recovery_file(&saved, &label, "yml")?;
                    eprintln!("Your edits were saved to '{}'", path.display());
                    outcome.saved_to = Some(path);
                }
                return Ok(outcome);
            }
            Err(Failure::Retry(err)) => {
                eprintln!(
                    "{} Failed to update {} '{name}': {err}",
                    "Error:".bright_red().bold(),
                    E::KIND.to_lowercase()
                );
                // reopen the editor with their own text to fix the problem
                if matches!(prompt_error_action_with(attempt.seams)?, ErrorAction::Edit) {
                    text = saved;
                    continue;
                }
                return Err(recoverable(&saved, &label, &err)?);
            }
            Err(Failure::Fatal(err)) => return Err(recoverable(&saved, &label, &err)?),
        }
    }
}

/// Check the text a user saved: syntax and view type first, then the resource's own checks
///
/// Returns `None` for a buffer with nothing but comments, which cancels the edit.
///
/// # Arguments
///
/// * `saved` - The text read back from the editor
/// * `data` - The resource as fetched
fn parse_saved<E: Editable>(
    saved: &str,
    data: &E::Data,
) -> Result<Option<(E::View, String)>, ViewError> {
    // an empty buffer cancels
    if is_blank(saved) {
        return Ok(None);
    }
    // syntax and view-type errors carry the line and column
    let view = serde_norway::from_str::<E::View>(saved).map_err(|err| ViewError {
        location: err.location().map(|loc| (loc.line(), loc.column())),
        message: err.to_string(),
    })?;
    // the resource's own checks have no position
    E::validate(&view, data).map_err(|err| ViewError {
        message: match err {
            Error::Generic(message) => message,
            other => other.to_string(),
        },
        location: None,
    })?;
    Ok(Some((view, saved.to_string())))
}

/// Build the outcome of an edit that was cancelled before anything was sent
///
/// # Arguments
///
/// * `attempt` - The state of this edit
/// * `saved_to` - Where the edit text was saved, if it was
fn cancelled<E: Editable>(attempt: &Attempt<'_, E>, saved_to: Option<PathBuf>) -> EditOutcome {
    EditOutcome {
        kind: E::KIND.to_string(),
        id: E::id(attempt.data),
        name: E::name(attempt.data),
        status: EditStatus::Cancelled,
        diff: Vec::new(),
        resource: None,
        saved_to,
    }
}

/// Save an abandoned edit and build the error that names the file
///
/// # Arguments
///
/// * `saved` - The text the user saved
/// * `label` - A label for the file name
/// * `err` - Why the edit could not be applied
fn recoverable(saved: &str, label: &str, err: &Error) -> Result<Error, Error> {
    // write the edit somewhere only the user can read
    let path = save_recovery_file(saved, label, "yml")?;
    let message = match err {
        Error::Generic(message) => message.clone(),
        other => other.to_string(),
    };
    Ok(Error::new(format!(
        "{message} (your edits were saved to '{}')",
        path.display()
    )))
}

/// Print an outcome in the requested format
///
/// # Arguments
///
/// * `outcome` - What the edit did
/// * `format` - The format to print in; people-readable text by default
fn print_outcome(outcome: &EditOutcome, format: Option<OutputFormat>) -> Result<(), Error> {
    match format {
        // documents carry the whole outcome
        Some(OutputFormat::Json) => print!("{}", spec_io::render_json(&outcome.to_json())?),
        Some(OutputFormat::Yaml) => print!("{}", spec_io::render_yaml(&outcome.to_json())?),
        // everything else is a sentence
        _ => {
            let kind = outcome.kind.to_lowercase();
            match outcome.status {
                EditStatus::Updated => println!(
                    "{} {} {}",
                    outcome.kind.bright_green(),
                    format!("'{}'", outcome.name).yellow(),
                    "updated successfully!".bright_green()
                ),
                EditStatus::NoChanges => {
                    println!("No changes detected for {kind} '{}'.", outcome.name);
                }
                EditStatus::DryRun => {
                    println!("Dry run: {kind} '{}' was not changed.", outcome.name);
                }
                EditStatus::Cancelled => println!("Cancelled."),
            }
        }
    }
    Ok(())
}
