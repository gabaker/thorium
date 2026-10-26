//! Tests for editing in an editor, driven by editor scripts and scripted prompt answers

use std::path::PathBuf;
use std::time::Duration;

use super::editor::{EditorScript, Step, recovery_path};
use super::fake::{Fake, FakeStore};
use crate::args::helpers::EditArgs;
use crate::handlers::edit::editable::{EditOutcome, EditStatus};
use crate::handlers::edit::flow::{Mode, run};
use crate::handlers::edit::seams::{PromptAnswer, Seams};

/// Seams for a session that counts as a terminal and answers prompts as told
///
/// # Arguments
///
/// * `prompt` - The answer to every prompt, if any
fn seams(prompt: Option<PromptAnswer>) -> Seams {
    // pretend to be a terminal so the editor opens
    Seams {
        assume_tty: true,
        prompt,
        ..Seams::default()
    }
}

/// Run `edit` with an editor script and no non-interactive source
///
/// # Arguments
///
/// * `store` - The store holding the resource
/// * `script` - The editor script
/// * `seams` - The terminal state and prompt answers
/// * `args` - The edit flags (without any change source)
async fn edit(
    store: &FakeStore,
    script: &EditorScript,
    seams: &Seams,
    args: &EditArgs,
) -> Result<EditOutcome, thorium::Error> {
    // open the script as the editor
    run::<Fake>(store, &script.editor(), &(), args, Mode::Edit, seams).await
}

/// The text of an error result
///
/// # Arguments
///
/// * `result` - The result expected to hold an error
fn error_text<T>(result: Result<T, thorium::Error>) -> String {
    // render the error as the command line would show it
    match result {
        Ok(_) => panic!("expected an error"),
        Err(err) => err.to_string(),
    }
}

/// Read and delete the recovery file an error names
///
/// # Arguments
///
/// * `message` - The error text
fn take_recovery(message: &str) -> String {
    // find the file, read it and remove it
    let path = recovery_path(message).unwrap_or_else(|| panic!("no recovery file in {message}"));
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    text
}

/// Changing a field in the editor updates the resource, and the buffer carries a header
#[tokio::test]
async fn editor_changes_are_applied() {
    // edit the description in the editor
    let store = FakeStore::new();
    let script = EditorScript::replace("description: old", "description: from the editor");
    let outcome = edit(&store, &script, &seams(None), &EditArgs::default())
        .await
        .unwrap();
    // the update was sent
    assert_eq!(outcome.status, EditStatus::Updated);
    assert_eq!(store.data().description.as_deref(), Some("from the editor"));
    assert_eq!(script.invocations(), 1);
    // the editor was shown a commented header and the read-only marker
    let seen = script.seen(0).unwrap();
    assert!(
        seen.starts_with("# Editing fake 'agent' (id 11111111"),
        "{seen}"
    );
    assert!(seen.contains("read-only"), "{seen}");
    assert!(seen.contains("'*id*':") || seen.contains("*id*:"), "{seen}");
}

/// Saving the buffer unchanged applies nothing
#[tokio::test]
async fn unchanged_buffer_changes_nothing() {
    // save without editing
    let store = FakeStore::new();
    let script = EditorScript::noop();
    let outcome = edit(&store, &script, &seams(None), &EditArgs::default())
        .await
        .unwrap();
    // nothing was sent
    assert_eq!(outcome.status, EditStatus::NoChanges);
    assert_eq!(store.updates(), 0);
}

/// An empty buffer, or one with only comments, cancels the edit
#[tokio::test]
async fn empty_buffer_cancels() {
    for content in ["", "# nothing left\n"] {
        // blank the buffer
        let store = FakeStore::new();
        let script = EditorScript::write(content);
        let outcome = edit(&store, &script, &seams(None), &EditArgs::default())
            .await
            .unwrap();
        // the edit was cancelled without a request
        assert_eq!(outcome.status, EditStatus::Cancelled, "{content:?}");
        assert_eq!(store.updates(), 0);
    }
}

