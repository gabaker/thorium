//! Tests the thorctl commands that print, edit, update and delete tools against the test API

mod common;

use common::editor::{EditorScript, Step, recovery_path};
use common::{TestUser, assert_success, json, stdout, unique};
use serde_json::{Value, json};
use std::process::{Output, Stdio};
use std::time::Duration;
use thorium::Error;
use thorium::models::{
    Entity, EntityMetadata, EntityMetadataRequest, EntityRequest, GroupRequest, GroupUsersRequest,
    ToolEntityRequest, ToolSpec,
};
use thorium::test_utilities::generators;
use tokio::io::AsyncWriteExt;

/// The environment that lets thorctl open an editor script without a terminal
const EDIT_ENV: [(&str, &str); 1] = [("THORCTL_EDIT_ASSUME_TTY", "1")];

/// Run thorctl to completion with extra environment variables
///
/// # Arguments
///
/// * `user` - The user to run as
/// * `_name` - A label for the run, which names what it checks
/// * `args` - The thorctl arguments after the global flags
/// * `envs` - The environment variables to set
async fn run_with(user: &TestUser, _name: &str, args: &[&str], envs: &[(&str, &str)]) -> Output {
    // set the variables and run, giving up if it hangs
    let mut command = user.command(args);
    command.envs(envs.iter().copied());
    tokio::time::timeout(Duration::from_secs(120), command.output())
        .await
        .expect("thorctl timed out")
        .expect("failed to run thorctl")
}

/// Run thorctl with text piped to its stdin
///
/// # Arguments
///
/// * `user` - The user to run as
/// * `args` - The thorctl arguments after the global flags
/// * `input` - The text to write to stdin
async fn run_stdin(user: &TestUser, args: &[&str], input: &str) -> Output {
    // start thorctl with a pipe for stdin
    let mut child = user
        .command(args)
        .stdin(Stdio::piped())
        .spawn()
        .expect("failed to start thorctl");
    child
        .stdin
        .take()
        .expect("no stdin")
        .write_all(input.as_bytes())
        .await
        .expect("failed to write stdin");
    child.wait_with_output().await.expect("thorctl failed")
}

/// Write text to a unique file
///
/// # Arguments
///
/// * `ext` - The file's extension
/// * `text` - The file's content
fn write_file(ext: &str, text: &str) -> String {
    // pick a unique path in the temp dir
    let path = std::env::temp_dir().join(format!("{}.{ext}", unique("tools-edit")));
    std::fs::write(&path, text).expect("failed to write the file");
    path.to_string_lossy().into_owned()
}

/// Create a group the user owns
///
/// # Arguments
///
/// * `user` - The user who owns the group
async fn other_group(user: &TestUser) -> Result<String, Error> {
    // the admin creates it with the user as an owner
    let other = unique("ctlgroup");
    let owners = GroupUsersRequest::default()
        .direct("thorium")
        .direct(&user.username);
    user.admin
        .groups
        .create(&GroupRequest::new(&other).owners(owners))
        .await?;
    Ok(other)
}

/// Describe a tool as JSON through thorctl
///
/// # Arguments
///
/// * `user` - The user to run as
/// * `name` - The tool's name
async fn describe(user: &TestUser, name: &str) -> Value {
    // describe it as JSON
    let out = user
        .run("describe", &["tools", "describe", name, "--format", "json"])
        .await;
    assert_success(&out);
    json(&out)
}

/// The versions of a tool as labels, newest first
///
/// # Arguments
///
/// * `user` - The user to run as
/// * `name` - The tool's name
async fn labels(user: &TestUser, name: &str) -> Vec<String> {
    // list them as JSON
    let out = user
        .run("labels", &["tools", "versions", name, "-o", "json"])
        .await;
    assert_success(&out);
    json(&out)
        .as_array()
        .expect("a list of versions")
        .iter()
        .map(|version| {
            let label = &version["version"];
            label["SemVer"]
                .as_str()
                .or_else(|| label["Custom"].as_str())
                .expect("a version label")
                .to_owned()
        })
        .collect()
}

/// Write a spec to a unique YAML file
///
/// # Arguments
///
/// * `spec` - The spec to write
fn write_spec(spec: &ToolSpec) -> String {
    // pick a unique path in the temp dir
    let path = std::env::temp_dir().join(format!("{}.yaml", unique("spec")));
    // write the kind as the top-level key, the way people write specs
    let value = serde_json::to_value(spec).expect("failed to convert the spec");
    let raw = serde_norway::to_string(&value).expect("failed to write yaml");
    std::fs::write(&path, raw).expect("failed to write the spec");
    path.to_string_lossy().into_owned()
}

/// The text a command wrote to stderr
///
/// # Arguments
///
/// * `output` - The command's output
fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Create an agentic tool with `tools create` at 1.0.0, returning its name
///
/// # Arguments
///
/// * `user` - The user to run thorctl as
/// * `transcript` - A label for the run, which names what it checks
async fn create_tool(user: &TestUser, transcript: &str) -> String {
    // create the tool from a fresh agentic spec
    let name = unique("tool");
    let spec = write_spec(&generators::gen_agentic_spec());
    let args = [
        "tools",
        "create",
        "--spec",
        &spec,
        "-n",
        &name,
        "-g",
        &user.group,
        "-v",
        "1.0.0",
    ];
    assert_success(&user.run(transcript, &args).await);
    name
}

/// Read a tool by name through the client
///
/// # Arguments
///
/// * `user` - The user to read as
/// * `name` - The tool's name
async fn read_tool(user: &TestUser, name: &str) -> Entity {
    // describe it as JSON and read it back through the client by id
    let out = user
        .run(
            "read-tool",
            &["tools", "describe", name, "--format", "json"],
        )
        .await;
    assert_success(&out);
    let id = json(&out)["id"]
        .as_str()
        .expect("a tool id")
        .parse()
        .expect("an id");
    user.client.entities.get(id).await.expect("read the tool")
}

