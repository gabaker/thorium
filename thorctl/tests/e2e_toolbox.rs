//! End-to-end acceptance of the thorctl surface against the test API: the real toolbox through
//! convert, validate, import, diff, export and remove; tools, toolsets, agents, workflows and
//! pipelines built and edited with the CLI; and the three kinds of run (tool, pipeline, workflow)
//! behind the shared run flags
//!
//! Every test creates its own user and groups, so the scenarios stay independent. The scenario
//! that needs a real worker is ignored unless `THORIUM_AGENT_BIN` names a thorium-agent built from
//! this workspace.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

use common::editor::EditorScript;
use common::{TestUser, json, stdout, unique};
use serde_json::{Value, json};
use thorium::client::{ClientSettings, ResultsClient, Users};
use thorium::models::{
    EntityKinds, EntityListOpts, EntityMetadata, GroupRequest, GroupUsersRequest, ModelProvider,
    UserCreate, UserRole,
};
use thorium::test_utilities::{self, generators};
use thorium::{Error, Thorium};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

/// The repository's toolbox in the legacy format, kept from before its manifests became tools
const REAL_TOOLBOX: &str = "../tools/toolbox.v1.json";

/// The real toolbox this repository ships, built from its tool manifests
const REAL_TOOLBOX_V2: &str = "../tools/toolbox.json";

/// The environment that lets thorctl open an editor script without a terminal
const EDIT_ENV: [(&str, &str); 1] = [("THORCTL_EDIT_ASSUME_TTY", "1")];

/// Run thorctl as a user with environment variables and optional text on stdin, recording the run
///
/// # Arguments
///
/// * `user` - The user to run as
/// * `_name` - A label for the run, which names what it checks
/// * `args` - The thorctl arguments after the global flags
/// * `envs` - The environment variables to set
/// * `input` - The text to write to stdin, if any
async fn ctl(
    user: &TestUser,
    _name: &str,
    args: &[&str],
    envs: &[(&str, &str)],
    input: Option<&str>,
) -> Output {
    // start thorctl with a pipe for stdin when there is input
    let mut command = user.command(args);
    command.envs(envs.iter().copied());
    if input.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command.spawn().expect("failed to start thorctl");
    // write the input and close stdin so thorctl sees the end
    if let Some(input) = input {
        let mut stdin = child.stdin.take().expect("no stdin pipe");
        stdin
            .write_all(input.as_bytes())
            .await
            .expect("failed to write stdin");
        drop(stdin);
    }
    // wait for the run to finish, giving up if it hangs
    tokio::time::timeout(Duration::from_secs(300), child.wait_with_output())
        .await
        .expect("thorctl timed out")
        .expect("failed to run thorctl")
}

/// Run thorctl as a user and require an exit code, returning stdout
///
/// # Arguments
///
/// * `user` - The user to run as
/// * `name` - A label for the run, which names what it checks
/// * `args` - The thorctl arguments after the global flags
/// * `code` - The exit code the run must end with
async fn expect(user: &TestUser, name: &str, args: &[&str], code: i32) -> String {
    // run, then check the code with both streams in the failure message
    let out = ctl(user, name, args, &[], None).await;
    assert_code(&out, code, name);
    stdout(&out)
}

/// Assert that a command ended with an exit code, printing both its streams when it didn't
///
/// # Arguments
///
/// * `out` - The command's output
/// * `code` - The expected exit code
/// * `what` - What ran, for the failure message
fn assert_code(out: &Output, code: i32, what: &str) {
    assert_eq!(
        out.status.code(),
        Some(code),
        "{what}: wrong exit code\nstdout: {}\nstderr: {}",
        stdout(out),
        stderr(out)
    );
}