/// An edit to a read-only field is ignored while other edits apply
#[tokio::test]
async fn read_only_edits_in_the_editor_are_ignored() {
    // change the id and the description in one session
    let store = FakeStore::new();
    let script = EditorScript::sequence(vec![Step::replace("11111111", "99999999"), Step::Noop]);
    let outcome = edit(&store, &script, &seams(None), &EditArgs::default())
        .await
        .unwrap();
    // changing only the id leaves nothing to apply
    assert_eq!(outcome.status, EditStatus::NoChanges);
    assert_eq!(store.updates(), 0);
}

/// A syntax error reopens the editor with the user's text when they choose to edit again
#[tokio::test]
async fn syntax_errors_can_be_fixed_in_the_editor() {
    // first save broken YAML, then fix it
    let store = FakeStore::new();
    let script = EditorScript::sequence(vec![
        Step::append("  : : broken ["),
        Step::write(
            "'*id*': x\nname: agent\ndescription: fixed\ngroups: [g1]\nconfig:\n  max_turns: 3\n",
        ),
    ]);
    let outcome = edit(
        &store,
        &script,
        &seams(Some(PromptAnswer::Reedit)),
        &EditArgs::default(),
    )
    .await
    .unwrap();
    // the second editor session saw the broken text and its fix was applied
    assert_eq!(outcome.status, EditStatus::Updated);
    assert_eq!(script.invocations(), 2);
    assert!(script.seen(1).unwrap().contains("broken ["));
    assert_eq!(store.data().description.as_deref(), Some("fixed"));
}

/// The resource's own checks reopen the editor too, and nothing is sent until they pass
#[tokio::test]
async fn semantic_errors_reopen_the_editor() {
    // first empty the groups (invalid), then restore them
    let store = FakeStore::new();
    let script = EditorScript::sequence(vec![
        Step::replace("- g1", "[]"),
        Step::write(
            "'*id*': x\nname: agent\ndescription: valid now\ngroups: [g1]\nconfig:\n  max_turns: 3\n",
        ),
    ]);
    let outcome = edit(
        &store,
        &script,
        &seams(Some(PromptAnswer::Reedit)),
        &EditArgs::default(),
    )
    .await;
    // the invalid first save was never sent
    assert_eq!(outcome.unwrap().status, EditStatus::Updated);
    assert_eq!(store.updates(), 1);
    assert_eq!(script.invocations(), 2);
}

/// Cancelling after a syntax error fails the command and keeps the text in a recovery file
#[tokio::test]
async fn cancel_after_a_syntax_error_keeps_the_text() {
    // save broken YAML and answer the error prompt with cancel
    let store = FakeStore::new();
    let script = EditorScript::append("  : : broken [");
    let result = edit(
        &store,
        &script,
        &seams(Some(PromptAnswer::Cancel)),
        &EditArgs::default(),
    )
    .await;
    // the command fails and the file holds what the user saved
    let message = error_text(result);
    assert!(message.contains("cancelled"), "{message}");
    assert!(take_recovery(&message).contains("broken ["));
    assert_eq!(store.updates(), 0);
}

/// Cancelling after the resource's own checks fail keeps the text too
#[tokio::test]
async fn cancel_after_a_semantic_error_keeps_the_text() {
    // save a document the resource refuses (no groups) and cancel
    let store = FakeStore::new();
    let script = EditorScript::write(
        "'*id*': x\nname: agent\ndescription: kept\ngroups: []\nconfig:\n  max_turns: 3\n",
    );
    let seams = seams(Some(PromptAnswer::Cancel));
    let result = edit(&store, &script, &seams, &EditArgs::default()).await;
    // the command fails and the file holds what the user saved
    let message = error_text(result);
    assert!(message.contains("cancelled"), "{message}");
    assert!(take_recovery(&message).contains("description: kept"));
    assert_eq!(store.updates(), 0);
}

/// A failing editor aborts the edit with its exit code
#[tokio::test]
async fn editor_failure_aborts() {
    // run an editor that fails
    let store = FakeStore::new();
    let script = EditorScript::fail(3);
    let message = error_text(edit(&store, &script, &seams(None), &EditArgs::default()).await);
    // the error names the code and nothing was sent
    assert!(message.contains("error code: 3"), "{message}");
    assert_eq!(store.updates(), 0);
}