/// How many versions a tool has
///
/// # Arguments
///
/// * `tool` - The tool
fn version_count(tool: &Entity) -> usize {
    // only tools have versions
    match &tool.metadata {
        EntityMetadata::Tool(meta) => meta.instances.len(),
        _ => panic!("expected a tool"),
    }
}

/// `tools spec` prints a spec that `tools publish` reads back as a no-op, for a name, a version
/// and the id of a version, in YAML and JSON
#[tokio::test]
async fn tools_spec_round_trips() -> Result<(), Error> {
    // a tool with one version
    let user = TestUser::new("tools-spec").await?;
    let name = create_tool(&user, "tools-spec-create").await;
    // the spec is written to a file by name
    let path = std::env::temp_dir().join(format!("{}.yaml", unique("exported")));
    let path = path.to_string_lossy().into_owned();
    let args = ["tools", "spec", &name, "-o", &path];
    assert_success(&user.run("tools-spec-file", &args).await);
    // publishing it again is the API's no-op conflict
    let args = ["tools", "publish", &name, "-v", "1.0.1", "--spec", &path];
    let out = user.run("tools-spec-republish", &args).await;
    assert!(!out.status.success());
    assert!(stderr(&out).contains("409"), "{}", stderr(&out));
    // a version is named after an @, and JSON parses as one document with the kind as its key
    let args = [
        "tools",
        "spec",
        &format!("{name}@1.0.0"),
        "--format",
        "json",
    ];
    let out = user.run("tools-spec-version-json", &args).await;
    assert_success(&out);
    assert!(json(&out).get("Agentic").is_some(), "{}", stdout(&out));
    // the id of a version prints the same spec as its tool's latest
    let tool = read_tool(&user, &name).await;
    let EntityMetadata::Tool(meta) = &tool.metadata else {
        panic!("expected a tool");
    };
    let latest = meta.latest.expect("a latest version").to_string();
    let by_id = user
        .run("tools-spec-by-id", &["tools", "spec", &latest])
        .await;
    let by_name = user
        .run("tools-spec-by-name", &["tools", "spec", &name])
        .await;
    assert_success(&by_id);
    assert_eq!(stdout(&by_id), stdout(&by_name));
    // an unknown version names the versions the tool has
    let args = ["tools", "spec", &format!("{name}@9.9.9")];
    let out = user.run("tools-spec-unknown-version", &args).await;
    assert!(!out.status.success());
    assert!(stderr(&out).contains("1.0.0"), "{}", stderr(&out));
    Ok(())
}

/// `tools delete` needs `--all-versions` for a tool with versions, deletes one version alone,
/// asks for `--skip-confirm` without a terminal, and `--dry-run` deletes nothing
#[tokio::test]
async fn tools_delete() -> Result<(), Error> {
    // a tool with two versions
    let user = TestUser::new("tools-delete").await?;
    let name = create_tool(&user, "tools-delete-create").await;
    let next = write_spec(&generators::gen_agentic_spec());
    let args = ["tools", "publish", &name, "-v", "1.1.0", "--spec", &next];
    assert_success(&user.run("tools-delete-publish", &args).await);
    // a tool with versions is refused, and nothing is deleted
    let out = user
        .run("tools-delete-refused", &["tools", "delete", &name, "--yes"])
        .await;
    assert!(!out.status.success());
    assert!(stderr(&out).contains("--all-versions"), "{}", stderr(&out));
    assert_eq!(version_count(&read_tool(&user, &name).await), 2);
    // a dry run says what would go and deletes nothing
    let args = ["tools", "delete", &name, "--all-versions", "--dry-run"];
    let out = user.run("tools-delete-dry-run", &args).await;
    assert_success(&out);
    assert!(stdout(&out).contains("Would delete"), "{}", stdout(&out));
    assert_eq!(version_count(&read_tool(&user, &name).await), 2);
    // without a terminal the confirmation needs --skip-confirm
    let args = ["tools", "delete", &format!("{name}@1.1.0")];
    let out = user.run("tools-delete-no-terminal", &args).await;
    assert!(!out.status.success());
    assert!(stderr(&out).contains("--skip-confirm"), "{}", stderr(&out));
    assert_eq!(version_count(&read_tool(&user, &name).await), 2);
    // one version is deleted alone
    let args = [
        "tools",
        "delete",
        &format!("{name}@1.1.0"),
        "--skip-confirm",
    ];
    assert_success(&user.run("tools-delete-version", &args).await);
    assert_eq!(version_count(&read_tool(&user, &name).await), 1);
    // --all-versions can't be combined with a version
    let args = [
        "tools",
        "delete",
        &format!("{name}@1.0.0"),
        "--all-versions",
        "--yes",
    ];
    assert!(!user.run("tools-delete-both", &args).await.status.success());
    // --all-versions deletes the last version and the tool
    let args = ["tools", "delete", &name, "--all-versions", "--yes"];
    let out = user.run("tools-delete-all", &args).await;
    assert_success(&out);
    assert!(stdout(&out).contains("Deleted tool"), "{}", stdout(&out));
    let out = user
        .run("tools-delete-gone", &["tools", "describe", &name])
        .await;
    assert!(!out.status.success());
    Ok(())
}

/// The tags of a tool, as `key=value` pairs in order
///
/// # Arguments
///
/// * `tool` - The tool as JSON
fn tag_pairs(tool: &Value) -> Vec<String> {
    // each tag key has its values as keys of an object
    let mut pairs = Vec::new();
    for (key, values) in tool["tags"].as_object().into_iter().flatten() {
        // Thorium sets the submitter tag
        if key == "submitter" {
            continue;
        }
        for value in values
            .as_object()
            .into_iter()
            .flatten()
            .map(|(value, _)| value)
        {
            pairs.push(format!("{key}={value}"));
        }
    }
    pairs.sort();
    pairs
}