/// Get the text a command wrote to stderr
///
/// # Arguments
///
/// * `out` - The command's output
fn stderr(out: &Output) -> String {
    // lossy, since a failure can print anything
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Create an admin and a keys file for it
///
/// # Arguments
///
/// * `user` - A test user, whose API the admin is created on
async fn admin_keys(user: &TestUser) -> Result<PathBuf, Error> {
    // the real toolbox runs tools as root, which only an admin may publish
    let name = unique("ctladmin");
    let password = unique("pw");
    let create = UserCreate::new(&name, &password, "fake@fake.gov")
        .skip_verification()
        .role(UserRole::Admin);
    let secret_key = Some(&test_utilities::CONF.thorium.secret_key);
    Users::create(
        &user.admin.host,
        create,
        secret_key,
        &ClientSettings::default(),
    )
    .await?;
    let dir = std::env::temp_dir().join(unique("thorctl-e2e-admin-"));
    std::fs::create_dir_all(&dir)?;
    let keys = dir.join("keys.yml");
    std::fs::write(
        &keys,
        format!(
            "api: {}\nusername: {name}\npassword: {password}\n",
            user.admin.host
        ),
    )?;
    Ok(keys)
}

/// Run thorctl with an admin's keys
///
/// # Arguments
///
/// * `keys` - The admin's keys file
/// * `args` - The arguments after the global flags
async fn as_admin(keys: &Path, args: &[&str]) -> Output {
    // the same environment the test user's commands run in
    tokio::time::timeout(
        Duration::from_secs(600),
        Command::new(env!("CARGO_BIN_EXE_thorctl"))
            .arg("--keys")
            .arg(keys)
            .arg("--skip-update")
            .args(args)
            .env("NO_PROXY", "127.0.0.1,localhost,::1")
            .env("no_proxy", "127.0.0.1,localhost,::1")
            .stdin(Stdio::null())
            .output(),
    )
    .await
    .expect("thorctl timed out")
    .expect("thorctl ran")
}

/// Run thorctl as an admin and require an exit code, returning stdout
///
/// # Arguments
///
/// * `keys` - The admin's keys file
/// * `args` - The arguments after the global flags
/// * `code` - The exit code the run must end with
async fn admin_expect(keys: &Path, args: &[&str], code: i32) -> String {
    // run, then check the code with both streams in the failure message
    let out = as_admin(keys, args).await;
    assert_code(&out, code, &args.join(" "));
    stdout(&out)
}

/// Create a group a user owns
///
/// # Arguments
///
/// * `user` - The user who owns the group
/// * `prefix` - A prefix for the group's name
async fn group_for(user: &TestUser, prefix: &str) -> Result<String, Error> {
    // the admin creates it with the user as an owner
    let name = unique(prefix);
    let owners = GroupUsersRequest::default()
        .direct("thorium")
        .direct(&user.username);
    user.admin
        .groups
        .create(&GroupRequest::new(&name).owners(owners))
        .await?;
    Ok(name)
}

/// List the entities of a kind in a group
///
/// # Arguments
///
/// * `client` - A client that can see the group
/// * `group` - The group
/// * `kind` - The kind of entity
async fn entities_of(
    client: &Thorium,
    group: &str,
    kind: EntityKinds,
) -> Result<Vec<thorium::models::EntityListLine>, Error> {
    // read every page of the group's entities of this kind
    let opts = EntityListOpts::default()
        .kind(kind)
        .groups(vec![group.to_owned()])
        .limit(10_000);
    let mut cursor = client.entities.list(&opts).await?;
    let mut found = Vec::new();
    loop {
        found.append(&mut cursor.data);
        if cursor.exhausted() {
            return Ok(found);
        }
        cursor.refill().await?;
    }
}

/// Get the spec hash of the latest version of every tool in a group
///
/// # Arguments
///
/// * `client` - A client that can see the group
/// * `group` - The group
async fn latest_hashes(client: &Thorium, group: &str) -> Result<BTreeMap<String, String>, Error> {
    // read each tool's latest hash from its root
    let mut hashes = BTreeMap::new();
    for line in entities_of(client, group, EntityKinds::Tool).await? {
        let entity = client.entities.get(line.id).await?;
        if let EntityMetadata::Tool(meta) = &entity.metadata {
            hashes.insert(
                line.name,
                meta.latest_spec_hash.clone().expect("a latest hash"),
            );
        }
    }
    Ok(hashes)
}

/// Get the names of every pipeline in a group
///
/// # Arguments
///
/// * `client` - A client that can see the group
/// * `group` - The group
async fn pipeline_names(client: &Thorium, group: &str) -> Result<Vec<String>, Error> {
    // read every page of the group's pipelines
    let mut cursor = client.pipelines.list(group);
    let mut names = Vec::new();
    loop {
        cursor.next().await?;
        names.append(&mut cursor.names);
        if cursor.exhausted {
            names.sort();
            return Ok(names);
        }
    }
}

/// Write text to a unique file
///
/// # Arguments
///
/// * `name` - The file's name
/// * `text` - The file's content
fn write_file(name: &str, text: &str) -> PathBuf {
    // a directory of its own, so files never collide
    let dir = std::env::temp_dir().join(unique("thorctl-e2e-"));
    std::fs::create_dir_all(&dir).expect("a temp dir");
    let path = dir.join(name);
    std::fs::write(&path, text).expect("failed to write the file");
    path
}

/// Read a file as text
///
/// # Arguments
///
/// * `path` - The file to read
fn read(path: &Path) -> String {
    // the file always exists when a step produced it
    std::fs::read_to_string(path).expect("failed to read the file")
}

/// The real toolbox converts to the committed v2 file byte for byte, `--check` notices drift,
/// both forms validate, and a toolbox with a broken pipeline does not
#[tokio::test]
async fn e2e_convert_and_validate_the_real_toolbox() -> Result<(), Error> {
    // a user to run as, since thorctl is always run with a keys file here
    let user = TestUser::new("e2e-convert").await?;
    // the committed converted file is current, by `--check`
    let args = [
        "toolbox",
        "convert",
        REAL_TOOLBOX,
        "--check",
        "-o",
        REAL_TOOLBOX_V2,
    ];
    expect(&user, "convert-check-current", &args, 0).await;
    // converting into a new file writes exactly the committed bytes
    let out = write_file("toolbox.v2.json", "").with_file_name("converted.json");
    let out_arg = out.to_string_lossy().into_owned();
    let args = ["toolbox", "convert", REAL_TOOLBOX, "-o", &out_arg];
    expect(&user, "convert-write", &args, 0).await;
    assert_eq!(read(&out), read(Path::new(REAL_TOOLBOX_V2)));
    // converting to stdout writes the same document
    let args = ["toolbox", "convert", REAL_TOOLBOX, "-o", "-"];
    let printed = expect(&user, "convert-stdout", &args, 0).await;
    assert_eq!(printed, read(Path::new(REAL_TOOLBOX_V2)));
    // converting the converted file changes nothing
    let again = write_file("again.json", "");
    let again_arg = again.to_string_lossy().into_owned();
    let args = ["toolbox", "convert", REAL_TOOLBOX_V2, "-o", &again_arg];
    expect(&user, "convert-idempotent", &args, 0).await;
    assert_eq!(read(&again), read(Path::new(REAL_TOOLBOX_V2)));
    // a stale output file fails the check with a diff, and a missing one does too
    let stale = write_file(
        "stale.json",
        &read(Path::new(REAL_TOOLBOX_V2)).replace("\"name\"", "\"nom\""),
    );
    let stale_arg = stale.to_string_lossy().into_owned();
    let args = [
        "toolbox",
        "convert",
        REAL_TOOLBOX,
        "--check",
        "-o",
        &stale_arg,
    ];
    expect(&user, "convert-check-stale", &args, 1).await;
    let missing = stale.with_file_name("missing.json");
    let missing_arg = missing.to_string_lossy().into_owned();
    let args = [
        "toolbox",
        "convert",
        REAL_TOOLBOX,
        "--check",
        "-o",
        &missing_arg,
    ];
    expect(&user, "convert-check-missing", &args, 1).await;
    assert!(!missing.exists(), "--check wrote the output");
    // both forms validate, and the JSON report parses
    expect(
        &user,
        "validate-legacy",
        &["toolbox", "validate", REAL_TOOLBOX],
        0,
    )
    .await;
    expect(
        &user,
        "validate-v2",
        &["toolbox", "validate", REAL_TOOLBOX_V2],
        0,
    )
    .await;
    let args = ["toolbox", "validate", REAL_TOOLBOX_V2, "--format", "json"];
    let report: Value = serde_json::from_str(&expect(&user, "validate-json", &args, 0).await)
        .expect("validate printed JSON");
    assert!(report.is_object(), "{report}");
    // a manifest whose pipeline stage has no tool is an error
    let broken = write_file(
        "broken.json",
        &json!({
            "format_version": 2,
            "name": "broken",
            "pipelines": {"p": {"versions": {"latest": {
                "config": {"group": "g", "name": "p", "order": [["scan"]]}
            }}}}
        })
        .to_string(),
    );
    let broken_arg = broken.to_string_lossy().into_owned();
    expect(
        &user,
        "validate-broken",
        &["toolbox", "validate", &broken_arg],
        1,
    )
    .await;
    Ok(())
}

/// The real toolbox imports into a fresh group, re-imports as a no-op, matches `diff
/// --exit-code`, exports and imports into a second group with identical tools and pipelines, and
/// `remove` cleans each group up in dependency order
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn e2e_import_diff_export_round_trip_remove() -> Result<(), Error> {
    // two groups the user owns, and an admin
    let user = TestUser::new("e2e-toolbox").await?;
    let first = user.group.clone();
    let second = group_for(&user, "e2egroup2").await?;
    let empty = group_for(&user, "e2egroup3").await?;
    let keys = admin_keys(&user).await?;
    // a dry run plans the import and writes nothing
    let args = [
        "toolbox",
        "import",
        REAL_TOOLBOX_V2,
        "--group-override",
        &first,
        "--dry-run",
    ];
    admin_expect(&keys, &args, 0).await;
    let before = entities_of(&user.client, &first, EntityKinds::Tool).await?;
    assert!(before.is_empty(), "a dry run created tools: {before:?}");
    // the import applies, and says so in JSON
    let args = [
        "toolbox",
        "import",
        REAL_TOOLBOX_V2,
        "--group-override",
        &first,
        "-o",
        "json",
    ];
    let report: Value =
        serde_json::from_str(&admin_expect(&keys, &args, 0).await).expect("import printed JSON");
    assert_eq!(report["outcome"], "applied", "{report}");
    let imported = latest_hashes(&user.client, &first).await?;
    assert_eq!(imported.len(), 59);
    assert_eq!(pipeline_names(&user.client, &first).await?.len(), 32);
    // importing again is a no-op, from the v2 file and from the legacy file
    for manifest in [REAL_TOOLBOX_V2, REAL_TOOLBOX] {
        let args = [
            "toolbox",
            "import",
            manifest,
            "--group-override",
            &first,
            "-o",
            "json",
        ];
        let report: Value = serde_json::from_str(&admin_expect(&keys, &args, 0).await)
            .expect("import printed JSON");
        assert_eq!(report["outcome"], "nothing_to_do", "{report}");
    }
    assert_eq!(latest_hashes(&user.client, &first).await?, imported);
    // the diff is clean for both forms, and reports drift against a group that has nothing
    for manifest in [REAL_TOOLBOX_V2, REAL_TOOLBOX] {
        let args = [
            "toolbox",
            "diff",
            manifest,
            "--group-override",
            &first,
            "--exit-code",
        ];
        admin_expect(&keys, &args, 0).await;
    }
    let args = [
        "toolbox",
        "diff",
        REAL_TOOLBOX_V2,
        "--group-override",
        &empty,
        "--exit-code",
    ];
    admin_expect(&keys, &args, 1).await;
    let args = [
        "toolbox",
        "diff",
        REAL_TOOLBOX_V2,
        "--group-override",
        &empty,
    ];
    admin_expect(&keys, &args, 0).await;
    // the toolbox in a third group gives its network policies the same names as the first group's,
    // and the diff of each group still reads its own policies
    let third = group_for(&user, "e2egroup4").await?;
    let args = [
        "toolbox",
        "import",
        REAL_TOOLBOX_V2,
        "--group-override",
        &third,
    ];
    admin_expect(&keys, &args, 0).await;
    for group in [&first, &third] {
        let args = [
            "toolbox",
            "diff",
            REAL_TOOLBOX_V2,
            "--group-override",
            group,
            "--exit-code",
        ];
        admin_expect(&keys, &args, 0).await;
    }
    // the first group exports with everything, into the second group's name
    let exported = std::env::temp_dir().join(unique("thorctl-e2e-export-"));
    let exported_arg = exported.to_string_lossy().into_owned();
    let args = [
        "toolbox",
        "export",
        "-g",
        &first,
        "--all-kinds",
        "--group-override",
        &second,
        "-o",
        &exported_arg,
    ];
    admin_expect(&keys, &args, 0).await;
    let manifest = exported.join("toolbox.json");
    let manifest_arg = manifest.to_string_lossy().into_owned();
    // the export validates, and imports into the second group
    admin_expect(&keys, &["toolbox", "validate", &manifest_arg], 0).await;
    let args = ["toolbox", "import", &exported_arg, "-o", "json"];
    let report: Value =
        serde_json::from_str(&admin_expect(&keys, &args, 0).await).expect("import printed JSON");
    assert_eq!(report["outcome"], "applied", "{report}");
    // the round trip is equivalent: the same tools with the same spec hashes
    let copied = latest_hashes(&user.client, &second).await?;
    assert_eq!(copied, imported);
    assert_eq!(
        pipeline_names(&user.client, &second).await?,
        pipeline_names(&user.client, &first).await?
    );
    // the second group's own export equals the first export, so pipelines and their stage tools
    // are identical too
    let again = std::env::temp_dir().join(unique("thorctl-e2e-export-"));
    let again_arg = again.to_string_lossy().into_owned();
    let args = [
        "toolbox",
        "export",
        "-g",
        &second,
        "--all-kinds",
        "-o",
        &again_arg,
    ];
    admin_expect(&keys, &args, 0).await;
    let before: Value = serde_json::from_str(&read(&manifest)).expect("a manifest");
    let after: Value =
        serde_json::from_str(&read(&again.join("toolbox.json"))).expect("a manifest");
    assert_eq!(before["pipelines"], after["pipelines"]);
    assert_eq!(before["tools"], after["tools"]);
    assert_eq!(before, after);
    // the diff of the export against the second group is clean
    let args = ["toolbox", "diff", &exported_arg, "--exit-code"];
    admin_expect(&keys, &args, 0).await;
    // remove previews with --dry-run, which deletes nothing, then deletes the first group's items
    let args = [
        "toolbox",
        "remove",
        REAL_TOOLBOX_V2,
        "--group-override",
        &first,
        "--dry-run",
    ];
    admin_expect(&keys, &args, 0).await;
    assert_eq!(latest_hashes(&user.client, &first).await?.len(), 59);
    // without a terminal, remove asks for --skip-confirm
    let args = [
        "toolbox",
        "remove",
        REAL_TOOLBOX_V2,
        "--group-override",
        &first,
    ];
    admin_expect(&keys, &args, 1).await;
    let args = [
        "toolbox",
        "remove",
        REAL_TOOLBOX_V2,
        "--group-override",
        &first,
        "--skip-confirm",
    ];
    let text = admin_expect(&keys, &args, 0).await;
    assert!(!text.contains("still used"), "{text}");
    assert!(latest_hashes(&user.client, &first).await?.is_empty());
    let left = pipeline_names(&user.client, &first).await?;
    assert!(left.is_empty(), "pipelines were left behind: {left:?}");
    // the second group is untouched until its own remove, which also leaves nothing behind
    assert_eq!(latest_hashes(&user.client, &second).await?.len(), 59);
    let args = ["toolbox", "remove", &exported_arg, "--yes"];
    admin_expect(&keys, &args, 0).await;
    assert!(latest_hashes(&user.client, &second).await?.is_empty());
    let left = pipeline_names(&user.client, &second).await?;
    assert!(left.is_empty(), "pipelines were left behind: {left:?}");
    let args = [
        "toolbox",
        "remove",
        REAL_TOOLBOX_V2,
        "--group-override",
        &third,
        "--yes",
    ];
    admin_expect(&keys, &args, 0).await;
    // removing again has nothing to do
    let args = ["toolbox", "remove", &exported_arg, "--yes"];
    admin_expect(&keys, &args, 0).await;
    Ok(())
}

/// Create an agentic tool with `tools create`, returning its name
///
/// # Arguments
///
/// * `user` - The user to run as
/// * `name` - The tool's name
/// * `spec` - The spec of the first version
/// * `version` - The first version's label
async fn create_tool(user: &TestUser, name: &str, spec: &Value, version: &str) {
    // write the spec as a YAML file and create the tool from it
    let path = write_file("spec.yaml", &serde_norway::to_string(spec).expect("yaml"));
    let path = path.to_string_lossy().into_owned();
    let args = [
        "tools",
        "create",
        "--spec",
        &path,
        "-n",
        name,
        "-g",
        &user.group,
        "-v",
        version,
    ];
    expect(user, &format!("create-tool-{name}"), &args, 0).await;
}

/// Get the labels of a tool's versions, newest first
///
/// # Arguments
///
/// * `user` - The user to run as
/// * `name` - The tool's name
async fn labels(user: &TestUser, name: &str) -> Vec<String> {
    // list the versions as JSON
    let args = ["tools", "versions", name, "-o", "json"];
    let versions: Value =
        serde_json::from_str(&expect(user, "tool-versions", &args, 0).await).expect("JSON");
    versions
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

/// Get a version's spec as JSON through `tools spec`
///
/// # Arguments
///
/// * `user` - The user to run as
/// * `target` - The tool, or `tool@version`
async fn spec_json(user: &TestUser, target: &str) -> Value {
    // print the spec as one document with the kind as its key
    let args = ["tools", "spec", target, "--format", "json"];
    serde_json::from_str(&expect(user, "tool-spec", &args, 0).await).expect("JSON")
}

/// Tools are created and published with the CLI, edited in an editor and from flags into new
/// versions, root edits apply, and `tools spec` round trips
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn e2e_tools_create_publish_edit_and_spec() -> Result<(), Error> {
    // a tool created from an agentic spec at 1.0.0
    let user = TestUser::new("e2e-tools").await?;
    let base = serde_json::to_value(generators::gen_agentic_spec()).expect("a spec");
    let name = unique("e2etool");
    create_tool(&user, &name, &base, "1.0.0").await;
    assert_eq!(labels(&user, &name).await, vec!["1.0.0"]);
    // publish a second version from a file, and a third from the latest with one field changed
    let mut second = base.clone();
    second["Agentic"]["timeout"] = json!(61);
    let path = write_file(
        "second.yaml",
        &serde_norway::to_string(&second).expect("yaml"),
    );
    let path = path.to_string_lossy().into_owned();
    let args = ["tools", "publish", &name, "-v", "1.1.0", "--spec", &path];
    expect(&user, "publish-file", &args, 0).await;
    let args = [
        "tools",
        "publish",
        &name,
        "--bump",
        "minor",
        "--from-latest",
        "--set",
        "Agentic.timeout=62",
    ];
    expect(&user, "publish-from-latest", &args, 0).await;
    assert_eq!(labels(&user, &name).await, vec!["1.2.0", "1.1.0", "1.0.0"]);
    assert_eq!(spec_json(&user, &name).await["Agentic"]["timeout"], 62);
    assert_eq!(
        spec_json(&user, &format!("{name}@1.1.0")).await["Agentic"]["timeout"],
        61
    );
    // an interactive edit publishes the next patch version holding the editor's change
    let mut edited = base.clone();
    edited["Agentic"]["timeout"] = json!(111);
    let script = EditorScript::write(&serde_norway::to_string(&edited).expect("yaml"));
    let editor = script.editor();
    let args = ["tools", "edit", &name, "--editor", &editor];
    let out = ctl(&user, "tools-edit-interactive", &args, &EDIT_ENV, None).await;
    assert_code(&out, 0, "interactive tools edit");
    assert!(
        stdout(&out).contains("updated successfully"),
        "{}",
        stdout(&out)
    );
    assert_eq!(script.invocations(), 1);
    let shown = script.seen(0).expect("the editor was opened");
    assert!(shown.contains("# Editing tool spec"), "{shown}");
    assert_eq!(labels(&user, &name).await[0], "1.2.1");
    assert_eq!(spec_json(&user, &name).await["Agentic"]["timeout"], 111);
    // saving the editor's buffer unchanged publishes nothing
    let script = EditorScript::noop();
    let editor = script.editor();
    let args = ["tools", "edit", &name, "--editor", &editor];
    let out = ctl(&user, "tools-edit-noop", &args, &EDIT_ENV, None).await;
    assert_code(&out, 0, "unchanged tools edit");
    assert!(stdout(&out).contains("No changes"), "{}", stdout(&out));
    assert_eq!(labels(&user, &name).await.len(), 4);
    // the non-interactive forms never open an editor: --set, --patch and --file each publish
    let args = [
        "tools",
        "edit",
        &name,
        "--set",
        "Agentic.timeout=222",
        "--bump",
        "major",
    ];
    expect(&user, "tools-edit-set", &args, 0).await;
    assert_eq!(labels(&user, &name).await[0], "2.0.0");
    let args = [
        "tools",
        "edit",
        &name,
        "--patch",
        "{\"Agentic\":{\"timeout\":333}}",
        "--version",
        "2.0.1",
    ];
    expect(&user, "tools-edit-patch", &args, 0).await;
    assert_eq!(spec_json(&user, &name).await["Agentic"]["timeout"], 333);
    let mut from_file = base.clone();
    from_file["Agentic"]["timeout"] = json!(444);
    let path = write_file(
        "file.yaml",
        &serde_norway::to_string(&from_file).expect("yaml"),
    );
    let path = path.to_string_lossy().into_owned();
    let args = [
        "tools",
        "edit",
        &name,
        "--file",
        &path,
        "--version",
        "2.0.2",
    ];
    expect(&user, "tools-edit-file", &args, 0).await;
    assert_eq!(
        labels(&user, &name).await,
        vec![
            "2.0.2", "2.0.1", "2.0.0", "1.2.1", "1.2.0", "1.1.0", "1.0.0"
        ]
    );
    assert_eq!(spec_json(&user, &name).await["Agentic"]["timeout"], 444);
    // without a terminal an edit with no change flag points at the flags
    let out = ctl(
        &user,
        "tools-edit-no-tty",
        &["tools", "edit", &name],
        &[],
        None,
    )
    .await;
    assert_code(&out, 1, "tools edit without a terminal");
    assert!(stderr(&out).contains("--set"), "{}", stderr(&out));
    // the spec round trips: printed to a file, publishing it again is the API's conflict, and
    // with one change it is a new version that prints back identically
    let out_path = write_file("printed.yaml", "").with_file_name("printed.yaml");
    let out_arg = out_path.to_string_lossy().into_owned();
    expect(
        &user,
        "tools-spec-file",
        &["tools", "spec", &name, "-o", &out_arg],
        0,
    )
    .await;
    let args = ["tools", "publish", &name, "-v", "3.0.0", "--spec", &out_arg];
    let out = ctl(&user, "tools-spec-republish", &args, &[], None).await;
    assert_code(&out, 1, "republishing the printed spec");
    assert!(stderr(&out).contains("409"), "{}", stderr(&out));
    let printed: Value = serde_norway::from_str(&read(&out_path)).expect("the spec is YAML");
    assert_eq!(printed, spec_json(&user, &name).await);
    let mut changed = printed.clone();
    changed["Agentic"]["timeout"] = json!(555);
    std::fs::write(&out_path, serde_norway::to_string(&changed).expect("yaml"))?;
    let args = ["tools", "publish", &name, "-v", "3.0.0", "--spec", &out_arg];
    expect(&user, "tools-spec-publish-changed", &args, 0).await;
    assert_eq!(spec_json(&user, &name).await, changed);
    // the root: --set updates the description, and an interactive root edit changes it again
    let args = ["tools", "update", &name, "--set", "description=before"];
    expect(&user, "tools-update-set", &args, 0).await;
    let script = EditorScript::replace("description: before", "description: after");
    let editor = script.editor();
    let args = ["tools", "edit", &name, "--root", "--editor", &editor];
    let out = ctl(&user, "tools-edit-root", &args, &EDIT_ENV, None).await;
    assert_code(&out, 0, "interactive root edit");
    let args = ["tools", "describe", &name, "--format", "json"];
    let root: Value =
        serde_json::from_str(&expect(&user, "tools-describe", &args, 0).await).expect("JSON");
    assert_eq!(root["description"], "after");
    // a tool that is deleted version by version and then whole is gone
    let args = ["tools", "delete", &format!("{name}@3.0.0"), "--yes"];
    expect(&user, "tools-delete-version", &args, 0).await;
    assert_eq!(labels(&user, &name).await[0], "2.0.2");
    let args = ["tools", "delete", &name, "--all-versions", "--yes"];
    expect(&user, "tools-delete-all", &args, 0).await;
    expect(
        &user,
        "tools-describe-gone",
        &["tools", "describe", &name],
        1,
    )
    .await;
    Ok(())
}

/// A tool name that exists in two groups is ambiguous, and `GROUP/NAME` picks the one wanted for
/// every command that takes a tool
#[tokio::test]
async fn e2e_tool_names_in_two_groups_take_a_group_qualifier() -> Result<(), Error> {
    // the same tool name in two groups the user owns
    let user = TestUser::new("e2e-tool-groups").await?;
    let other = group_for(&user, "e2egroup2").await?;
    let base = serde_json::to_value(generators::gen_image_spec()).expect("a spec");
    let name = unique("twice");
    create_tool(&user, &name, &base, "1.0.0").await;
    let mut elsewhere = base.clone();
    elsewhere["Image"]["execution"]["timeout"] = json!(77);
    let path = write_file(
        "elsewhere.yaml",
        &serde_norway::to_string(&elsewhere).expect("yaml"),
    );
    let path = path.to_string_lossy().into_owned();
    let args = [
        "tools", "create", "--spec", &path, "-n", &name, "-g", &other, "-v", "1.0.0",
    ];
    expect(&user, "create-in-other-group", &args, 0).await;
    // the bare name is ambiguous
    let out = ctl(
        &user,
        "tools-spec-ambiguous",
        &["tools", "spec", &name],
        &[],
        None,
    )
    .await;
    assert_code(&out, 1, "an ambiguous tool name");
    assert!(stderr(&out).contains(&name), "{}", stderr(&out));
    // a group-qualified name picks the tool in that group, for spec, describe, versions and edit
    let mine = format!("{}/{name}", user.group);
    let theirs = format!("{other}/{name}");
    assert_eq!(spec_json(&user, &mine).await, base);
    assert_eq!(spec_json(&user, &theirs).await, elsewhere);
    let args = ["tools", "describe", &theirs, "--format", "json"];
    let described: Value =
        serde_json::from_str(&expect(&user, "tools-describe-qualified", &args, 0).await)
            .expect("JSON");
    assert_eq!(described["groups"], json!([other]));
    assert_eq!(labels(&user, &theirs).await, vec!["1.0.0"]);
    let args = [
        "tools",
        "edit",
        &theirs,
        "--set",
        "Image.execution.timeout=78",
        "--bump",
        "patch",
    ];
    expect(&user, "tools-edit-qualified", &args, 0).await;
    assert_eq!(labels(&user, &theirs).await, vec!["1.0.1", "1.0.0"]);
    assert_eq!(labels(&user, &mine).await, vec!["1.0.0"]);
    // a pipeline's stage tool is looked up in the pipeline's group, so the bare name works there
    let pipe = unique("pipe")[..20].to_owned();
    let stage = format!("scan={name}");
    let args = [
        "pipelines",
        "create",
        &pipe,
        "-g",
        &user.group,
        "--stage",
        "scan",
        "--stage-tool",
        &stage,
    ];
    expect(&user, "pipelines-create-bare-tool", &args, 0).await;
    let args = [
        "pipelines",
        "stage-tool",
        &pipe,
        "scan",
        &name,
        "-g",
        &user.group,
    ];
    expect(&user, "pipelines-stage-tool-bare", &args, 0).await;
    let pinned = format!("{name}@1.0.0");
    let args = [
        "pipelines",
        "stage-tool",
        &pipe,
        "scan",
        &pinned,
        "-g",
        &user.group,
    ];
    expect(&user, "pipelines-stage-tool-bare-pinned", &args, 0).await;
    // a group that has no such tool is a not found error that names it
    let args = ["tools", "spec", &format!("nosuchgroup/{name}")];
    let out = ctl(&user, "tools-spec-wrong-group", &args, &[], None).await;
    assert_code(&out, 1, "a tool in a group that has none");
    Ok(())
}

/// Print a resource's editable view with its `spec` command and parse it
///
/// # Arguments
///
/// * `user` - The user to run as
/// * `kind` - The command group (`toolsets`, `agents`, `workflows`, or `pipelines`)
/// * `name` - The resource's name
async fn view(user: &TestUser, kind: &str, name: &str) -> Value {
    // the view is a YAML document
    let text = expect(
        user,
        &format!("{kind}-spec-{name}"),
        &[kind, "spec", name],
        0,
    )
    .await;
    serde_norway::from_str(&text).expect("the spec is YAML")
}

/// Create a tool of any kind from a spec, returning the tool root's id
///
/// # Arguments
///
/// * `user` - The user to run as
/// * `name` - The tool's name
/// * `spec` - The tool's spec
async fn tool_from(user: &TestUser, name: &str, spec: &thorium::models::ToolSpec) -> uuid::Uuid {
    // create it through the CLI and read its id back from describe
    let value = serde_json::to_value(spec).expect("a spec");
    create_tool(user, name, &value, "1.0.0").await;
    let args = ["tools", "describe", name, "--format", "json"];
    let root: Value =
        serde_json::from_str(&expect(user, "tool-describe-id", &args, 0).await).expect("JSON");
    root["id"].as_str().expect("an id").parse().expect("a uuid")
}

/// Toolsets, agents, workflows and pipelines are built and changed with the CLI, and each step's
/// server state is read back
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn e2e_build_toolsets_agents_workflows_and_pipelines() -> Result<(), Error> {
    // a user and the tools everything is built from
    let user = TestUser::new("e2e-build").await?;
    let group = user.group.clone();
    let native_spec = generators::gen_agentic_spec();
    let native = unique("native");
    let native_id = tool_from(&user, &native, &native_spec).await;
    let harness = unique("harness");
    let harness_id = tool_from(
        &user,
        &harness,
        &test_utilities::runs::gen_model_harness_spec(),
    )
    .await;
    // a toolset of the native tool, changed with flags and with --set
    let args = [
        "toolsets", "create", "kit", "-g", &group, "--tool", &native, "-d", "a kit",
    ];
    expect(&user, "toolsets-create", &args, 0).await;
    let kit = view(&user, "toolsets", "kit").await;
    assert_eq!(kit["description"], "a kit");
    assert_eq!(kit["tools"][0]["tool"], json!(native));
    let args = ["toolsets", "update", "kit", "--auto-instance", "off"];
    expect(&user, "toolsets-update-flag", &args, 0).await;
    assert_eq!(view(&user, "toolsets", "kit").await["auto_instance"], false);
    let args = ["toolsets", "update", "kit", "--set", "description=the kit"];
    expect(&user, "toolsets-update-set", &args, 0).await;
    let script = EditorScript::replace("description: the kit", "description: edited kit");
    let editor = script.editor();
    let args = ["toolsets", "edit", "kit", "--editor", &editor];
    let out = ctl(&user, "toolsets-edit", &args, &EDIT_ENV, None).await;
    assert_code(&out, 0, "toolsets edit");
    assert_eq!(
        view(&user, "toolsets", "kit").await["description"],
        "edited kit"
    );
    // two agents: a helper, and a lead that has an edge of every kind
    let helper_config = serde_json::to_string(&generators::gen_agent_config("helper_fn"))?;
    let args = [
        "agents", "create", "-n", "helper", "-g", &group, "--config", "-",
    ];
    let out = ctl(
        &user,
        "agents-create-helper",
        &args,
        &[],
        Some(&helper_config),
    )
    .await;
    assert_code(&out, 0, "agents create helper");
    let lead_config = serde_json::to_string(&generators::gen_agent_config("lead_fn"))?;
    let args = [
        "agents",
        "create",
        "-n",
        "lead",
        "-g",
        &group,
        "--config",
        "-",
        "--tool",
        &native,
        "--toolset",
        "kit",
        "--sub-agent",
        "helper",
        "--harness",
        &harness,
        "--model-auth",
        "group",
        "--model-auth",
        "user",
    ];
    let out = ctl(&user, "agents-create-lead", &args, &[], Some(&lead_config)).await;
    assert_code(&out, 0, "agents create lead");
    let lead = view(&user, "agents", "lead").await;
    assert_eq!(lead["tools"][0]["tool"], json!(native), "{lead}");
    assert_eq!(lead["toolsets"][0]["toolset"], "kit", "{lead}");
    assert_eq!(lead["sub_agents"], json!(["helper"]), "{lead}");
    assert_eq!(lead["harness"]["tool"], json!(harness), "{lead}");
    let mut levels: Vec<String> = serde_json::from_value(lead["model_auth"].clone())?;
    levels.sort();
    assert_eq!(levels, ["group", "user"]);
    // the agent changes by flag, by --set, and in an editor
    let args = ["agents", "update", "lead", "--clear-model-auth"];
    expect(&user, "agents-update-clear-auth", &args, 0).await;
    assert!(view(&user, "agents", "lead").await["model_auth"].is_null());
    let args = ["agents", "update", "lead", "--model-auth", "harness"];
    expect(&user, "agents-update-auth", &args, 0).await;
    assert_eq!(
        view(&user, "agents", "lead").await["model_auth"],
        json!(["harness"])
    );
    let args = [
        "agents",
        "edit",
        "lead",
        "--set",
        "config.max_turns=7",
        "--yes",
    ];
    expect(&user, "agents-edit-set", &args, 0).await;
    assert_eq!(
        view(&user, "agents", "lead").await["config"]["max_turns"],
        7
    );
    let script = EditorScript::replace("max_turns: 7", "max_turns: 8");
    let editor = script.editor();
    let args = ["agents", "edit", "lead", "--editor", &editor];
    let out = ctl(&user, "agents-edit", &args, &EDIT_ENV, None).await;
    assert_code(&out, 0, "agents edit");
    assert_eq!(
        view(&user, "agents", "lead").await["config"]["max_turns"],
        8
    );
    // a workflow is created from a spec, then changed by flag, by --set and in an editor
    let spec = write_file(
        "workflow.yaml",
        &format!("name: flow\ngroups: [{group}]\nagents: [lead, helper]\nentry_agent: lead\n"),
    );
    let spec = spec.to_string_lossy().into_owned();
    expect(
        &user,
        "workflows-create",
        &["workflows", "create", "--spec", &spec],
        0,
    )
    .await;
    let flow = view(&user, "workflows", "flow").await;
    assert_eq!(flow["entry_agent"], "lead", "{flow}");
    assert_eq!(flow["agents"], json!(["helper", "lead"]), "{flow}");
    let args = [
        "workflows",
        "update",
        "flow",
        "--max-alive-sub-agents",
        "3",
        "--default-model",
        "claude-sonnet",
    ];
    expect(&user, "workflows-update-flags", &args, 0).await;
    let flow = view(&user, "workflows", "flow").await;
    assert_eq!(flow["limits"]["max_alive_sub_agents"], 3, "{flow}");
    assert_eq!(
        flow["default_models"][0]["model_id"], "claude-sonnet",
        "{flow}"
    );
    let args = [
        "workflows",
        "edit",
        "flow",
        "--set",
        "auto_instance=false",
        "--yes",
    ];
    expect(&user, "workflows-edit-set", &args, 0).await;
    assert_eq!(
        view(&user, "workflows", "flow").await["auto_instance"],
        false
    );
    let script = EditorScript::replace("max_alive_sub_agents: 3", "max_alive_sub_agents: 4");
    let editor = script.editor();
    let args = ["workflows", "edit", "flow", "--editor", &editor];
    let out = ctl(&user, "workflows-edit", &args, &EDIT_ENV, None).await;
    assert_code(&out, 0, "workflows edit");
    assert_eq!(
        view(&user, "workflows", "flow").await["limits"]["max_alive_sub_agents"],
        4
    );
    // no default model is required, since instances snapshot the workflow and no endpoint exists
    expect(
        &user,
        "workflows-update-clear-models",
        &["workflows", "update", "flow", "--clear-default-models"],
        0,
    )
    .await;
    // a keyless endpoint in the group serves the agents' models, and the lead may use group auth
    test_utilities::runs::keyless_endpoint(
        &[&group],
        &[test_utilities::runs::TEST_MODEL],
        &user.client,
    )
    .await?;
    let args = [
        "agents",
        "update",
        "lead",
        "--model-auth",
        "group",
        "--model-auth",
        "user",
    ];
    expect(&user, "agents-update-auth-group", &args, 0).await;
    // an instance is requested, then the event handler's part is played: the instance is stored
    let args = ["instances", "request", "workflow", "flow"];
    let text = expect(&user, "instances-request", &args, 0).await;
    assert!(text.contains("instance-revision=1"), "{text}");
    let workflow_id = entities_of(&user.client, &group, EntityKinds::WorkflowCatalog)
        .await?
        .into_iter()
        .find(|line| line.name == "flow")
        .expect("the workflow")
        .id;
    let workflow = user.client.entities.get(workflow_id).await?;
    let lead_id = entities_of(&user.client, &group, EntityKinds::Agent)
        .await?
        .into_iter()
        .find(|line| line.name == "lead")
        .expect("the lead agent")
        .id;
    let lead_entity = user.client.entities.get(lead_id).await?;
    let native_instance = user.client.entities.tool_instances(native_id).await?[0].id;
    let harness_instance = user.client.entities.tool_instances(harness_id).await?[0].id;
    let native_entity = user.client.entities.get(native_instance).await?;
    let harness_entity = user.client.entities.get(harness_instance).await?;
    let helper_id = entities_of(&user.client, &group, EntityKinds::Agent)
        .await?
        .into_iter()
        .find(|line| line.name == "helper")
        .expect("the helper agent")
        .id;
    let helper_entity = user.client.entities.get(helper_id).await?;
    let instance = generators::gen_workflow_instance(
        &workflow,
        1,
        None,
        &[
            (&lead_entity, &[&native_entity, &harness_entity]),
            (&helper_entity, &[&harness_entity]),
        ],
    );
    user.client
        .entities
        .create(generators::gen_workflow_instance_req(&instance))
        .await?;
    let sha256 = user
        .client
        .files
        .create(generators::gen_sample(&group))
        .await?
        .sha256;
    // the workflow runs by name, with its tags on the run
    let args = [
        "workflows",
        "run",
        "flow",
        "--sample",
        &sha256,
        "--tag",
        "e2e",
    ];
    let started = expect(&user, "workflows-run-by-name", &args, 0).await;
    let run = uuid::Uuid::parse_str(started.trim()).expect("a reaction id");
    let reaction = user.client.reactions.get(&group, run).await?;
    assert!(
        reaction.tags.contains(&"e2e".to_owned()),
        "{:?}",
        reaction.tags
    );
    // two image tools back a pipeline created with a stage tool for each stage
    let image = serde_json::to_value(generators::gen_image_spec())?;
    let (fetch, scan, report) = (unique("fetch"), unique("scan"), unique("report"));
    for name in [&fetch, &scan, &report] {
        create_tool(&user, name, &image, "1.0.0").await;
    }
    let mut changed = image.clone();
    changed["Image"]["execution"]["timeout"] = json!(99);
    create_tool(&user, &unique("unused"), &changed, "1.0.0").await;
    let pipe = unique("pipe")[..20].to_owned();
    let scan_pinned = format!("scan={scan}@1.0.0");
    let fetch_arg = format!("fetch={fetch}");
    let args = [
        "pipelines",
        "create",
        &pipe,
        "-g",
        &group,
        "--stage",
        "fetch",
        "--stage",
        "scan",
        "--stage-tool",
        &fetch_arg,
        "--stage-tool",
        &scan_pinned,
        "--sla",
        "100",
    ];
    expect(&user, "pipelines-create", &args, 0).await;
    let stored = user.client.pipelines.get(&group, &pipe).await?;
    assert_eq!(
        stored.order,
        vec![vec!["fetch".to_owned()], vec!["scan".to_owned()]]
    );
    assert_eq!(stored.sla, 100);
    assert_eq!(stored.stage_tools.len(), 2);
    // the pipeline changes by --set, in an editor, and with update flags that add a stage
    let args = [
        "pipelines",
        "edit",
        &pipe,
        "-g",
        &group,
        "--set",
        "sla=200",
        "--yes",
    ];
    expect(&user, "pipelines-edit-set", &args, 0).await;
    assert_eq!(user.client.pipelines.get(&group, &pipe).await?.sla, 200);
    let script = EditorScript::replace("sla: 200", "sla: 300");
    let editor = script.editor();
    let args = [
        "pipelines",
        "edit",
        &pipe,
        "-g",
        &group,
        "--editor",
        &editor,
    ];
    let out = ctl(&user, "pipelines-edit", &args, &EDIT_ENV, None).await;
    assert_code(&out, 0, "pipelines edit");
    assert_eq!(user.client.pipelines.get(&group, &pipe).await?.sla, 300);
    let report_arg = format!("report={report}");
    let args = [
        "pipelines",
        "update",
        &pipe,
        "-g",
        &group,
        "--order",
        "fetch;scan;report",
        "--stage-tool",
        &report_arg,
    ];
    expect(&user, "pipelines-update-add-stage", &args, 0).await;
    let stored = user.client.pipelines.get(&group, &pipe).await?;
    assert_eq!(stored.order.len(), 3);
    let report_tool = stored.stage_tools["report"];
    // retargeting a stage changes only that stage's tool
    let retarget = format!("scan={fetch}");
    let args = [
        "pipelines",
        "update",
        &pipe,
        "-g",
        &group,
        "--stage-tool",
        &retarget,
    ];
    expect(&user, "pipelines-update-stage-tool", &args, 0).await;
    let after = user.client.pipelines.get(&group, &pipe).await?;
    assert_eq!(after.stage_tools["scan"], after.stage_tools["fetch"]);
    assert_eq!(after.stage_tools["report"], report_tool);
    // stage-tool --remove drops the stage and its tool
    let args = [
        "pipelines",
        "stage-tool",
        &pipe,
        "report",
        "--remove",
        "-g",
        &group,
    ];
    expect(&user, "pipelines-stage-tool-remove", &args, 0).await;
    let after = user.client.pipelines.get(&group, &pipe).await?;
    assert_eq!(after.order.len(), 2);
    assert!(!after.stage_tools.contains_key("report"));
    // the pipeline runs, creating one reaction
    let args = ["pipelines", "run", &pipe, "-g", &group, "--sample", &sha256];
    let started = expect(&user, "pipelines-run", &args, 0).await;
    let reaction = user
        .client
        .reactions
        .get(
            &group,
            uuid::Uuid::parse_str(started.trim()).expect("a reaction id"),
        )
        .await?;
    assert_eq!(reaction.pipeline, pipe);
    // the toolset, the agents and the workflow are deleted in dependency order
    expect(
        &user,
        "workflows-delete",
        &["workflows", "delete", "flow", "--yes"],
        0,
    )
    .await;
    expect(
        &user,
        "agents-delete-lead",
        &["agents", "delete", "lead", "--yes"],
        0,
    )
    .await;
    expect(
        &user,
        "agents-delete-helper",
        &["agents", "delete", "helper", "--yes"],
        0,
    )
    .await;
    expect(
        &user,
        "toolsets-delete",
        &["toolsets", "delete", "kit", "--yes"],
        0,
    )
    .await;
    expect(
        &user,
        "workflows-describe-gone",
        &["workflows", "describe", "flow"],
        1,
    )
    .await;
    Ok(())
}

/// Model endpoints and profiles are created from spec files, edited in an editor and updated
/// from flags, and the edits are read back
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn e2e_models_endpoint_and_profile_edit_and_update() -> Result<(), Error> {
    // a keyless gateway the deployment allows, and an owner
    let user = TestUser::new("e2e-models").await?;
    let provider = ModelProvider::LiteLlm {
        base_url: "https://gw.local/v1".to_owned(),
        key_header: None,
    };
    generators::allow_model_endpoint(&provider).await?;
    // an endpoint created from a spec file
    let spec = write_file(
        "endpoint.yaml",
        "provider:\n  LiteLlm:\n    base_url: https://gw.local/v1\n    key_header: null\ncredential: null\nmodels:\n  - model_id: m1\n    context_window: 100\n  - model_id: m2\n    context_window: 100\n",
    );
    let spec = spec.to_string_lossy().into_owned();
    let endpoint = unique("endpoint");
    let args = [
        "models",
        "endpoints",
        "create",
        "--spec",
        &spec,
        "-n",
        &endpoint,
        "-g",
        &user.group,
        "-d",
        "before",
    ];
    let created = expect(&user, "models-endpoint-create", &args, 0).await;
    assert!(created.contains(&endpoint), "{created}");
    // the endpoint changes in an editor, then from --set
    let script = EditorScript::replace("description: before", "description: edited");
    let editor = script.editor();
    let args = [
        "models",
        "endpoints",
        "edit",
        &endpoint,
        "--editor",
        &editor,
    ];
    let out = ctl(&user, "models-endpoint-edit", &args, &EDIT_ENV, None).await;
    assert_code(&out, 0, "endpoints edit");
    let args = ["models", "endpoints", "describe", &endpoint, "-o", "json"];
    let described: Value =
        serde_json::from_str(&expect(&user, "models-endpoint-describe", &args, 0).await)?;
    assert_eq!(described["description"], "edited");
    let endpoint_id = described["id"].as_str().expect("an id").to_owned();
    let args = [
        "models",
        "endpoints",
        "update",
        &endpoint,
        "--set",
        "description=updated",
        "--set",
        "catalog[0].context_window=555",
    ];
    expect(&user, "models-endpoint-update", &args, 0).await;
    let described: Value = serde_json::from_str(
        &expect(
            &user,
            "models-endpoint-describe-2",
            &["models", "endpoints", "describe", &endpoint, "-o", "json"],
            0,
        )
        .await,
    )?;
    assert_eq!(described["description"], "updated");
    assert_eq!(
        described["metadata"]["ModelEndpoint"]["catalog"][0]["limits"]["context_window"],
        555
    );
    // a profile on the endpoint, edited in an editor and updated from --patch
    let spec = write_file(
        "profile.yaml",
        &format!(
            "endpoint: {endpoint_id}\nmodel_id: m1\napi: OpenAiChat\nparameters:\n  temperature: 0.2\n"
        ),
    );
    let spec = spec.to_string_lossy().into_owned();
    let profile = unique("profile");
    let args = [
        "models",
        "profiles",
        "create",
        "--spec",
        &spec,
        "-n",
        &profile,
        "-g",
        &user.group,
        "-d",
        "before",
    ];
    expect(&user, "models-profile-create", &args, 0).await;
    let script = EditorScript::replace("model_id: m1", "model_id: m2");
    let editor = script.editor();
    let args = ["models", "profiles", "edit", &profile, "--editor", &editor];
    let out = ctl(&user, "models-profile-edit", &args, &EDIT_ENV, None).await;
    assert_code(&out, 0, "profiles edit");
    let args = [
        "models",
        "profiles",
        "update",
        &profile,
        "--patch",
        "{\"description\": \"patched\", \"parameters\": {\"temperature\": 0.7}}",
    ];
    expect(&user, "models-profile-update", &args, 0).await;
    let args = ["models", "profiles", "describe", &profile, "-o", "json"];
    let described: Value =
        serde_json::from_str(&expect(&user, "models-profile-describe", &args, 0).await)?;
    assert_eq!(described["description"], "patched");
    assert_eq!(described["metadata"]["ModelProfile"]["model_id"], "m2");
    assert_eq!(
        described["metadata"]["ModelProfile"]["parameters"]["temperature"],
        0.7
    );
    Ok(())
}

/// The three structures a run can have
#[derive(Clone, Copy, Debug)]
enum Structure {
    /// A single tool run
    Tool,
    /// A pipeline run
    Pipeline,
    /// A workflow run
    Workflow,
}

impl Structure {
    /// The name the structure's runs are recorded under
    fn label(self) -> &'static str {
        match self {
            Structure::Tool => "tool",
            Structure::Pipeline => "pipeline",
            Structure::Workflow => "workflow",
        }
    }
}

/// What a structure's run command targets
struct Target {
    /// The structure
    structure: Structure,
    /// The group the runs are in
    group: String,
    /// The positional argument naming what to run
    name: String,
    /// The pipeline, when the structure is a pipeline
    pipeline: Option<thorium::models::Pipeline>,
}

impl Target {
    /// Build the thorctl arguments of a run of this target
    ///
    /// # Arguments
    ///
    /// * `extra` - The flags after the target
    fn args<'a>(&'a self, extra: &[&'a str]) -> Vec<&'a str> {
        // each structure's own command, then the shared flags
        let mut args = match self.structure {
            Structure::Tool => vec!["tools", "run", self.name.as_str()],
            Structure::Pipeline => vec!["pipelines", "run", self.name.as_str()],
            Structure::Workflow => vec!["workflows", "run", self.name.as_str()],
        };
        args.extend(["-g", self.group.as_str()]);
        args.extend_from_slice(extra);
        args
    }
}

/// Get the ids of the created runs of a group
///
/// # Arguments
///
/// * `user` - The user to read as
async fn created_runs(user: &TestUser) -> Result<Vec<uuid::Uuid>, Error> {
    // runs that were created and not yet started
    let cursor = user
        .client
        .reactions
        .list_group(&user.group, &thorium::models::ReactionStatus::Created)
        .exec()
        .await?;
    Ok(cursor
        .names
        .iter()
        .filter_map(|name| uuid::Uuid::parse_str(name).ok())
        .collect())
}

/// Wait for a created run that is not one of the known ones, failing early if the thorctl that
/// should have started it exits first
///
/// # Arguments
///
/// * `user` - The user to read as
/// * `known` - The runs already seen; the new one is added
/// * `starter` - The thorctl process that starts the run
async fn next_run(
    user: &TestUser,
    known: &mut Vec<uuid::Uuid>,
    starter: &mut tokio::process::Child,
) -> Result<uuid::Uuid, Error> {
    // poll the group's created runs
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        for run in created_runs(user).await? {
            if !known.contains(&run) {
                known.push(run);
                return Ok(run);
            }
        }
        // a thorctl that already exited created nothing, and says why
        if let Some(status) = starter.try_wait()? {
            let mut said = String::new();
            if let Some(mut stderr) = starter.stderr.take() {
                stderr.read_to_string(&mut said).await?;
            }
            if let Some(mut stdout) = starter.stdout.take() {
                stdout.read_to_string(&mut said).await?;
            }
            panic!("thorctl exited with {status} before a run was created: {said}");
        }
        assert!(tokio::time::Instant::now() < deadline, "no run was created");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Get the authorization header of a token client for the user
///
/// # Arguments
///
/// * `user` - The user to authenticate as
async fn auth(user: &TestUser) -> Result<String, Error> {
    // trade the user's password for a token
    let (token, _) = Thorium::auth(
        &user.admin.host,
        &user.username,
        &user.password,
        &reqwest::Client::new(),
    )
    .await?;
    Ok(test_utilities::auth_header(&token))
}

/// Complete a run the way its worker would: store a result with one file under the run's key and
/// proceed the run
///
/// # Arguments
///
/// * `user` - The user who owns the run
/// * `run` - The run's reaction
/// * `sha256` - The file the run ran on
async fn complete_with_result(user: &TestUser, run: uuid::Uuid, sha256: &str) -> Result<(), Error> {
    // the run stores its results under its key
    let reaction = user.client.reactions.get(&user.group, run).await?;
    let key = reaction.result_key().expect("a run has a result key");
    let dir = std::env::temp_dir().join(unique("e2e-result-"));
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("report.txt"), "harvested")?;
    let req = thorium::models::OutputRequest::new(
        sha256.to_owned(),
        key,
        "{\"functions\": 412}",
        thorium::models::OutputDisplayType::Json,
    )
    .file(thorium::models::OnDiskFile::new(dir.join("report.txt")).trim_prefix(&dir));
    user.client.files.create_result(req).await?;
    std::fs::remove_dir_all(&dir)?;
    // the run completes
    let auth = auth(user).await?;
    test_utilities::runs::handle_reaction(&user.client, &auth, &user.group, run, "proceed").await
}

/// Make a run end the way a worker would
///
/// # Arguments
///
/// * `user` - The user who owns the run
/// * `target` - What the run is of
/// * `run` - The run's reaction
/// * `sha256` - The file the run ran on
/// * `ending` - `proceed` to complete the run or `fail` to fail it
async fn finish(
    user: &TestUser,
    target: &Target,
    run: uuid::Uuid,
    sha256: &str,
    ending: &str,
) -> Result<(), Error> {
    // pipelines are finished by claiming and finishing each stage's job, everything else by its
    // result key
    match (target.structure, ending) {
        (Structure::Pipeline, "proceed") => {
            user.complete(target.pipeline.as_ref().expect("a pipeline"))
                .await
        }
        (_, "proceed") => complete_with_result(user, run, sha256).await,
        _ => {
            let auth = auth(user).await?;
            test_utilities::runs::handle_reaction(&user.client, &auth, &user.group, run, "fail")
                .await
        }
    }
}

/// Get the id of the run a run command started from what it printed
///
/// # Arguments
///
/// * `printed` - The command's stdout
fn started_id(printed: &str) -> uuid::Uuid {
    // a run prints its reaction id, or tools print one JSON object that holds it
    if let Ok(id) = uuid::Uuid::parse_str(printed.trim()) {
        return id;
    }
    let value: Value = serde_json::from_str(printed.trim()).expect("a run id or a JSON object");
    ["reaction", "reaction_id", "id"]
        .iter()
        .find_map(|key| value[key].as_str())
        .and_then(|id| uuid::Uuid::parse_str(id).ok())
        .expect("the JSON object names the run")
}

/// Assert that a finished run command reported how the run ended, in the form it prints: tool
/// runs print one JSON object unless `--logs` or `--output` ask for the full view, and pipeline
/// and workflow runs print text
///
/// # Arguments
///
/// * `out` - The command's output
/// * `ending` - `Completed`, `Failed`, or `timed out`
fn assert_says(out: &Output, ending: &str) {
    // a JSON object holds the status, or the error of a wait that gave up
    if let Ok(object) = serde_json::from_str::<Value>(stdout(out).trim()) {
        if ending == "timed out" {
            let error = object["error"].as_str().unwrap_or_default();
            assert!(error.contains("timed out"), "{object}");
        } else {
            assert_eq!(object["status"], ending, "{object}");
        }
        return;
    }
    // text says it on stderr for a timeout and on stdout otherwise
    if ending == "timed out" {
        assert!(stderr(out).contains("Timed out"), "{}", stderr(out));
    } else {
        assert!(stdout(out).contains(ending), "{}", stdout(out));
    }
}

/// Tool runs, pipeline runs and workflow runs take the same flags and behave the same way: tags,
/// SLA, parents where the route has one, `--wait` exit codes 0, 1 and 3, `--logs`, `--output`
/// and the JSON shape
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn e2e_shared_run_flags_across_structures() -> Result<(), Error> {
    // a user with a tool, a ready workflow, a pipeline and two files
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "e2e-run-flags",
        &generators::readable_name("pan", &suffix),
        &generators::readable_name("ceres", &suffix),
    )
    .await?;
    let groups = [user.group.as_str()];
    let (tool_root, _) =
        test_utilities::runs::image_tool(&groups, thorium::models::ImageScaler::K8s, &user.admin)
            .await?;
    let (_, image) =
        test_utilities::runs::image_tool(&groups, thorium::models::ImageScaler::K8s, &user.admin)
            .await?;
    let (_, boxed) = test_utilities::runs::container_tool(&groups, image.id, &user.admin).await?;
    let triage = Box::pin(test_utilities::runs::ready_workflow(
        "Triage",
        &groups,
        &[(&boxed, thorium::models::ToolMechanism::Cli)],
        &user.admin,
    ))
    .await?;
    let first = user
        .client
        .files
        .create(generators::gen_sample(&user.group))
        .await?
        .sha256;
    let second = user
        .client
        .files
        .create(generators::gen_sample(&user.group))
        .await?
        .sha256;
    let list = write_file("samples.txt", &format!("# files\n{first}\n\n{second}\n"));
    let list_arg = list.to_string_lossy().into_owned();
    let mut known: Vec<uuid::Uuid> = Vec::new();
    for structure in [Structure::Tool, Structure::Pipeline, Structure::Workflow] {
        let label = structure.label();
        // each structure starts from a target of its own
        let pipeline = if let Structure::Pipeline = structure {
            let req = common::cli_pipeline(&user.group, &user.client).await?;
            Some(user.client.pipelines.get(&user.group, &req.name).await?)
        } else {
            None
        };
        let name = match structure {
            Structure::Tool => tool_root.name.clone(),
            Structure::Pipeline => pipeline.as_ref().expect("a pipeline").name.clone(),
            Structure::Workflow => triage.id().to_string(),
        };
        let mut target = Target {
            structure,
            group: user.group.clone(),
            name,
            pipeline,
        };
        // tags, an SLA and a list of samples reach the run's reaction, and the run prints its id
        let args = target.args(&[
            "--sample-list",
            &list_arg,
            "--tag",
            "t1",
            "--tag",
            "t2",
            "--sla",
            "60",
        ]);
        let printed = expect(&user, &format!("{label}-run-flags"), &args, 0).await;
        let started = started_id(&printed);
        known.push(started);
        let reaction = user.client.reactions.get(&user.group, started).await?;
        assert!(
            reaction.tags.contains(&"t1".to_owned()),
            "{label}: {:?}",
            reaction.tags
        );
        assert!(
            reaction.tags.contains(&"t2".to_owned()),
            "{label}: {:?}",
            reaction.tags
        );
        let until = (reaction.sla - chrono::Utc::now()).num_seconds();
        assert!(
            (30..=61).contains(&until),
            "{label}: the SLA is {until}s away"
        );
        assert!(
            reaction.samples.contains(&first) && reaction.samples.contains(&second),
            "{label}: {:?}",
            reaction.samples
        );
        // a parent is accepted where the route has one, and refused where it has none
        let parent = started.to_string();
        let args = target.args(&["--sample", &first, "--parent", &parent]);
        let out = ctl(&user, &format!("{label}-run-parent"), &args, &[], None).await;
        match structure {
            Structure::Workflow => {
                assert_ne!(out.status.code(), Some(0), "workflow runs have no parent");
            }
            _ => {
                assert_code(&out, 0, &format!("{label} run --parent"));
                let child = started_id(&stdout(&out));
                known.push(child);
                let reaction = user.client.reactions.get(&user.group, child).await?;
                assert_eq!(reaction.parent, Some(started), "{label}");
            }
        }
        // a wait that outlasts its timeout exits 3
        let args = target.args(&[
            "--sample",
            &first,
            "--wait",
            "--timeout",
            "1s",
            "--interval",
            "200ms",
        ]);
        let out = ctl(
            &user,
            &format!("{label}-run-wait-timeout"),
            &args,
            &[],
            None,
        )
        .await;
        assert_code(&out, 3, &format!("{label} run --wait timeout"));
        assert_says(&out, "timed out");
        // the run the timed out wait started is still there, and is not one to finish later
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        loop {
            let fresh: Vec<uuid::Uuid> = created_runs(&user)
                .await?
                .into_iter()
                .filter(|run| !known.contains(run))
                .collect();
            if !fresh.is_empty() {
                known.extend(fresh);
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "{label}: the timed out wait left no run"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        // flags that cannot be combined are a usage error
        let args = target.args(&["--wait", "--watch"]);
        let out = ctl(&user, &format!("{label}-run-wait-watch"), &args, &[], None).await;
        assert_code(&out, 2, &format!("{label} run --wait --watch"));
        // a wait that ends with the run failed exits 1 naming the status
        if let Structure::Pipeline = structure {
            let req = common::cli_pipeline(&user.group, &user.client).await?;
            target.pipeline = Some(user.client.pipelines.get(&user.group, &req.name).await?);
            target.name = target.pipeline.as_ref().expect("a pipeline").name.clone();
        }
        let args = target.args(&[
            "--sample",
            &first,
            "--wait",
            "--timeout",
            "60s",
            "--interval",
            "200ms",
        ]);
        let mut waiting = user.spawn(&args);
        let run = next_run(&user, &mut known, &mut waiting).await?;
        finish(&user, &target, run, &first, "fail").await?;
        let out = tokio::time::timeout(Duration::from_secs(90), waiting.wait_with_output())
            .await
            .expect("run --wait didn't return")?;
        assert_code(&out, 1, &format!("{label} run --wait failed"));
        assert_says(&out, "Failed");
        // a wait that ends with the run completed prints its logs and its JSON, and exits 0
        if let Structure::Pipeline = structure {
            let req = common::cli_pipeline(&user.group, &user.client).await?;
            target.pipeline = Some(user.client.pipelines.get(&user.group, &req.name).await?);
            target.name = target.pipeline.as_ref().expect("a pipeline").name.clone();
        }
        let dir = std::env::temp_dir().join(unique("e2e-run-output-"));
        let dir_arg = dir.to_string_lossy().into_owned();
        let args = target.args(&[
            "--sample",
            &first,
            "--wait",
            "--logs",
            "--output",
            &dir_arg,
            "--timeout",
            "60s",
            "--interval",
            "200ms",
        ]);
        let mut waiting = user.spawn(&args);
        let run = next_run(&user, &mut known, &mut waiting).await?;
        finish(&user, &target, run, &first, "proceed").await?;
        let out = tokio::time::timeout(Duration::from_secs(90), waiting.wait_with_output())
            .await
            .expect("run --wait didn't return")?;
        assert_code(&out, 0, &format!("{label} run --wait completed"));
        assert_says(&out, "Completed");
        assert!(
            stdout(&out).contains("Stage '"),
            "{label}: no logs in {}",
            stdout(&out)
        );
        // results with files land under the output directory for runs that store results
        if let Structure::Pipeline = structure {
            let _ = std::fs::remove_dir_all(&dir);
        } else {
            let downloaded: Vec<_> = std::fs::read_dir(&dir)?.collect::<Result<_, _>>()?;
            assert_eq!(downloaded.len(), 1, "{label}: {downloaded:?}");
            assert_eq!(
                std::fs::read_to_string(downloaded[0].path().join("report.txt"))?,
                "harvested",
                "{label}"
            );
            std::fs::remove_dir_all(&dir)?;
        }
        // the JSON form of a finished run is one object with the run's id and status
        if let Structure::Pipeline = structure {
            let req = common::cli_pipeline(&user.group, &user.client).await?;
            target.pipeline = Some(user.client.pipelines.get(&user.group, &req.name).await?);
            target.name = target.pipeline.as_ref().expect("a pipeline").name.clone();
        }
        let args = target.args(&[
            "--sample",
            &first,
            "--wait",
            "--format",
            "json",
            "--timeout",
            "60s",
            "--interval",
            "200ms",
        ]);
        let mut waiting = user.spawn(&args);
        let run = next_run(&user, &mut known, &mut waiting).await?;
        finish(&user, &target, run, &first, "proceed").await?;
        let out = tokio::time::timeout(Duration::from_secs(90), waiting.wait_with_output())
            .await
            .expect("run --wait didn't return")?;
        assert_code(&out, 0, &format!("{label} run --wait --format json"));
        let shape = json(&out);
        assert!(shape.is_object(), "{label}: {shape}");
        assert_eq!(shape["status"], "Completed", "{label}: {shape}");
        assert_eq!(shape["id"], run.to_string(), "{label}: {shape}");
    }
    Ok(())
}

/// List every file under a directory
///
/// # Arguments
///
/// * `dir` - The directory to walk
fn walk_files(dir: &Path) -> Vec<PathBuf> {
    // a missing directory has no files
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(walk_files(&path));
        } else {
            files.push(path);
        }
    }
    files
}

/// The thorium-agent binary the real-worker scenarios run
fn agent_bin() -> Result<String, Error> {
    // the binary is built from this workspace by the caller
    std::env::var("THORIUM_AGENT_BIN")
        .map_err(|_| Error::new("THORIUM_AGENT_BIN must point at a thorium-agent binary"))
}

/// A real worker's inputs: the node it runs on and the keys it authenticates with
struct Worker {
    /// The worker's name
    name: String,
    /// The directory holding the worker's keys file
    dir: PathBuf,
}

/// Register a worker for a user and write the keys file the agent authenticates with
///
/// # Arguments
///
/// * `user` - The user the worker runs as
/// * `scaler` - The scaler the worker is under
/// * `instance` - The tool instance the worker runs
/// * `run` - The run the worker is dedicated to, if any
/// * `token` - The token the agent authenticates with
async fn register_worker(
    user: &TestUser,
    scaler: thorium::models::ImageScaler,
    instance: uuid::Uuid,
    run: Option<uuid::Uuid>,
    token: &str,
) -> Result<Worker, Error> {
    // the scaler would register the worker on a node
    let name = unique("w")[..24].to_owned();
    let list = thorium::models::WorkerRegistrationList::default().add(
        "cluster0",
        "node0",
        &name,
        &user.username,
        &user.group,
        instance,
        run,
        thorium::models::Resources::default(),
        thorium::models::Pools::Deadline,
    );
    user.admin.system.register_workers(scaler, &list).await?;
    // then start the agent with a keys file holding a token
    let dir = std::env::temp_dir().join(unique("e2e-agent-"));
    std::fs::create_dir_all(&dir)?;
    std::fs::write(
        dir.join("keys.yml"),
        format!("api: {}\ntoken: {token}\n", user.admin.host),
    )?;
    Ok(Worker { name, dir })
}

/// Run the agent as a worker until it has no more work, requiring a clean exit
///
/// # Arguments
///
/// * `user` - The user the worker runs as
/// * `worker` - The registered worker
/// * `instance` - The tool instance the worker runs
/// * `runtime` - The runtime arguments after the common ones
async fn run_agent(
    user: &TestUser,
    worker: &Worker,
    instance: uuid::Uuid,
    runtime: &[&str],
) -> Result<String, Error> {
    // the agent exits once it has been idle for its limbo
    let output = Command::new(agent_bin()?)
        .args([
            "--cluster",
            "cluster0",
            "--node",
            "node0",
            "--group",
            &user.group,
        ])
        .args([
            "--tool-instance",
            &instance.to_string(),
            "--name",
            &worker.name,
            "--limbo",
            "1",
        ])
        .arg("--keys")
        .arg(worker.dir.join("keys.yml"))
        .arg("--trace")
        .arg(worker.dir.join("none.yml"))
        .args(runtime)
        .output()
        .await?;
    let printed = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "the agent failed: {printed}");
    std::fs::remove_dir_all(&worker.dir)?;
    Ok(printed)
}

