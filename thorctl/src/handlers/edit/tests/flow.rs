//! Tests for the non-interactive edit sources and the checks around applying an edit

use serde_json::json;
use std::path::PathBuf;

use super::fake::{Fake, FakeData, FakeStore, args_with, update};
use crate::args::helpers::EditArgs;
use crate::handlers::edit::editable::EditStatus;
use crate::handlers::edit::flow::{Mode, run};
use crate::handlers::edit::seams::Seams;
use crate::utils::output::OutputFormat;

/// A scratch directory removed when dropped
struct Scratch(PathBuf);

impl Scratch {
    /// Create an empty scratch directory under the system temp dir
    fn new() -> Self {
        // use a fresh random name so tests never collide
        let path = std::env::temp_dir().join(format!("thorctl-edit-test-{}", uuid::Uuid::new_v4()));
        // create the directory
        std::fs::create_dir_all(&path).unwrap();
        Scratch(path)
    }

    /// Write a file into the directory and return its path as text
    ///
    /// # Arguments
    ///
    /// * `name` - The file name
    /// * `content` - The file content
    fn write(&self, name: &str, content: &str) -> String {
        // write the file and hand back its path
        let path = self.0.join(name);
        std::fs::write(&path, content).unwrap();
        path.display().to_string()
    }
}