/// `tools update` changes the name, description, groups and tags with one flag each, prints a
/// dry run without writing, and refuses to mix flags with a document change
#[tokio::test]
async fn tools_update_flags() -> Result<(), Error> {
    // a tool and a second group
    let user = TestUser::new("tools-update-flags").await?;
    let name = create_tool(&user, "tools-update-create").await;
    let other = other_group(&user).await?;
    // every setting flag at once
    let renamed = unique("renamed");
    let args = [
        "tools",
        "update",
        &name,
        "--rename",
        &renamed,
        "-d",
        "Scans files",
        "--add-group",
        &other,
        "--tag",
        "env=prod",
        "--tag",
        "env=dev",
    ];
    let out = user.run("tools-update-all", &args).await;
    assert_success(&out);
    assert!(
        stdout(&out).contains("updated successfully"),
        "{}",
        stdout(&out)
    );
    let tool = describe(&user, &renamed).await;
    assert_eq!(tool["description"], "Scans files");
    let groups = tool["groups"].as_array().unwrap();
    assert!(groups.iter().any(|group| group == &json!(other)), "{tool}");
    assert_eq!(tag_pairs(&tool), vec!["env=dev", "env=prod"]);
    // the old name is gone
    assert!(
        !user
            .run("tools-update-old-name", &["tools", "describe", &name])
            .await
            .status
            .success()
    );
    // removing a tag, a group and the description
    let args = [
        "tools",
        "update",
        &renamed,
        "--remove-tag",
        "env=dev",
        "--remove-group",
        &other,
        "--clear-description",
    ];
    assert_success(&user.run("tools-update-remove", &args).await);
    let tool = describe(&user, &renamed).await;
    assert_eq!(tool["description"], Value::Null);
    assert_eq!(tool["groups"], json!([user.group]));
    assert_eq!(tag_pairs(&tool), vec!["env=prod"]);
    // a dry run shows the change and writes nothing
    let args = ["tools", "update", &renamed, "-d", "Unwritten", "--dry-run"];
    let out = user.run("tools-update-dry-run", &args).await;
    assert_success(&out);
    assert!(stdout(&out).contains("Dry run"), "{}", stdout(&out));
    assert!(stdout(&out).contains("Unwritten"), "{}", stdout(&out));
    assert_eq!(describe(&user, &renamed).await["description"], Value::Null);
    // setting a value to what it is says nothing changed
    let args = ["tools", "update", &renamed, "--rename", &renamed];
    let out = user.run("tools-update-no-change", &args).await;
    assert_success(&out);
    assert!(stdout(&out).contains("No changes"), "{}", stdout(&out));
    // flags and a document change can't be mixed
    let args = [
        "tools",
        "update",
        &renamed,
        "-d",
        "x",
        "--set",
        "description=y",
    ];
    let out = user.run("tools-update-mixed", &args).await;
    assert!(!out.status.success());
    assert!(stderr(&out).contains("not both"), "{}", stderr(&out));
    // a tag needs a value
    let args = ["tools", "update", &renamed, "--tag", "novalue"];
    assert!(
        !user
            .run("tools-update-bad-tag", &args)
            .await
            .status
            .success()
    );
    // an update with nothing to change is refused
    let out = user
        .run("tools-update-nothing", &["tools", "update", &renamed])
        .await;
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("Nothing to update"),
        "{}",
        stderr(&out)
    );
    Ok(())
}

/// `tools update` takes the root as a document through `--file`, stdin, `--patch`, `--set` and
/// `--unset`, refuses bad documents before any request, and prints a JSON result
#[tokio::test]
async fn tools_update_documents() -> Result<(), Error> {
    // a tool
    let user = TestUser::new("tools-update-documents").await?;
    let name = create_tool(&user, "tools-update-documents-create").await;
    // --set changes one field and prints the JSON result in the documented shape
    let args = [
        "tools",
        "update",
        &name,
        "--set",
        "description=first",
        "-o",
        "json",
    ];
    let out = user.run("tools-update-set", &args).await;
    assert_success(&out);
    let result = json(&out);
    assert_eq!(result["kind"], "tool");
    assert_eq!(result["name"], json!(name));
    assert_eq!(result["status"], "updated");
    assert_eq!(result["changed"], true);
    assert_eq!(result["diff"][0]["path"], "description");
    assert_eq!(result["diff"][0]["new"], "first");
    assert_eq!(result["resource"]["description"], "first");
    // --unset removes it and --patch sets it
    let args = ["tools", "update", &name, "--unset", "description"];
    assert_success(&user.run("tools-update-unset", &args).await);
    assert_eq!(describe(&user, &name).await["description"], Value::Null);
    let args = [
        "tools",
        "update",
        &name,
        "--patch",
        "{\"description\":\"patched\"}",
    ];
    assert_success(&user.run("tools-update-patch", &args).await);
    assert_eq!(describe(&user, &name).await["description"], "patched");
    // --file replaces the document, from a file and from stdin
    let doc = format!(
        "name: {name}\ndescription: from a file\ngroups: [{}]\ntags: {{}}\n",
        user.group
    );
    let path = write_file("yaml", &doc);
    let args = ["tools", "update", &name, "--file", &path];
    assert_success(&user.run("tools-update-file", &args).await);
    assert_eq!(describe(&user, &name).await["description"], "from a file");
    let from_stdin = doc.replace("from a file", "from stdin");
    let args = ["tools", "update", &name, "--file", "-"];
    assert_success(&run_stdin(&user, &args, &from_stdin).await);
    assert_eq!(describe(&user, &name).await["description"], "from stdin");
    // a document with an unknown key is refused before anything is sent
    let bad = write_file("yaml", &format!("{doc}colour: red\n"));
    let args = ["tools", "update", &name, "--file", &bad];
    let out = user.run("tools-update-unknown-key", &args).await;
    assert!(!out.status.success());
    assert!(stderr(&out).contains("colour"), "{}", stderr(&out));
    assert_eq!(describe(&user, &name).await["description"], "from stdin");
    // a document that isn't YAML is refused too
    let broken = write_file("yaml", "name: [unclosed\n");
    let args = ["tools", "update", &name, "--file", &broken];
    assert!(
        !user
            .run("tools-update-broken", &args)
            .await
            .status
            .success()
    );
    // an empty list of groups is refused before any request
    let args = ["tools", "update", &name, "--set", "groups=[]"];
    let out = user.run("tools-update-no-groups", &args).await;
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("at least one group"),
        "{}",
        stderr(&out)
    );
    // read-only fields can't be set
    let args = ["tools", "update", &name, "--set", "id=other"];
    let out = user.run("tools-update-read-only", &args).await;
    assert!(!out.status.success());
    assert!(stderr(&out).contains("read-only"), "{}", stderr(&out));
    // a group that doesn't exist is the API's refusal, with its reason, and nothing changes
    let missing = unique("nogroup");
    let args = ["tools", "update", &name, "--add-group", &missing];
    let out = user.run("tools-update-refused", &args).await;
    assert!(!out.status.success());
    assert_ne!(stderr(&out), "");
    assert_eq!(describe(&user, &name).await["description"], "from stdin");
    // a harness setting on another kind of tool is refused
    let args = ["tools", "update", &name, "--set", "disabled_models=[gpt-5]"];
    let out = user.run("tools-update-not-harness", &args).await;
    assert!(!out.status.success());
    assert!(stderr(&out).contains("harness"), "{}", stderr(&out));
    Ok(())
}