/// `--dry-run` shows the changes from the editor and sends nothing
#[tokio::test]
async fn dry_run_applies_nothing_from_the_editor() {
    // edit with --dry-run
    let store = FakeStore::new();
    let script = EditorScript::replace("description: old", "description: new");
    let args = EditArgs {
        dry_run: true,
        ..EditArgs::default()
    };
    let outcome = edit(&store, &script, &seams(None), &args).await.unwrap();
    // the diff is reported and nothing changed
    assert_eq!(outcome.status, EditStatus::DryRun);
    assert_eq!(outcome.diff.len(), 1);
    assert_eq!(store.updates(), 0);
}

/// Declining the confirmation applies nothing and keeps the edit in a recovery file
#[tokio::test]
async fn declined_confirmation_keeps_the_edit() {
    // decline the confirmation
    let store = FakeStore::new();
    let script = EditorScript::replace("description: old", "description: declined");
    let outcome = edit(
        &store,
        &script,
        &seams(Some(PromptAnswer::Cancel)),
        &EditArgs::default(),
    )
    .await
    .unwrap();
    // nothing was sent, and the edit was saved
    assert_eq!(outcome.status, EditStatus::Cancelled);
    assert_eq!(store.updates(), 0);
    let saved: PathBuf = outcome.saved_to.expect("the edit is saved");
    let text = std::fs::read_to_string(&saved).unwrap();
    std::fs::remove_file(&saved).unwrap();
    assert!(text.contains("description: declined"), "{text}");
}

/// `--skip-confirm` applies without asking even when prompts would be declined
#[tokio::test]
async fn skip_confirm_applies_without_asking() {
    // edit with --skip-confirm and a prompt that would cancel
    let store = FakeStore::new();
    let script = EditorScript::replace("description: old", "description: confirmed");
    let args = EditArgs {
        skip_confirm: true,
        ..EditArgs::default()
    };
    let outcome = edit(&store, &script, &seams(Some(PromptAnswer::Cancel)), &args)
        .await
        .unwrap();
    // the update was sent
    assert_eq!(outcome.status, EditStatus::Updated);
    assert_eq!(store.data().description.as_deref(), Some("confirmed"));
}

/// A refused update that is not retried names a recovery file holding the edit
#[tokio::test]
async fn refused_updates_are_recoverable() {
    // make the server refuse and answer the prompt with cancel
    let store = FakeStore::new();
    store.0.lock().unwrap().fail_updates = 1;
    let script = EditorScript::replace("description: old", "description: refused");
    let args = EditArgs {
        skip_confirm: true,
        ..EditArgs::default()
    };
    let message =
        error_text(edit(&store, &script, &seams(Some(PromptAnswer::Cancel)), &args).await);
    // the error carries the reason and the file holds the text that was saved
    assert!(
        message.contains("the server refused the update"),
        "{message}"
    );
    let saved = take_recovery(&message);
    assert!(saved.contains("description: refused"), "{saved}");
}

/// A refused update can be edited again and sent
#[tokio::test]
async fn refused_updates_can_be_retried() {
    // make the server refuse once and answer the prompt with reedit
    let store = FakeStore::new();
    store.0.lock().unwrap().fail_updates = 1;
    let script = EditorScript::sequence(vec![
        Step::replace("description: old", "description: retried"),
        Step::Noop,
    ]);
    let args = EditArgs {
        skip_confirm: true,
        ..EditArgs::default()
    };
    let outcome = edit(&store, &script, &seams(Some(PromptAnswer::Reedit)), &args)
        .await
        .unwrap();
    // the second attempt went through with the user's own text
    assert_eq!(outcome.status, EditStatus::Updated);
    assert_eq!(store.updates(), 2);
    assert_eq!(script.invocations(), 2);
    assert!(script.seen(1).unwrap().contains("description: retried"));
}

