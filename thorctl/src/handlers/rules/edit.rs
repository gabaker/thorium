//! Handles editing a detection rule's source in an editor
//!
//! The rule opens as a raw `.yar` or `.yml` buffer depending on its engine so
//! editors highlight the right syntax. Server-side rejections (e.g. yara-x
//! compile errors) reopen the editor seeded with the edited text, and fatal
//! failures (an editor abort on a re-edit, a non-400 submit error) save the
//! edited buffer to a kept temp file, so the user's work is never lost.

use owo_colors::OwoColorize;
use std::collections::BTreeSet;
use std::path::PathBuf;
use thorium::models::{EntityKinds, EntityMetadata, EntityMetadataUpdate, EntityUpdate, YaraRule};
use thorium::{CtlConf, Error, Thorium};
use uuid::Uuid;

use crate::args::rules::EditRule;
use crate::handlers::imports::editor::{
    ErrorAction, PlainTextError, edit_text, prompt_error_action, resolve_editor,
};
use crate::utils;

use super::get_rule;

/// Check whether an edited source is unchanged from the original
///
/// Editors typically ensure a trailing newline on save so at most one
/// trailing newline is stripped from each side before comparing; the text is
/// still submitted exactly as saved when it did change.
///
/// # Arguments
///
/// * `original` - The rule source before the edit
/// * `edited` - The rule source after the edit
fn is_unchanged(original: &str, edited: &str) -> bool {
    // strip at most one trailing newline from each side before comparing
    original.strip_suffix('\n').unwrap_or(original) == edited.strip_suffix('\n').unwrap_or(edited)
}

/// Save an edited rule buffer to a kept file so a failure never loses work
///
/// The editor's own temp file is deleted on every return, so fatal paths copy
/// the latest text somewhere durable and name it in their error output. The
/// write is best effort: failing to rescue must never mask the real error.
///
/// # Arguments
///
/// * `id` - The id of the rule being edited
/// * `ext` - The file extension matching this rules engine
/// * `text` - The edited text to save
fn rescue_edit(id: &Uuid, ext: &str, text: &str) -> Option<PathBuf> {
    // build a kept path for this rescue in the temp dir
    let path = std::env::temp_dir().join(format!("thorium-rule-edit-{id}.{ext}"));
    // best effort write so losing the rescue cannot mask the real error
    match std::fs::write(&path, text) {
        Ok(()) => Some(path),
        Err(_) => None,
    }
}

/// Build the engine specific metadata update carrying a new rule source
///
/// # Arguments
///
/// * `kind` - The entity kind of the rule being edited
/// * `source` - The new rule source to set
fn build_update(kind: EntityKinds, source: String) -> EntityMetadataUpdate {
    // build the metadata update matching this rule's engine
    match kind {
        EntityKinds::YaraRule => EntityMetadataUpdate::YaraRule {
            yara_rule: Some(source),
            score: None,
            add_applies_to: Vec::default(),
            remove_applies_to: Vec::default(),
            // action updates are not exposed in thorctl v1
            add_actions: Vec::default(),
            remove_actions: BTreeSet::default(),
        },
        EntityKinds::SigmaRule => EntityMetadataUpdate::SigmaRule {
            sigma_rule: Some(source),
            score: None,
            add_applies_to: Vec::default(),
            remove_applies_to: Vec::default(),
            // action updates are not exposed in thorctl v1
            add_actions: Vec::default(),
            remove_actions: BTreeSet::default(),
        },
        // get_rule already rejected non-rule kinds
        _ => unreachable!("non-rule kinds are rejected when resolving the rule"),
    }
}