impl Drop for Scratch {
    /// Remove the directory and everything in it
    fn drop(&mut self) {
        // cleanup is best effort
        let _ = std::fs::remove_dir_all(&self.0);
    }
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

/// A full view document for the fake resource with the given description
///
/// # Arguments
///
/// * `id` - The read-only id to write
/// * `description` - The description to write
fn view_file(id: &str, description: &str) -> String {
    // write the same shape `edit` would show
    format!(
        "'*id*': {id}\nname: agent\ndescription: {description}\ngroups: [g1]\nconfig:\n  max_turns: 3\n"
    )
}

/// Flags that read the view from a file
///
/// # Arguments
///
/// * `file` - The path of the file
fn file_args(file: String) -> EditArgs {
    // everything else stays at its default
    EditArgs {
        file: Some(file),
        ..EditArgs::default()
    }
}

/// `--set` changes one field, sends one update and reports the change
#[tokio::test]
async fn set_changes_one_field() {
    // set the description
    let store = FakeStore::new();
    let outcome = update(&store, &args_with(&["set:description=hello"]))
        .await
        .unwrap();
    // the resource changed with exactly one update
    assert_eq!(outcome.status, EditStatus::Updated);
    assert_eq!(store.data().description.as_deref(), Some("hello"));
    assert_eq!(store.updates(), 1);
    // the outcome names the one field that changed
    assert_eq!(outcome.diff.len(), 1);
    assert_eq!(outcome.diff[0].path, "description");
    assert_eq!(outcome.diff[0].old, json!("old"));
    assert_eq!(outcome.diff[0].new, json!("hello"));
}

/// Nested paths and typed values reach the update
#[tokio::test]
async fn set_nested_typed_values() {
    // set a nested number and a list in one edit
    let store = FakeStore::new();
    update(
        &store,
        &args_with(&["set:config.max_turns=5", "set:groups=[g1, g2]"]),
    )
    .await
    .unwrap();
    // both arrived with their types
    assert_eq!(store.data().max_turns, 5);
    assert_eq!(store.data().groups, vec!["g1", "g2"]);
}

/// `--file` replaces the view and an omitted optional field is cleared
#[tokio::test]
async fn file_replaces_the_view() {
    // write a view that omits the description
    let store = FakeStore::new();
    let scratch = Scratch::new();
    let id = store.data().id;
    let file = scratch.write(
        "view.yml",
        &format!("'*id*': {id}\nname: renamed\ngroups: [g9]\nconfig:\n  max_turns: 7\n"),
    );
    update(&store, &file_args(file)).await.unwrap();
    // everything came from the file
    let data = store.data();
    assert_eq!(data.name, "renamed");
    assert_eq!(data.description, None);
    assert_eq!(data.groups, vec!["g9"]);
    assert_eq!(data.max_turns, 7);
}

/// A JSON file works as well as YAML
#[tokio::test]
async fn file_accepts_json() {
    // write the view as JSON
    let store = FakeStore::new();
    let scratch = Scratch::new();
    let id = store.data().id;
    let file = scratch.write(
        "view.json",
        &format!(
            "{{\"*id*\": \"{id}\", \"name\": \"agent\", \"description\": \"json\", \"groups\": [\"g1\"], \"config\": {{\"max_turns\": 3}}}}"
        ),
    );
    update(&store, &file_args(file)).await.unwrap();
    // the description came from the JSON
    assert_eq!(store.data().description.as_deref(), Some("json"));
}

/// The file applies first, then patches, then set and unset
#[tokio::test]
async fn sources_apply_in_a_fixed_order() {
    // start from a file that sets the description
    let scratch = Scratch::new();
    let store = FakeStore::new();
    let file = scratch.write("view.yml", &view_file(&store.data().id, "from-file"));
    let mut args = file_args(file);
    update(&store, &args).await.unwrap();
    assert_eq!(store.data().description.as_deref(), Some("from-file"));
    // a patch wins over the file
    args.patch = vec!["{\"description\": \"from-patch\"}".to_string()];
    update(&store, &args).await.unwrap();
    assert_eq!(store.data().description.as_deref(), Some("from-patch"));
    // a set wins over the patch
    args.ops = args_with(&["set:description=from-set"]).ops;
    update(&store, &args).await.unwrap();
    assert_eq!(store.data().description.as_deref(), Some("from-set"));
}

/// A merge patch with null removes a field
#[tokio::test]
async fn patch_null_clears_a_field() {
    // clear the description with a null in the patch
    let store = FakeStore::new();
    let args = EditArgs {
        patch: vec!["{\"description\": null}".to_string()],
        ..EditArgs::default()
    };
    update(&store, &args).await.unwrap();
    // the field is gone
    assert_eq!(store.data().description, None);
}

/// `--set` and `--unset` apply in the order given
#[tokio::test]
async fn set_and_unset_apply_in_order() {
    // unset then set leaves the value
    let store = FakeStore::new();
    let ops = ["unset:description", "set:description=again"];
    update(&store, &args_with(&ops)).await.unwrap();
    assert_eq!(store.data().description.as_deref(), Some("again"));
    // set then unset leaves nothing
    let ops = ["set:description=gone", "unset:description"];
    update(&store, &args_with(&ops)).await.unwrap();
    assert_eq!(store.data().description, None);
}

/// A view that fails to parse or validate sends nothing
#[tokio::test]
async fn invalid_edits_send_nothing() {
    let store = FakeStore::new();
    // an unknown key
    let unknown = error_text(update(&store, &args_with(&["set:bogus=1"])).await);
    assert!(unknown.contains("unknown field"), "{unknown}");
    // a value of the wrong type
    let wrong_type = error_text(update(&store, &args_with(&["set:config.max_turns=lots"])).await);
    assert!(wrong_type.contains("invalid type"), "{wrong_type}");
    // the resource's own check
    let invalid = error_text(update(&store, &args_with(&["set:groups=[]"])).await);
    assert!(invalid.contains("groups must not be empty"), "{invalid}");
    // a missing file
    let missing = file_args("/nonexistent/thorctl/view.yml".to_string());
    let missing = error_text(update(&store, &missing).await);
    assert!(
        missing.contains("/nonexistent/thorctl/view.yml"),
        "{missing}"
    );
    // a patch that is not a document
    let args = EditArgs {
        patch: vec!["{\"description\": ".to_string()],
        ..EditArgs::default()
    };
    assert!(error_text(update(&store, &args).await).contains("--patch"));
    // none of them reached the server
    assert_eq!(store.updates(), 0);
    assert_eq!(store.data().description.as_deref(), Some("old"));
}

/// Read-only fields cannot be set from the command line
#[tokio::test]
async fn read_only_fields_are_refused() {
    // try each way of naming the read-only id
    let store = FakeStore::new();
    for op in ["set:id=x", "set:*id*=x", "unset:*id*"] {
        let message = error_text(update(&store, &args_with(&[op])).await);
        assert!(message.contains("read-only"), "{op}: {message}");
    }
    // nothing was sent
    assert_eq!(store.updates(), 0);
}

/// A file that changes only a read-only field is ignored with nothing to apply
#[tokio::test]
async fn file_edits_to_read_only_fields_are_ignored() {
    // write a view that differs only in the read-only id
    let store = FakeStore::new();
    let scratch = Scratch::new();
    let file = scratch.write("view.yml", &view_file("some-other-id", "old"));
    let outcome = update(&store, &file_args(file)).await.unwrap();
    // there was nothing else to apply
    assert_eq!(outcome.status, EditStatus::NoChanges);
    assert_eq!(store.updates(), 0);
}

/// Setting a field to its current value is a no-op that sends nothing
#[tokio::test]
async fn unchanged_edits_report_no_changes() {
    // set the description to what it already is
    let store = FakeStore::new();
    let outcome = update(&store, &args_with(&["set:description=old"]))
        .await
        .unwrap();
    // nothing changed and nothing was sent
    assert_eq!(outcome.status, EditStatus::NoChanges);
    assert!(outcome.diff.is_empty(), "{:?}", outcome.diff);
    assert_eq!(store.updates(), 0);
}

/// `--dry-run` reports the changes and sends nothing
#[tokio::test]
async fn dry_run_sends_nothing() {
    // ask for a change as a dry run
    let store = FakeStore::new();
    let mut args = args_with(&["set:description=hello"]);
    args.dry_run = true;
    let outcome = update(&store, &args).await.unwrap();
    // the change is reported but not made
    assert_eq!(outcome.status, EditStatus::DryRun);
    assert_eq!(outcome.diff.len(), 1);
    assert!(!outcome.changed());
    assert_eq!(store.updates(), 0);
    assert_eq!(store.data().description.as_deref(), Some("old"));
}

/// `update` without a source never reads the resource and says what to pass
#[tokio::test]
async fn update_needs_a_source() {
    // update with no flags
    let store = FakeStore::new();
    let message = error_text(update(&store, &EditArgs::default()).await);
    // the error names the flags and nothing was read
    assert!(message.contains("Nothing to update"), "{message}");
    assert!(message.contains("--set"), "{message}");
    assert_eq!(store.fetches(), 0);
}

/// `edit` without a source and without a terminal refuses and points at the flags
#[tokio::test]
async fn edit_needs_a_terminal() {
    // edit with no flags and no terminal
    let store = FakeStore::new();
    let args = EditArgs::default();
    let result = run::<Fake>(&store, "true", &(), &args, Mode::Edit, &Seams::default()).await;
    // the error names the flags that work without one, and nothing was read
    let message = error_text(result);
    assert!(message.contains("interactive terminal"), "{message}");
    assert!(message.contains("--file"), "{message}");
    assert_eq!(store.fetches(), 0);
}

/// `edit` with a flag source never needs a terminal or an editor
#[tokio::test]
async fn edit_with_flags_skips_the_editor() {
    // edit with a flag and an editor that cannot run
    let store = FakeStore::new();
    let args = args_with(&["set:description=via-edit"]);
    let editor = "an-editor-that-does-not-exist";
    let outcome = run::<Fake>(&store, editor, &(), &args, Mode::Edit, &Seams::default())
        .await
        .unwrap();
    // the flag was applied without the editor
    assert_eq!(outcome.status, EditStatus::Updated);
    assert_eq!(store.data().description.as_deref(), Some("via-edit"));
}

/// A resource that changed between the read and the apply is not overwritten
#[tokio::test]
async fn remote_changes_are_refused() {
    // the second read of the resource (the guard's) sees a different resource
    let store = FakeStore::new();
    let mut changed = store.data();
    changed.description = Some("changed elsewhere".to_string());
    store.0.lock().unwrap().change_on_fetch = Some((2, changed));
    // the edit is refused with advice
    let message = error_text(update(&store, &args_with(&["set:config.max_turns=9"])).await);
    assert!(message.contains("changed remotely"), "{message}");
    assert!(message.contains("--force"), "{message}");
    assert_eq!(store.updates(), 0);
}

/// `--force` applies over a remote change
#[tokio::test]
async fn force_overrides_the_guard() {
    // schedule a remote change at the guard's read
    let store = FakeStore::new();
    let mut changed: FakeData = store.data();
    changed.description = Some("changed elsewhere".to_string());
    store.0.lock().unwrap().change_on_fetch = Some((2, changed));
    // apply with --force
    let mut args = args_with(&["set:config.max_turns=9"]);
    args.force = true;
    let outcome = update(&store, &args).await.unwrap();
    // the update went through
    assert_eq!(outcome.status, EditStatus::Updated);
    assert_eq!(store.updates(), 1);
    assert_eq!(store.data().max_turns, 9);
}

/// A failing before-apply hook aborts the edit before anything is sent
#[tokio::test]
async fn before_apply_hook_failure_aborts() {
    // run an edit whose hook fails
    let store = FakeStore::new();
    let seams = Seams {
        before_apply: Some("exit 3".to_string()),
        ..Seams::default()
    };
    let args = args_with(&["set:description=x"]);
    let result = run::<Fake>(&store, "", &(), &args, Mode::Update, &seams).await;
    // the error names the hook and nothing was sent
    assert!(error_text(result).contains("THORCTL_EDIT_BEFORE_APPLY"));
    assert_eq!(store.updates(), 0);
}

/// A refusal from the server is returned as is for flag edits
#[tokio::test]
async fn server_refusals_are_returned() {
    // make the server refuse the update
    let store = FakeStore::new();
    store.0.lock().unwrap().fail_updates = 1;
    let message = error_text(update(&store, &args_with(&["set:description=x"])).await);
    // the refusal reaches the caller
    assert!(
        message.contains("the server refused the update"),
        "{message}"
    );
    assert_eq!(store.updates(), 1);
}

/// The JSON result carries the kind, id, diff and the updated resource
#[tokio::test]
async fn json_result_shape() {
    // apply a change and ask for JSON
    let store = FakeStore::new();
    let mut args = args_with(&["set:description=hello"]);
    args.format = Some(OutputFormat::Json);
    let outcome = update(&store, &args).await.unwrap();
    // the document names the resource and what changed
    let document = outcome.to_json();
    assert_eq!(document["kind"], "fake");
    assert_eq!(document["id"], "11111111-2222-3333-4444-555555555555");
    assert_eq!(document["name"], "agent");
    assert_eq!(document["changed"], true);
    assert_eq!(document["status"], "updated");
    assert_eq!(
        document["diff"],
        json!([{"path": "description", "old": "old", "new": "hello"}])
    );
    // and carries the updated resource
    assert_eq!(document["resource"]["description"], "hello");
}