/// `tools update` stops when the tool changed between being read and being written, unless
/// forced
#[tokio::test]
async fn tools_update_guards_remote_changes() -> Result<(), Error> {
    // a tool, and a hook that changes it between the read and the write
    let user = TestUser::new("tools-update-guard").await?;
    let name = create_tool(&user, "tools-update-guard-create").await;
    let keys = user.keys.display().to_string();
    let hook = format!(
        "env -u THORCTL_EDIT_BEFORE_APPLY {} --keys {keys} --skip-update tools update {name} -d changed-elsewhere",
        env!("CARGO_BIN_EXE_thorctl")
    );
    let envs = [("THORCTL_EDIT_BEFORE_APPLY", hook.as_str())];
    // the guard refuses and the other change stands
    let args = ["tools", "update", &name, "--set", "description=mine"];
    let out = run_with(&user, "tools-update-guard-refused", &args, &envs).await;
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("changed remotely"),
        "{}",
        stderr(&out)
    );
    assert_eq!(
        describe(&user, &name).await["description"],
        "changed-elsewhere"
    );
    // --force applies anyway
    let args = [
        "tools",
        "update",
        &name,
        "--set",
        "description=forced",
        "--force",
    ];
    let hook = hook.replace("changed-elsewhere", "changed-again");
    let envs = [("THORCTL_EDIT_BEFORE_APPLY", hook.as_str())];
    let out = run_with(&user, "tools-update-guard-forced", &args, &envs).await;
    assert_success(&out);
    assert_eq!(describe(&user, &name).await["description"], "forced");
    Ok(())
}

/// `tools edit --root` opens the root document in an editor: a changed field applies, an
/// unchanged buffer or a read-only edit changes nothing, a broken one is recoverable, and an
/// empty one cancels
#[tokio::test]
async fn tools_edit_root_interactive() -> Result<(), Error> {
    // a tool with a description
    let user = TestUser::new("tools-edit-root").await?;
    let name = create_tool(&user, "tools-edit-root-create").await;
    let args = ["tools", "update", &name, "--set", "description=before"];
    assert_success(&user.run("tools-edit-root-describe", &args).await);
    // the editor sees a header and the document, and a changed field applies
    let script = EditorScript::replace("description: before", "description: after");
    let editor = script.editor();
    let args = ["tools", "edit", &name, "--root", "--editor", &editor];
    let out = run_with(&user, "tools-edit-root-change", &args, &EDIT_ENV).await;
    assert_success(&out);
    assert!(
        stdout(&out).contains("updated successfully"),
        "{}",
        stdout(&out)
    );
    assert_eq!(describe(&user, &name).await["description"], "after");
    let seen = script.seen(0).expect("the editor ran");
    assert!(seen.contains("# Editing tool"), "{seen}");
    assert!(
        seen.contains("'*id*':") && seen.contains("'*kind*': Agentic"),
        "{seen}"
    );
    // saving it unchanged is no change
    let script = EditorScript::noop();
    let editor = script.editor();
    let args = ["tools", "edit", &name, "--root", "--editor", &editor];
    let out = run_with(&user, "tools-edit-root-noop", &args, &EDIT_ENV).await;
    assert_success(&out);
    assert!(stdout(&out).contains("No changes"), "{}", stdout(&out));
    // an edit to a read-only field is reported and ignored
    let script = EditorScript::replace("'*kind*': Agentic", "'*kind*': Image");
    let editor = script.editor();
    let args = ["tools", "edit", &name, "--root", "--editor", &editor];
    let out = run_with(&user, "tools-edit-root-read-only", &args, &EDIT_ENV).await;
    assert_success(&out);
    assert!(stderr(&out).contains("read-only"), "{}", stderr(&out));
    assert!(stdout(&out).contains("No changes"), "{}", stdout(&out));
    // an empty buffer cancels
    let script = EditorScript::write("");
    let editor = script.editor();
    let args = ["tools", "edit", &name, "--root", "--editor", &editor];
    let out = run_with(&user, "tools-edit-root-cancel", &args, &EDIT_ENV).await;
    assert_success(&out);
    assert!(stdout(&out).contains("Cancelled"), "{}", stdout(&out));
    // a broken edit cancelled at the prompt exits 1 and leaves a recovery file with the text
    let broken = "name: [unclosed";
    let script = EditorScript::write(broken);
    let editor = script.editor();
    let envs = [EDIT_ENV[0], ("THORCTL_EDIT_PROMPT", "cancel")];
    let args = ["tools", "edit", &name, "--root", "--editor", &editor];
    let out = run_with(&user, "tools-edit-root-broken", &args, &envs).await;
    assert!(!out.status.success());
    let recovery = recovery_path(&stderr(&out)).expect("the error names a recovery file");
    assert!(std::fs::read_to_string(&recovery)?.contains(broken));
    std::fs::remove_file(recovery)?;
    // a document the API refuses is an error with its reason and a recovery file
    let missing = unique("nogroup");
    let script = EditorScript::write(&format!(
        "name: {name}\ndescription: after\ngroups: [{}, {missing}]\n",
        user.group
    ));
    let editor = script.editor();
    let args = ["tools", "edit", &name, "--root", "--editor", &editor];
    let out = run_with(&user, "tools-edit-root-refused", &args, &EDIT_ENV).await;
    assert!(!out.status.success());
    assert!(recovery_path(&stderr(&out)).is_some(), "{}", stderr(&out));
    assert_eq!(describe(&user, &name).await["description"], "after");
    // a dry run edit shows the changes and writes nothing
    let script = EditorScript::replace("description: after", "description: dry");
    let editor = script.editor();
    let args = [
        "tools",
        "edit",
        &name,
        "--root",
        "--editor",
        &editor,
        "--dry-run",
    ];
    let out = run_with(&user, "tools-edit-root-dry-run", &args, &EDIT_ENV).await;
    assert_success(&out);
    assert!(stdout(&out).contains("Dry run"), "{}", stdout(&out));
    assert_eq!(describe(&user, &name).await["description"], "after");
    // a flag source edits without opening an editor, so `edit` does what `update` does
    let script = EditorScript::fail(9);
    let editor = script.editor();
    let args = [
        "tools",
        "edit",
        &name,
        "--root",
        "--editor",
        &editor,
        "--set",
        "description=flag",
    ];
    assert_success(&user.run("tools-edit-root-flags", &args).await);
    assert_eq!(script.invocations(), 0);
    assert_eq!(describe(&user, &name).await["description"], "flag");
    // without a terminal an interactive edit points at the flags
    let args = ["tools", "edit", &name, "--root"];
    let out = user.run("tools-edit-root-no-terminal", &args).await;
    assert!(!out.status.success());
    assert!(stderr(&out).contains("--set"), "{}", stderr(&out));
    Ok(())
}