/// A resource that changed while the editor was open is not overwritten, and the edit is kept
#[tokio::test]
async fn remote_change_during_edit_is_refused_and_saved() {
    // the guard's read sees a different resource
    let store = FakeStore::new();
    let mut changed = store.data();
    changed.max_turns = 50;
    store.0.lock().unwrap().change_on_fetch = Some((2, changed));
    let script = EditorScript::replace("description: old", "description: mine");
    let message = error_text(edit(&store, &script, &seams(None), &EditArgs::default()).await);
    // the edit was refused but saved
    assert!(message.contains("changed remotely"), "{message}");
    assert_eq!(store.updates(), 0);
    assert!(take_recovery(&message).contains("description: mine"));
}

/// `--force` applies an editor edit over a remote change
#[tokio::test]
async fn force_applies_over_a_remote_change() {
    // schedule the change and force the edit
    let store = FakeStore::new();
    let mut changed = store.data();
    changed.max_turns = 50;
    store.0.lock().unwrap().change_on_fetch = Some((2, changed));
    let script = EditorScript::replace("description: old", "description: forced");
    let args = EditArgs {
        force: true,
        ..EditArgs::default()
    };
    let outcome = edit(&store, &script, &seams(None), &args).await.unwrap();
    // the edit was applied
    assert_eq!(outcome.status, EditStatus::Updated);
    assert_eq!(store.data().description.as_deref(), Some("forced"));
}

/// The pty tests need patience for a debug build to start and print
const PTY_WAIT: Duration = Duration::from_secs(30);

/// The environment variable that tells the pty child which editor script to use
const PTY_EDITOR_VAR: &str = "THORCTL_TEST_PTY_EDITOR";

/// The child half of the pty smoke test
///
/// It does nothing unless the parent test started it with an editor script, so a normal test run
/// passes it trivially. When started by the parent it runs the real flow on the pseudo-terminal
/// it was given: real terminal detection and real prompts.
#[tokio::test]
async fn pty_child() {
    // do nothing unless the parent test started this process
    let Ok(editor) = std::env::var(PTY_EDITOR_VAR) else {
        return;
    };
    // the first update is refused so the re-edit prompt appears
    let store = FakeStore::new();
    store.0.lock().unwrap().fail_updates = 1;
    // run the flow with the real terminal and the real environment
    let args = EditArgs::default();
    let result = run::<Fake>(&store, &editor, &(), &args, Mode::Edit, &Seams::from_env()).await;
    // report the outcome in a form the parent can read
    match result {
        Ok(outcome) => println!(
            "PTY-CHILD status={} description={:?}",
            outcome.status.as_str(),
            store.data().description
        ),
        Err(err) => println!("PTY-CHILD error={err}"),
    }
}

/// On a real terminal the confirmation and the re-edit prompt are drawn and answered by keys
#[cfg(unix)]
#[test]
fn pty_drives_the_confirm_and_reedit_prompts() {
    use super::editor::Pty;
    // the child edits the description, has its first update refused, then edits again unchanged
    let script = EditorScript::sequence(vec![
        Step::replace("description: old", "description: via pty"),
        Step::Noop,
    ]);
    let exe = std::env::current_exe().unwrap();
    let mut pty = Pty::spawn(
        &exe,
        &[
            "--exact",
            "handlers::edit::tests::interactive::pty_child",
            "--nocapture",
            "--test-threads=1",
        ],
        &[(PTY_EDITOR_VAR, &script.editor())],
    )
    .unwrap();
    // confirm the first apply
    pty.read_until("Apply these changes", PTY_WAIT).unwrap();
    pty.send("y").unwrap();
    // the refused update offers to reopen the editor; take the default (Edit)
    pty.read_until("Reopen editor", PTY_WAIT).unwrap();
    pty.send("\n").unwrap();
    // confirm the second apply, which succeeds
    pty.read_until("Apply these changes", PTY_WAIT).unwrap();
    pty.send("y").unwrap();
    // the flow finishes with the user's text applied
    let status = pty.finish(PTY_WAIT).unwrap();
    let output = pty.output().to_string();
    assert!(status.success(), "{output}");
    assert!(
        output.contains("PTY-CHILD status=updated description=Some(\"via pty\")"),
        "{output}"
    );
    assert_eq!(script.invocations(), 2);
}