/// Get a user's raw API token
///
/// # Arguments
///
/// * `user` - The user to authenticate as
async fn token(user: &TestUser) -> Result<String, Error> {
    // trade the user's password for a token
    let (token, _) = Thorium::auth(
        &user.admin.host,
        &user.username,
        &user.password,
        &reqwest::Client::new(),
    )
    .await?;
    Ok(token)
}

/// Create a bare metal Image tool through the CLI that prints a line when it runs
///
/// # Arguments
///
/// * `user` - The user to run as
/// * `name` - The tool's name
/// * `line` - The line the tool prints
async fn bare_metal_tool(user: &TestUser, name: &str, line: &str) -> Result<uuid::Uuid, Error> {
    // a bare metal workload that runs a shell command
    let mut spec = thorium::models::WorkloadSpec {
        environment: thorium::models::WorkloadEnvironment::BareMetal(
            thorium::models::BareMetalEnvironment::default(),
        ),
        ..thorium::models::WorkloadSpec::default()
    };
    // the agent passes the job's own results path as an appended argument, and the tool writes a
    // result there
    let script = format!(
        "echo {line}; for arg in \"$@\"; do case \"$arg\" in */results) echo '{{\"hits\": 3}}' > \"$arg\";; esac; done"
    );
    spec.execution.args = thorium::models::ImageArgs {
        command: Some(vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            script,
            "sh".to_owned(),
        ]),
        output: thorium::models::ArgStrategy::Append,
        ..thorium::models::ImageArgs::default()
    };
    let root = tool_from(user, name, &thorium::models::ToolSpec::Image(spec)).await;
    let instances = user.client.entities.tool_instances(root).await?;
    Ok(instances[0].id)
}