/// `tools edit --root` changes a harness's env and disabled models, and a remote change during
/// an editor session is refused
#[tokio::test]
async fn tools_edit_root_harness_and_guard() -> Result<(), Error> {
    // a harness
    let user = TestUser::new("tools-edit-root-harness").await?;
    let name = unique("harness");
    let spec = write_spec(&generators::gen_harness_spec());
    let args = [
        "tools",
        "create",
        "--spec",
        &spec,
        "-n",
        &name,
        "-g",
        &user.group,
        "-v",
        "1.0.0",
    ];
    assert_success(&user.run("tools-harness-root-create", &args).await);
    // the document shows the harness settings, and an edit exports a value and disables a model
    let script = EditorScript::write(&format!(
        "name: {name}\ngroups: [{}]\nharness_env:\n  export: [ModelId]\n  names: {{}}\ndisabled_models: [gpt-5]\n",
        user.group
    ));
    let editor = script.editor();
    let args = ["tools", "edit", &name, "--root", "--editor", &editor];
    let out = run_with(&user, "tools-harness-root-edit", &args, &EDIT_ENV).await;
    assert_success(&out);
    let seen = script.seen(0).expect("the editor ran");
    assert!(
        seen.contains("harness_env:") && seen.contains("disabled_models:"),
        "{seen}"
    );
    let tool = describe(&user, &name).await;
    assert_eq!(
        tool["metadata"]["Tool"]["harness_env"]["export"],
        json!(["ModelId"])
    );
    assert_eq!(
        tool["metadata"]["Tool"]["disabled_models"],
        json!(["gpt-5"])
    );
    // the harness settings are unset by --set
    let args = [
        "tools",
        "update",
        &name,
        "--set",
        "disabled_models=[]",
        "--set",
        "harness_env.export=[]",
    ];
    assert_success(&user.run("tools-harness-root-clear", &args).await);
    let tool = describe(&user, &name).await;
    assert!(
        tool["metadata"]["Tool"].get("disabled_models").is_none(),
        "{tool}"
    );
    // a change between the editor and the write is refused
    let keys = user.keys.display().to_string();
    let hook = format!(
        "env -u THORCTL_EDIT_BEFORE_APPLY {} --keys {keys} --skip-update tools update {name} -d elsewhere",
        env!("CARGO_BIN_EXE_thorctl")
    );
    let script = EditorScript::replace("description: null", "description: mine");
    let editor = script.editor();
    let envs = [EDIT_ENV[0], ("THORCTL_EDIT_BEFORE_APPLY", hook.as_str())];
    let args = ["tools", "edit", &name, "--root", "--editor", &editor];
    let out = run_with(&user, "tools-edit-root-guard", &args, &envs).await;
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("changed remotely"),
        "{}",
        stderr(&out)
    );
    Ok(())
}

/// Render a spec as the YAML document `tools edit` shows, with a change applied
///
/// # Arguments
///
/// * `spec` - The spec to render
/// * `change` - What to change in the spec's JSON document
fn spec_doc(spec: &ToolSpec, change: impl FnOnce(&mut Value)) -> String {
    // the kind is the document's one top-level key
    let mut value = serde_json::to_value(spec).expect("failed to convert the spec");
    change(&mut value);
    serde_norway::to_string(&value).expect("failed to write yaml")
}

/// Create a tool from a spec at a version, returning its name
///
/// # Arguments
///
/// * `user` - The user to run thorctl as
/// * `spec` - The first version's spec
/// * `version` - The first version's label
async fn create_with(user: &TestUser, spec: &ToolSpec, version: &str) -> String {
    // create the tool's first version
    let name = unique("tool");
    let path = write_spec(spec);
    let args = [
        "tools",
        "create",
        "--spec",
        &path,
        "-n",
        &name,
        "-g",
        &user.group,
        "-v",
        version,
    ];
    assert_success(&user.run("tools-create-with", &args).await);
    name
}