/// Edit a detection rule's source in an editor
///
/// # Arguments
///
/// * `conf` - The Thorctl config whose default editor is the fallback
/// * `thorium` - A Thorium client
/// * `cmd` - The edit rule command to execute
pub async fn edit(conf: &CtlConf, thorium: &Thorium, cmd: &EditRule) -> Result<(), Error> {
    // resolve this rule and make sure it is one
    let entity = get_rule(thorium, &cmd.rule).await?;
    // pull this rules source and pick the extension for editor highlighting
    let (source, ext) = match &entity.metadata {
        EntityMetadata::YaraRule(rule) => (rule.rule.clone(), "yar"),
        EntityMetadata::SigmaRule(rule) => (rule.rule.clone(), "yml"),
        // get_rule only returns rule kinds so other kinds cannot appear
        _ => unreachable!("non-rule kinds are rejected when resolving the rule"),
    };
    // resolve the editor to edit this rule with
    let editor = resolve_editor(cmd.editor.as_deref(), conf);
    // remember this rules kind for the engine specific pieces below
    let kind = entity.kind;
    // validate saved text client side: non empty and under the yara size cap;
    // compiling happens server side on submit
    let validate = move |text: &str| {
        // empty rules can never be valid
        if text.trim().is_empty() {
            return Err(PlainTextError("Rules cannot be empty!".to_owned()));
        }
        // enforce the yara size cap client side like rules create does
        if kind == EntityKinds::YaraRule && text.len() > YaraRule::MAX_RULE_SIZE {
            return Err(PlainTextError(format!(
                "Yara rules cannot be larger then {} bytes!",
                YaraRule::MAX_RULE_SIZE
            )));
        }
        Ok(())
    };
    // seed the first editor pass with this rules current source
    let mut seed = source.clone();
    loop {
        // open the editor on the seeded source
        let edited = match edit_text(&seed, &entity.id.to_string(), editor, ext, &validate).await {
            Ok(Some(edited)) => edited,
            // the user cancelled from the error prompt
            Ok(None) => {
                println!("Cancelled.");
                return Ok(());
            }
            // an editor failure on a re-edit pass would drop the prior edits
            Err(error) => {
                // rescue the prior pass's edits before surfacing the error
                if !is_unchanged(&source, &seed)
                    && let Some(path) = rescue_edit(&entity.id, ext, &seed)
                {
                    eprintln!("Your edited rule was saved to {}", path.display());
                }
                return Err(error);
            }
        };
        // a save with no changes is a no-op
        if is_unchanged(&source, &edited) {
            println!("No changes detected!");
            return Ok(());
        }
        // build the update carrying the edited source exactly as saved
        let update = EntityUpdate::default().metadata(build_update(kind, edited.clone()));
        // send this update to Thorium using the resolved id
        match thorium.entities.update(entity.id, update).await {
            Ok(_) => {
                // show that this rule was updated
                println!(
                    "{} rule '{}' ({})",
                    "Updated".bright_green(),
                    entity.name,
                    entity.id
                );
                return Ok(());
            }
            // a 400 is a server-side rejection (e.g. yara-x compile errors)
            Err(error) if error.status().map(|status| status.as_u16()) == Some(400) => {
                // surface the servers validation errors verbatim
                eprintln!(
                    "{} {}",
                    "Error:".bright_red().bold(),
                    utils::api_error_text(&error)
                        .unwrap_or_else(|| "Invalid rule update".to_owned())
                );
                // offer to reopen the editor so the users work is never lost
                match prompt_error_action() {
                    // reopen the editor seeded with the rejected text
                    Ok(ErrorAction::Edit) => seed = edited,
                    Ok(ErrorAction::Cancel) => {
                        println!("Cancelled.");
                        return Ok(());
                    }
                    // a failed prompt (e.g. no TTY) would drop the edits
                    Err(error) => {
                        // rescue the edited text before surfacing the error
                        if let Some(path) = rescue_edit(&entity.id, ext, &edited) {
                            eprintln!("Your edited rule was saved to {}", path.display());
                        }
                        return Err(error);
                    }
                }
            }
            // anything else is a real error; keep the users edit on disk since
            // a transient 5xx or network failure must never lose their work
            Err(error) => {
                // rescue the edited text before surfacing the error
                if let Some(path) = rescue_edit(&entity.id, ext, &edited) {
                    eprintln!("Your edited rule was saved to {}", path.display());
                }
                return Err(error);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pure open and save must read as unchanged despite trailing newlines
    #[test]
    fn unchanged_normalizes_one_trailing_newline() {
        // an editor appending a final newline is still a no-op
        assert!(is_unchanged(
            "rule a { condition: true }",
            "rule a { condition: true }\n"
        ));
        // identical text is a no-op in both directions
        assert!(is_unchanged("rule a {}\n", "rule a {}\n"));
        assert!(is_unchanged("rule a {}\n", "rule a {}"));
    }

    /// Real changes must never read as unchanged
    #[test]
    fn changed_text_is_detected() {
        // a body edit is a change
        assert!(!is_unchanged(
            "rule a { condition: true }",
            "rule a { condition: false }"
        ));
        // adding more than one trailing newline is a change
        assert!(!is_unchanged("rule a {}", "rule a {}\n\n"));
    }
}