/// A real worker runs a single tool run and a pipeline run, and the workflow harness runs a
/// workflow run, each behind `--wait --logs --output`
///
/// Set `THORIUM_AGENT_BIN` to a thorium-agent built from this workspace to run it.
#[tokio::test]
#[ignore = "needs THORIUM_AGENT_BIN, a thorium-agent binary built from this workspace"]
#[allow(clippy::too_many_lines)]
async fn e2e_real_worker_runs_all_three_structures() -> Result<(), Error> {
    // a user, a node, a file, and the real agent
    agent_bin()?;
    let suffix = generators::name_suffix();
    let user = TestUser::named(
        "e2e-real-worker",
        &generators::readable_name("amalthea", &suffix),
        &generators::readable_name("minerva", &suffix),
    )
    .await?;
    generators::node(
        "cluster0",
        "node0",
        thorium::models::Resources::default(),
        &user.admin,
    )
    .await?;
    let sha256 = user
        .client
        .files
        .create(generators::gen_sample(&user.group))
        .await?
        .sha256;
    let token = token(&user).await?;
    // a pipeline run: a bare metal tool backs the pipeline's one stage
    let tool = unique("harvester");
    let instance = bare_metal_tool(&user, &tool, "harvester ran").await?;
    let pipe = unique("pipe")[..20].to_owned();
    let stage_tool = format!("harvest={tool}");
    let args = [
        "pipelines",
        "create",
        &pipe,
        "-g",
        &user.group,
        "--stage",
        "harvest",
        "--stage-tool",
        &stage_tool,
    ];
    expect(&user, "real-pipelines-create", &args, 0).await;
    let dir = std::env::temp_dir().join(unique("e2e-real-output-"));
    let dir_arg = dir.to_string_lossy().into_owned();
    let args = [
        "pipelines",
        "run",
        &pipe,
        "-g",
        &user.group,
        "--sample",
        &sha256,
        "--tag",
        "real",
        "--wait",
        "--logs",
        "--output",
        &dir_arg,
        "--timeout",
        "120s",
        "--interval",
        "200ms",
    ];
    let mut waiting = user.spawn(&args);
    let mut known = Vec::new();
    let run = next_run(&user, &mut known, &mut waiting).await?;
    let worker = register_worker(
        &user,
        thorium::models::ImageScaler::BareMetal,
        instance,
        None,
        &token,
    )
    .await?;
    run_agent(&user, &worker, instance, &["bare-metal"]).await?;
    let out = tokio::time::timeout(Duration::from_secs(120), waiting.wait_with_output())
        .await
        .expect("pipelines run --wait didn't return")?;
    assert_code(&out, 0, "pipelines run --wait with a real worker");
    assert!(stdout(&out).contains("Completed"), "{}", stdout(&out));
    assert!(stdout(&out).contains("harvester ran"), "{}", stdout(&out));
    let reaction = user.client.reactions.get(&user.group, run).await?;
    assert_eq!(reaction.status, thorium::models::ReactionStatus::Completed);
    assert!(reaction.tags.contains(&"real".to_owned()));
    let downloaded = walk_files(&dir);
    assert!(
        downloaded
            .iter()
            .any(|path| read(path).contains("\"hits\": 3")),
        "no results were downloaded into {dir:?}: {downloaded:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
    // a tool run of the same tool, with the same worker and flags
    let tool_dir = std::env::temp_dir().join(unique("e2e-real-tool-output-"));
    let tool_dir_arg = tool_dir.to_string_lossy().into_owned();
    let args = [
        "tools",
        "run",
        &tool,
        "-g",
        &user.group,
        "--sample",
        &sha256,
        "--tag",
        "real",
        "--wait",
        "--logs",
        "--output",
        &tool_dir_arg,
        "--timeout",
        "120s",
        "--interval",
        "200ms",
    ];
    let mut waiting = user.spawn(&args);
    let run = next_run(&user, &mut known, &mut waiting).await?;
    // a tool run has a worker of its own, which authenticates with the run's token
    let run_token = user.admin.reactions.run_token(&user.group, run).await?;
    let worker = register_worker(
        &user,
        thorium::models::ImageScaler::BareMetal,
        instance,
        Some(run),
        &run_token.token,
    )
    .await?;
    run_agent(&user, &worker, instance, &["bare-metal"]).await?;
    let out = tokio::time::timeout(Duration::from_secs(120), waiting.wait_with_output())
        .await
        .expect("tools run --wait didn't return")?;
    assert_code(&out, 0, "tools run --wait with a real worker");
    assert!(stdout(&out).contains("harvester ran"), "{}", stdout(&out));
    assert!(
        walk_files(&tool_dir)
            .iter()
            .any(|path| read(path).contains("\"hits\": 3")),
        "no results were downloaded into {tool_dir:?}"
    );
    let _ = std::fs::remove_dir_all(&tool_dir);
    let reaction = user.client.reactions.get(&user.group, run).await?;
    assert_eq!(reaction.status, thorium::models::ReactionStatus::Completed);
    // a workflow run: the workflow's harness is the real worker, dedicated to the run
    let groups = [user.group.as_str()];
    let (_, image) =
        test_utilities::runs::image_tool(&groups, thorium::models::ImageScaler::K8s, &user.admin)
            .await?;
    let (_, boxed) = test_utilities::runs::container_tool(&groups, image.id, &user.admin).await?;
    let triage = Box::pin(test_utilities::runs::ready_workflow(
        "Triage",
        &groups,
        &[(&boxed, thorium::models::ToolMechanism::Cli)],
        &user.admin,
    ))
    .await?;
    let id = triage.id().to_string();
    let args = [
        "workflows",
        "run",
        &id,
        "-g",
        &user.group,
        "--sample",
        &sha256,
        "--tag",
        "real",
        "--wait",
        "--logs",
        "--timeout",
        "120s",
        "--interval",
        "200ms",
    ];
    let mut waiting = user.spawn(&args);
    let run = next_run(&user, &mut known, &mut waiting).await?;
    let run_token = user.admin.reactions.run_token(&user.group, run).await?;
    let worker = register_worker(
        &user,
        thorium::models::ImageScaler::K8s,
        triage.harness.id,
        Some(run),
        &run_token.token,
    )
    .await?;
    run_agent(
        &user,
        &worker,
        triage.harness.id,
        &[
            "k8s",
            "--entrypoint",
            r#"["/bin/sh"]"#,
            "--cmd",
            r#"["-c","echo harness done"]"#,
        ],
    )
    .await?;
    let out = tokio::time::timeout(Duration::from_secs(120), waiting.wait_with_output())
        .await
        .expect("workflows run --wait didn't return")?;
    assert_code(&out, 0, "workflows run --wait with a real harness");
    assert!(stdout(&out).contains("harness done"), "{}", stdout(&out));
    let reaction = user.client.reactions.get(&user.group, run).await?;
    assert_eq!(reaction.status, thorium::models::ReactionStatus::Completed);
    Ok(())
}

/// The committed end-to-end script for live clusters passes against the test API, so the script
/// and the test scenarios cannot drift apart
#[tokio::test]
async fn e2e_script_passes_against_the_test_api() -> Result<(), Error> {
    // an admin to run the script as
    let user = TestUser::new("e2e-script").await?;
    let keys = admin_keys(&user).await?;
    // run the script with this build of thorctl, giving up if it hangs
    let output = tokio::time::timeout(
        Duration::from_mins(15),
        Command::new("bash")
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/e2e.sh"))
            .env("THORCTL", env!("CARGO_BIN_EXE_thorctl"))
            .env("THORCTL_E2E_KEYS", &keys)
            .env("NO_PROXY", "127.0.0.1,localhost,::1")
            .env("no_proxy", "127.0.0.1,localhost,::1")
            .stdin(Stdio::null())
            .output(),
    )
    .await
    .expect("the script timed out")?;
    // the script prints each step, and its last line on success
    let printed = stdout(&output);
    assert!(
        output.status.success(),
        "the script failed with {:?}\nstdout: {printed}\nstderr: {}",
        output.status.code(),
        stderr(&output)
    );
    assert!(printed.contains("PASSED:"), "{printed}");
    Ok(())
}