/// `tools edit` of a spec publishes a new version: the label is bumped, named, or chosen, an
/// unchanged spec publishes nothing, and the edited version never changes
#[tokio::test]
// one scenario walks every label choice against one tool
#[allow(clippy::too_many_lines)]
async fn tools_edit_spec_interactive() -> Result<(), Error> {
    // a tool at 1.0.0
    let user = TestUser::new("tools-edit-spec").await?;
    let spec = generators::gen_agentic_spec();
    let name = create_with(&user, &spec, "1.0.0").await;
    // an editor change publishes the next patch version, and the editor was shown a header
    let edited = spec_doc(&spec, |doc| doc["Agentic"]["timeout"] = json!(111));
    let script = EditorScript::write(&edited);
    let editor = script.editor();
    let args = ["tools", "edit", &name, "--editor", &editor];
    let out = run_with(&user, "tools-edit-spec-patch", &args, &EDIT_ENV).await;
    assert_success(&out);
    assert!(
        stdout(&out).contains("updated successfully"),
        "{}",
        stdout(&out)
    );
    assert!(stderr(&out).contains("version 1.0.1"), "{}", stderr(&out));
    assert_eq!(labels(&user, &name).await, vec!["1.0.1", "1.0.0"]);
    let seen = script.seen(0).expect("the editor ran");
    assert!(seen.contains("# Editing tool spec"), "{seen}");
    assert!(seen.contains("NEW version"), "{seen}");
    // the new version holds the edit and the old one is unchanged
    let out = user
        .run(
            "tools-edit-spec-new",
            &["tools", "spec", &name, "--format", "json"],
        )
        .await;
    assert_eq!(json(&out)["Agentic"]["timeout"], 111);
    let out = user
        .run(
            "tools-edit-spec-old",
            &[
                "tools",
                "spec",
                &format!("{name}@1.0.0"),
                "--format",
                "json",
            ],
        )
        .await;
    assert_eq!(
        json(&out)["Agentic"]["timeout"],
        json!(match &spec {
            ToolSpec::Agentic(agentic) => agentic.timeout,
            _ => unreachable!(),
        })
    );
    // --bump chooses the part
    let edited = spec_doc(&spec, |doc| doc["Agentic"]["timeout"] = json!(222));
    let script = EditorScript::write(&edited);
    let editor = script.editor();
    let args = [
        "tools", "edit", &name, "--editor", &editor, "--bump", "minor",
    ];
    assert_success(&run_with(&user, "tools-edit-spec-minor", &args, &EDIT_ENV).await);
    assert_eq!(labels(&user, &name).await[0], "1.1.0");
    // --version names the label
    let edited = spec_doc(&spec, |doc| doc["Agentic"]["timeout"] = json!(333));
    let script = EditorScript::write(&edited);
    let editor = script.editor();
    let args = [
        "tools",
        "edit",
        &name,
        "--editor",
        &editor,
        "--version",
        "2.0.0-rc.1",
    ];
    assert_success(&run_with(&user, "tools-edit-spec-named", &args, &EDIT_ENV).await);
    assert_eq!(labels(&user, &name).await[0], "2.0.0-rc.1");
    // an unchanged spec publishes nothing
    let script = EditorScript::noop();
    let editor = script.editor();
    let args = ["tools", "edit", &name, "--editor", &editor];
    let out = run_with(&user, "tools-edit-spec-noop", &args, &EDIT_ENV).await;
    assert_success(&out);
    assert!(stdout(&out).contains("No changes"), "{}", stdout(&out));
    assert_eq!(labels(&user, &name).await.len(), 4);
    // a dry run shows the change and publishes nothing
    let edited = spec_doc(&spec, |doc| doc["Agentic"]["timeout"] = json!(444));
    let script = EditorScript::write(&edited);
    let editor = script.editor();
    let args = ["tools", "edit", &name, "--editor", &editor, "--dry-run"];
    let out = run_with(&user, "tools-edit-spec-dry-run", &args, &EDIT_ENV).await;
    assert_success(&out);
    assert!(stdout(&out).contains("timeout"), "{}", stdout(&out));
    assert_eq!(labels(&user, &name).await.len(), 4);
    // a version label that is taken is refused with a recovery file
    let edited = spec_doc(&spec, |doc| doc["Agentic"]["timeout"] = json!(555));
    let script = EditorScript::write(&edited);
    let editor = script.editor();
    let envs = [EDIT_ENV[0], ("THORCTL_EDIT_PROMPT", "cancel")];
    let args = [
        "tools",
        "edit",
        &name,
        "--editor",
        &editor,
        "--version",
        "1.0.0",
    ];
    let out = run_with(&user, "tools-edit-spec-taken", &args, &EDIT_ENV).await;
    assert!(!out.status.success());
    assert!(stderr(&out).contains("already exists"), "{}", stderr(&out));
    assert!(recovery_path(&stderr(&out)).is_some(), "{}", stderr(&out));
    // an edit that makes a spec with an unknown key is refused before any request
    let script = EditorScript::write("Agentic:\n  colour: red\n");
    let editor = script.editor();
    let args = ["tools", "edit", &name, "--editor", &editor];
    let out = run_with(&user, "tools-edit-spec-unknown-key", &args, &envs).await;
    assert!(!out.status.success());
    let recovery = recovery_path(&stderr(&out)).expect("a recovery file");
    assert!(std::fs::read_to_string(&recovery)?.contains("colour"));
    std::fs::remove_file(recovery)?;
    // a spec of another kind is refused
    let image = spec_doc(&generators::gen_image_spec(), |_| {});
    let script = EditorScript::write(&image);
    let editor = script.editor();
    let args = ["tools", "edit", &name, "--editor", &editor];
    let out = run_with(&user, "tools-edit-spec-other-kind", &args, &envs).await;
    assert!(!out.status.success());
    assert!(stderr(&out).contains("Agentic"), "{}", stderr(&out));
    assert_eq!(labels(&user, &name).await.len(), 4);
    Ok(())
}

/// `tools edit` of a spec without an editor: `--set` publishes a new version, `--file` replaces
/// the spec, a custom label needs `--version`, and an old version can be edited
#[tokio::test]
// one scenario walks every flag source against one tool
#[allow(clippy::too_many_lines)]
async fn tools_edit_spec_non_interactive() -> Result<(), Error> {
    // a tool at 1.0.0
    let user = TestUser::new("tools-edit-spec-flags").await?;
    let spec = generators::gen_agentic_spec();
    let name = create_with(&user, &spec, "1.0.0").await;
    // --set changes one field and publishes the next patch version
    let args = ["tools", "edit", &name, "--set", "Agentic.timeout=120"];
    let out = user.run("tools-edit-spec-set", &args).await;
    assert_success(&out);
    assert_eq!(labels(&user, &name).await, vec!["1.0.1", "1.0.0"]);
    let out = user
        .run(
            "tools-edit-spec-set-read",
            &["tools", "spec", &name, "--format", "json"],
        )
        .await;
    assert_eq!(json(&out)["Agentic"]["timeout"], 120);
    // setting it to the same value publishes nothing
    let out = user.run("tools-edit-spec-same", &args).await;
    assert_success(&out);
    assert!(stdout(&out).contains("No changes"), "{}", stdout(&out));
    assert_eq!(labels(&user, &name).await.len(), 2);
    // --file replaces the spec, and --version names the label
    let path = write_file(
        "yaml",
        &spec_doc(&spec, |doc| doc["Agentic"]["timeout"] = json!(77)),
    );
    let args = [
        "tools",
        "edit",
        &name,
        "--file",
        &path,
        "--version",
        "1.5.0",
    ];
    assert_success(&user.run("tools-edit-spec-file", &args).await);
    assert_eq!(labels(&user, &name).await[0], "1.5.0");
    // an old version can be edited: the new version bumps the highest label
    let args = [
        "tools",
        "edit",
        &format!("{name}@1.0.0"),
        "--set",
        "Agentic.timeout=88",
        "--bump",
        "major",
    ];
    assert_success(&user.run("tools-edit-spec-old-version", &args).await);
    assert_eq!(labels(&user, &name).await[0], "2.0.0");
    // --unset and --patch address the spec too
    let args = [
        "tools",
        "edit",
        &name,
        "--unset",
        "Agentic.title",
        "-o",
        "json",
    ];
    let out = user.run("tools-edit-spec-unset", &args).await;
    assert_success(&out);
    assert_eq!(json(&out)["kind"], "tool spec");
    let merge = "{\"Agentic\":{\"model_description\":\"patched description\"}}";
    let args = ["tools", "edit", &name, "--patch", merge];
    assert_success(&user.run("tools-edit-spec-patch", &args).await);
    let out = user
        .run(
            "tools-edit-spec-patched",
            &["tools", "spec", &name, "--format", "json"],
        )
        .await;
    assert_eq!(
        json(&out)["Agentic"]["model_description"],
        "patched description"
    );
    // a tool whose latest label is custom needs --version without an editor
    let custom = create_with(&user, &spec, "dev").await;
    let args = ["tools", "edit", &custom, "--set", "Agentic.timeout=9"];
    let out = user.run("tools-edit-spec-custom", &args).await;
    assert!(!out.status.success());
    assert!(stderr(&out).contains("--version"), "{}", stderr(&out));
    assert_eq!(labels(&user, &custom).await, vec!["dev"]);
    let args = [
        "tools",
        "edit",
        &custom,
        "--set",
        "Agentic.timeout=9",
        "--version",
        "stable",
    ];
    assert_success(&user.run("tools-edit-spec-custom-named", &args).await);
    assert_eq!(labels(&user, &custom).await[0], "stable");
    // a bad path or a spec with an unknown key is refused before any request
    let args = ["tools", "edit", &name, "--set", "Agentic.colour=red"];
    assert!(
        !user
            .run("tools-edit-spec-bad-key", &args)
            .await
            .status
            .success()
    );
    assert_eq!(labels(&user, &name).await[0], "2.0.2");
    // the server's refusal of a spec is printed with its reason
    let args = [
        "tools",
        "edit",
        &name,
        "--set",
        "Agentic.function_name=bad name!",
    ];
    let out = user.run("tools-edit-spec-refused", &args).await;
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("400") || stderr(&out).contains("function"),
        "{}",
        stderr(&out)
    );
    Ok(())
}

/// An invalid spec edit reopens the editor when asked to, and a spec that changed between the
/// read and the write is refused unless forced
#[tokio::test]
async fn tools_edit_spec_reedit_and_remote_change() -> Result<(), Error> {
    // a tool at 1.0.0
    let user = TestUser::new("tools-edit-spec-guard").await?;
    let spec = generators::gen_agentic_spec();
    let name = create_with(&user, &spec, "1.0.0").await;
    // the first save is not a spec, the second is, and the editor reopens in between
    let good = spec_doc(&spec, |doc| doc["Agentic"]["timeout"] = json!(31));
    let script = EditorScript::sequence(vec![
        Step::write("Agentic:\n  colour: red\n"),
        Step::write(&good),
    ]);
    let editor = script.editor();
    let envs = [EDIT_ENV[0], ("THORCTL_EDIT_PROMPT", "reedit")];
    let args = ["tools", "edit", &name, "--editor", &editor];
    let out = run_with(&user, "tools-edit-spec-reedit", &args, &envs).await;
    assert_success(&out);
    assert_eq!(script.invocations(), 2);
    assert_eq!(labels(&user, &name).await, vec!["1.0.1", "1.0.0"]);
    // a new version published between the read and the write is refused, and the other stands
    let keys = user.keys.display().to_string();
    let hook = format!(
        "env -u THORCTL_EDIT_BEFORE_APPLY {} --keys {keys} --skip-update tools publish {name} --bump patch --from-latest --set Agentic.timeout=5",
        env!("CARGO_BIN_EXE_thorctl")
    );
    let envs = [("THORCTL_EDIT_BEFORE_APPLY", hook.as_str())];
    let args = ["tools", "edit", &name, "--set", "Agentic.timeout=7"];
    let out = run_with(&user, "tools-edit-spec-guard-refused", &args, &envs).await;
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("changed remotely"),
        "{}",
        stderr(&out)
    );
    assert_eq!(labels(&user, &name).await[0], "1.0.2");
    // --force publishes anyway, after the other change, under a label it names itself
    let hook = hook.replace("timeout=5", "timeout=6");
    let envs = [("THORCTL_EDIT_BEFORE_APPLY", hook.as_str())];
    let args = [
        "tools",
        "edit",
        &name,
        "--set",
        "Agentic.timeout=7",
        "--force",
        "--version",
        "9.0.0",
    ];
    let out = run_with(&user, "tools-edit-spec-guard-forced", &args, &envs).await;
    assert_success(&out);
    assert_eq!(
        labels(&user, &name).await,
        vec!["9.0.0", "1.0.3", "1.0.2", "1.0.1", "1.0.0"]
    );
    let out = user
        .run(
            "tools-edit-spec-guard-read",
            &["tools", "spec", &name, "--format", "json"],
        )
        .await;
    assert_eq!(json(&out)["Agentic"]["timeout"], 7);
    Ok(())
}

/// `tools edit` of a harness spec keeps the native tools the version provides
#[tokio::test]
async fn tools_edit_spec_harness_round_trip() -> Result<(), Error> {
    // a native tool and a harness that provides it
    let user = TestUser::new("tools-edit-spec-harness").await?;
    let (_, native) = generators::tool(
        &[&user.group],
        "1.0.0",
        &generators::gen_native_spec(),
        &user.client,
    )
    .await?;
    let spec = generators::gen_harness_spec();
    let name = unique("harness");
    let request = ToolEntityRequest::new("1.0.0", &spec)
        .expect("failed to encode the spec")
        .provides(vec![native.id]);
    let request = EntityRequest::new(&name, EntityMetadataRequest::Tool(request), [&user.group]);
    user.client.entities.create(request).await?;
    // editing the spec publishes a version that still provides the native tool
    let edited = spec_doc(&spec, |doc| {
        doc["Harness"]["interface"]["inputs"]["models"] = json!("/work/models.json");
    });
    let script = EditorScript::write(&edited);
    let editor = script.editor();
    let args = ["tools", "edit", &name, "--editor", &editor];
    assert_success(&run_with(&user, "tools-harness-edit", &args, &EDIT_ENV).await);
    assert_eq!(labels(&user, &name).await, vec!["1.0.1", "1.0.0"]);
    let tool = describe(&user, &name).await;
    let latest = tool["metadata"]["Tool"]["latest"]
        .as_str()
        .unwrap()
        .parse()?;
    let instance = user.client.entities.get(latest).await?;
    let EntityMetadata::ToolInstance(meta) = &instance.metadata else {
        panic!("expected a tool instance");
    };
    assert_eq!(meta.provides, vec![native.id]);
    let ToolSpec::Harness(harness) = &meta.spec else {
        panic!("expected a harness spec");
    };
    assert_eq!(harness.interface.inputs.models, "/work/models.json");
    Ok(())
}

/// `tools publish` takes a bump instead of a version, the latest spec with sets instead of a file,
/// a spec on stdin, and a dry run
#[tokio::test]
async fn tools_publish_extras() -> Result<(), Error> {
    // a tool at 1.0.0
    let user = TestUser::new("tools-publish-extras").await?;
    let spec = generators::gen_agentic_spec();
    let name = create_with(&user, &spec, "1.0.0").await;
    // a dry run says what would be published and publishes nothing
    let args = [
        "tools",
        "publish",
        &name,
        "--bump",
        "minor",
        "--from-latest",
        "--set",
        "Agentic.timeout=50",
        "--dry-run",
    ];
    let out = user.run("tools-publish-dry-run", &args).await;
    assert_success(&out);
    assert!(
        stdout(&out).contains("Would publish") && stdout(&out).contains("1.1.0"),
        "{}",
        stdout(&out)
    );
    assert_eq!(labels(&user, &name).await.len(), 1);
    // the latest spec with a set, bumped
    let args = [
        "tools",
        "publish",
        &name,
        "--bump",
        "minor",
        "--from-latest",
        "--set",
        "Agentic.timeout=50",
    ];
    let out = user.run("tools-publish-from-latest", &args).await;
    assert_success(&out);
    assert!(stdout(&out).contains("version 1.1.0"), "{}", stdout(&out));
    let out = user
        .run(
            "tools-publish-from-latest-read",
            &["tools", "spec", &name, "--format", "json"],
        )
        .await;
    assert_eq!(json(&out)["Agentic"]["timeout"], 50);
    // the latest spec unchanged is the API's no-op conflict
    let args = ["tools", "publish", &name, "-v", "1.2.0", "--from-latest"];
    let out = user.run("tools-publish-from-latest-noop", &args).await;
    assert!(!out.status.success());
    assert!(stderr(&out).contains("409"), "{}", stderr(&out));
    // a spec from stdin
    let doc = spec_doc(&spec, |doc| doc["Agentic"]["timeout"] = json!(60));
    let args = ["tools", "publish", &name, "--bump", "patch", "--spec", "-"];
    assert_success(&run_stdin(&user, &args, &doc).await);
    assert_eq!(labels(&user, &name).await[0], "1.1.1");
    // --set needs --from-latest
    let args = [
        "tools",
        "publish",
        &name,
        "-v",
        "9",
        "--spec",
        &write_spec(&spec),
        "--set",
        "Agentic.timeout=1",
    ];
    let out = user.run("tools-publish-set-alone", &args).await;
    assert!(!out.status.success());
    assert!(stderr(&out).contains("--from-latest"), "{}", stderr(&out));
    Ok(())
}
