//! Tests `thorctl toolbox import` and `toolbox update` against the test API

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use common::{TestUser, json, stdout, unique};
use serde_json::{Value, json};
use thorium::client::{ClientSettings, Users};
use thorium::models::{EntityKinds, EntityListOpts, UserCreate, UserRole};
use thorium::test_utilities::{self, generators};
use thorium::{Error, Thorium};
use tokio::process::Command;

/// The repository's toolbox in the legacy format, kept from before its manifests became tools
const REAL_TOOLBOX: &str = "../tools/toolbox.v1.json";

/// The real toolbox this repository ships, built from its tool manifests
const REAL_TOOLBOX_V2: &str = "../tools/toolbox.json";

/// Write a manifest into a new temp directory and return its path
///
/// # Arguments
///
/// * `document` - The manifest as JSON
fn write_manifest(document: &Value) -> PathBuf {
    // a directory of its own, so manifests never share relative paths
    let dir = std::env::temp_dir().join(unique("thorctl-import-"));
    std::fs::create_dir_all(&dir).expect("a temp dir");
    let path = dir.join("toolbox.json");
    std::fs::write(&path, serde_json::to_vec_pretty(document).expect("json")).expect("the file");
    path
}

/// Build the JSON of an Image tool spec that runs a container image
///
/// # Arguments
///
/// * `image` - The container image
fn image_spec(image: &str) -> Value {
    // start from the API test generator, which makes a spec the API accepts
    let mut spec = serde_json::to_value(generators::gen_image_spec()).expect("a spec");
    spec["Image"]["environment"]["K8s"]["image"] = json!(image);
    spec
}

/// Build the JSON of an Image tool spec that depends on another tool's results
///
/// # Arguments
///
/// * `image` - The container image
/// * `tool` - The name of the tool whose results it needs
fn image_spec_depending_on(image: &str, tool: &str) -> Value {
    // fill in the dependency slot with a reference
    let mut spec = image_spec(image);
    spec["Image"]["data"]["dependencies"]["results"]["tools"] = json!([format!("ref:tool/{tool}")]);
    spec
}

/// Build a tool of a manifest
///
/// # Arguments
///
/// * `group` - The tool's group
/// * `versions` - The labels and specs of its versions
fn tool(group: &str, versions: &[(&str, Value)]) -> Value {
    // each version holds its spec
    let versions: serde_json::Map<String, Value> = versions
        .iter()
        .map(|(label, spec)| ((*label).to_owned(), json!({"spec": spec})))
        .collect();
    json!({"groups": [group], "versions": versions})
}

/// Run `thorctl toolbox` as a user, with extra global arguments
///
/// # Arguments
///
/// * `user` - The user to run as
/// * `transcript` - A label for the run, which names what it checks
/// * `args` - The arguments after `toolbox`
async fn toolbox(user: &TestUser, transcript: &str, args: &[&str]) -> Output {
    // the user's keys file is added by the helper
    let mut full = vec!["toolbox"];
    full.extend_from_slice(args);
    user.run(transcript, &full).await
}

/// Import a manifest as a user
///
/// # Arguments
///
/// * `user` - The user to run as
/// * `transcript` - A label for the run, which names what it checks
/// * `manifest` - The manifest's path
/// * `flags` - Flags after the manifest
async fn import(user: &TestUser, transcript: &str, manifest: &Path, flags: &[&str]) -> Output {
    // import with the manifest first
    let path = manifest.to_string_lossy().into_owned();
    let mut args = vec!["import", path.as_str()];
    args.extend_from_slice(flags);
    toolbox(user, transcript, &args).await
}

/// Assert that a command exited 0, printing both its streams when it didn't
///
/// # Arguments
///
/// * `output` - The command's output
fn ok(output: &Output) {
    // a refused import explains itself on stdout as well as stderr
    assert!(
        output.status.success(),
        "thorctl failed with {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        stdout(output),
        stderr(output)
    );
}

/// Get the stderr of a command
///
/// # Arguments
///
/// * `output` - The command's output
fn stderr(output: &Output) -> String {
    // lossy, since a failure can print anything
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Get the version labels of every tool in a group, oldest first
///
/// # Arguments
///
/// * `client` - A client that can see the group
/// * `group` - The group
async fn tool_labels(
    client: &Thorium,
    group: &str,
) -> Result<BTreeMap<String, Vec<String>>, Error> {
    // list the group's tools, then each one's versions
    let opts = EntityListOpts::default()
        .kind(EntityKinds::Tool)
        .groups(vec![group.to_owned()])
        .limit(10_000);
    let mut cursor = client.entities.list(&opts).await?;
    let mut found = Vec::new();
    loop {
        found.append(&mut cursor.data);
        if cursor.exhausted() {
            break;
        }
        cursor.refill().await?;
    }
    let mut tools = BTreeMap::new();
    for line in found {
        let mut labels: Vec<String> = client
            .entities
            .tool_instances(line.id)
            .await?
            .into_iter()
            .map(|instance| instance.version.to_string())
            .collect();
        labels.sort();
        tools.insert(line.name, labels);
    }
    Ok(tools)
}

/// Get the spec hash of the latest version of every tool in a group
///
/// # Arguments
///
/// * `client` - A client that can see the group
/// * `group` - The group
async fn latest_hashes(client: &Thorium, group: &str) -> Result<BTreeMap<String, String>, Error> {
    // read each tool's latest hash from its root
    let opts = EntityListOpts::default()
        .kind(EntityKinds::Tool)
        .groups(vec![group.to_owned()])
        .limit(10_000);
    let mut cursor = client.entities.list(&opts).await?;
    let mut found = Vec::new();
    loop {
        found.append(&mut cursor.data);
        if cursor.exhausted() {
            break;
        }
        cursor.refill().await?;
    }
    let mut hashes = BTreeMap::new();
    for line in found {
        let entity = client.entities.get(line.id).await?;
        if let thorium::models::EntityMetadata::Tool(meta) = &entity.metadata {
            hashes.insert(
                line.name,
                meta.latest_spec_hash.clone().expect("a latest hash"),
            );
        }
    }
    Ok(hashes)
}

/// A manifest of two Image tools (one depending on the other), an agentic tool, a toolset,
/// agents, a workflow, and a pipeline that runs the tools, all in one group
///
/// # Arguments
///
/// * `group` - The group everything goes in
/// * `prefix` - A prefix that keeps names unique
fn full_toolbox(group: &str, prefix: &str) -> Value {
    // names are prefixed so tests sharing one API never collide
    let base = format!("{prefix}base");
    let scan = format!("{prefix}scan");
    let native = format!("{prefix}native");
    let kit = format!("{prefix}kit");
    let lead = format!("{prefix}lead");
    let helper = format!("{prefix}helper");
    let flow = format!("{prefix}flow");
    let run = format!("{prefix}run");
    let mut agentic = serde_json::to_value(generators::gen_agentic_spec()).expect("a spec");
    // an agentic tool's function name has to be valid and unique per toolset
    agentic["Agentic"]["function_name"] = json!(format!("fn_{}", unique("")));
    let lead_config =
        serde_json::to_value(generators::gen_agent_config("lead_fn")).expect("config");
    let helper_config =
        serde_json::to_value(generators::gen_agent_config("helper_fn")).expect("config");
    json!({
        "format_version": 2,
        "name": "import test",
        "tools": {
            base.clone(): tool(group, &[("1.0.0", image_spec("registry.local/base:1")), ("1.1.0", image_spec("registry.local/base:2"))]),
            scan.clone(): tool(group, &[("1.0.0", image_spec_depending_on("registry.local/scan:1", &base))]),
            native.clone(): tool(group, &[("1.0.0", agentic)])
        },
        "toolsets": {kit.clone(): {"groups": [group], "tools": [{"tool": native.clone()}]}},
        "agents": {
            lead.clone(): {"groups": [group], "config": lead_config, "tools": [{"tool": native.clone()}],
                           "toolsets": [{"toolset": kit.clone()}], "sub_agents": [helper.clone()]},
            helper.clone(): {"groups": [group], "config": helper_config}
        },
        "workflows": {flow.clone(): {"groups": [group], "agents": [lead.clone(), helper.clone()], "entry_agent": lead.clone()}},
        "pipelines": {run.clone(): {"versions": {"latest": {
            "config": {"group": group, "name": run.clone(), "order": [[scan.clone()]]},
            "stage_tools": {scan.clone(): {"tool": scan.clone(), "version": "1.0.0"}}
        }}}}
    })
}

/// Importing a toolbox creates its tools, toolset, agents, workflow, and pipeline in dependency
/// order, and importing it again changes nothing
#[tokio::test]
// one scenario walks the whole import against shared fixtures
#[allow(clippy::too_many_lines)]
async fn import_creates_a_toolbox_and_reimports_it_as_a_no_op() -> Result<(), Error> {
    // a toolbox of every kind of item for a group the user owns
    let user = TestUser::new("toolbox-import").await?;
    // pipeline names are at most 25 characters
    let prefix = unique("")[..6].to_owned();
    let manifest = write_manifest(&full_toolbox(&user.group, &prefix));
    // the dry run shows the plan and writes nothing
    let dry = import(
        &user,
        "import-dry-run",
        &manifest,
        &["--dry-run", "-o", "json"],
    )
    .await;
    ok(&dry);
    let plan = json(&dry);
    assert_eq!(plan["errors"], json!([]));
    let items = plan["plan"]["items"].as_array().expect("items");
    assert_eq!(items.len(), 3 + 1 + 2 + 1 + 1);
    assert!(items.iter().all(|item| item["state"] == "create"));
    assert!(tool_labels(&user.client, &user.group).await?.is_empty());
    // the import creates everything
    let first = import(&user, "import-first", &manifest, &["-o", "json"]).await;
    ok(&first);
    let report = json(&first);
    assert_eq!(report["outcome"], "applied", "{report}");
    let created = report["results"]
        .as_array()
        .expect("results")
        .iter()
        .filter(|result| result["action"] == "created")
        .count();
    assert_eq!(created, 8);
    // both versions of the base tool were published in order
    let tools = tool_labels(&user.client, &user.group).await?;
    assert_eq!(tools[&format!("{prefix}base")], ["1.0.0", "1.1.0"]);
    assert_eq!(tools[&format!("{prefix}scan")], ["1.0.0"]);
    assert_eq!(tools.len(), 3);
    // the pipeline's stage runs the scan tool's 1.0.0 instance
    let pipeline = user
        .client
        .pipelines
        .get(&user.group, &format!("{prefix}run"))
        .await?;
    let scan = user
        .client
        .entities
        .list_details(
            &EntityListOpts::default()
                .kind(EntityKinds::Tool)
                .groups(vec![user.group.clone()])
                .limit(100),
        )
        .await?;
    let scan_id = scan
        .data
        .iter()
        .find(|tool| tool.name == format!("{prefix}scan"))
        .expect("the scan tool")
        .id;
    let instances = user.client.entities.tool_instances(scan_id).await?;
    assert_eq!(
        pipeline.stage_tools[&format!("{prefix}scan")],
        instances[0].id
    );
    // the toolset, agents, and workflow exist with their edges
    let agents = user
        .client
        .entities
        .list_details(
            &EntityListOpts::default()
                .kind(EntityKinds::Agent)
                .groups(vec![user.group.clone()])
                .limit(100),
        )
        .await?;
    assert_eq!(agents.data.len(), 2);
    let lead = user
        .client
        .entities
        .get(
            agents
                .data
                .iter()
                .find(|agent| agent.name == format!("{prefix}lead"))
                .expect("the lead agent")
                .id,
        )
        .await?;
    let thorium::models::EntityMetadata::Agent(meta) = &lead.metadata else {
        panic!("expected an agent");
    };
    assert_eq!(meta.sub_agents.len(), 1);
    assert_eq!(meta.tool_sets.len(), 1);
    assert_eq!(meta.latest_tools.len(), 1);
    // importing again changes nothing
    let again = import(&user, "import-again", &manifest, &[]).await;
    ok(&again);
    assert!(
        stdout(&again).contains("nothing to do"),
        "{}",
        stdout(&again)
    );
    assert_eq!(tool_labels(&user.client, &user.group).await?, tools);
    let again_json = import(&user, "import-again-json", &manifest, &["-o", "json"]).await;
    assert_eq!(json(&again_json)["outcome"], "nothing_to_do");
    Ok(())
}

/// Find an entity by kind and name in a group
///
/// # Arguments
///
/// * `client` - A client that can see the group
/// * `kind` - The kind of entity
/// * `group` - The group
/// * `name` - The entity's name
async fn find(
    client: &Thorium,
    kind: EntityKinds,
    group: &str,
    name: &str,
) -> Result<thorium::models::Entity, Error> {
    // list the group's entities of the kind and read the one with the name whole
    let opts = EntityListOpts::default()
        .kind(kind)
        .groups(vec![group.to_owned()])
        .limit(10_000);
    let mut cursor = client.entities.list(&opts).await?;
    loop {
        if let Some(line) = cursor.data.iter().find(|line| line.name == name) {
            return client.entities.get(line.id).await;
        }
        if cursor.exhausted() {
            return Err(Error::new(format!("no {kind} named {name}")));
        }
        cursor.refill().await?;
    }
}

/// Changing a toolset, an agent, a workflow, and a pipeline of an imported toolbox updates them
/// in place with --overwrite, and importing again changes nothing
#[tokio::test]
// one scenario walks every in-place kind against shared fixtures
#[allow(clippy::too_many_lines)]
async fn in_place_items_are_updated_to_match_the_toolbox() -> Result<(), Error> {
    // import a toolbox of every kind
    let user = TestUser::new("toolbox-in-place").await?;
    let prefix = unique("")[..6].to_owned();
    let original = full_toolbox(&user.group, &prefix);
    ok(&import(&user, "in-place-first", &write_manifest(&original), &[]).await);
    // change each in-place kind: another tool in the toolset, an agent setting, a workflow
    // setting, and the pipeline's SLA, and add the tool the toolset now names
    let mut changed = original.clone();
    let extra = format!("{prefix}extra");
    let mut agentic = serde_json::to_value(generators::gen_agentic_spec()).expect("a spec");
    agentic["Agentic"]["function_name"] = json!(format!("fn_{}", unique("")));
    changed["tools"][&extra] = tool(&user.group, &[("1.0.0", agentic)]);
    changed["toolsets"][format!("{prefix}kit")]["tools"]
        .as_array_mut()
        .expect("tools")
        .push(json!({"tool": extra}));
    changed["agents"][format!("{prefix}lead")]["config"]["max_turns"] = json!(9);
    changed["agents"][format!("{prefix}lead")]["description"] = json!("the lead");
    changed["workflows"][format!("{prefix}flow")]["auto_instance"] = json!(false);
    changed["pipelines"][format!("{prefix}run")]["versions"]["latest"]["config"]["sla"] =
        json!(123);
    let manifest = write_manifest(&changed);
    // the plan marks the four items for update and the new tool for creation
    let dry = import(
        &user,
        "in-place-dry",
        &manifest,
        &["--dry-run", "-o", "json"],
    )
    .await;
    ok(&dry);
    let plan = json(&dry);
    let states: BTreeMap<String, String> = plan["plan"]["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|item| {
            (
                item["item"]["name"].as_str().expect("name").to_owned(),
                item["state"].as_str().expect("state").to_owned(),
            )
        })
        .collect();
    for name in ["kit", "lead", "flow", "run"] {
        assert_eq!(states[&format!("{prefix}{name}")], "update", "{states:?}");
    }
    assert_eq!(states[&extra], "create");
    // without a flag the import is refused, listing what differs
    let refused = import(&user, "in-place-refused", &manifest, &[]).await;
    assert_eq!(refused.status.code(), Some(1));
    // --overwrite applies every difference
    let applied = import(
        &user,
        "in-place-overwrite",
        &manifest,
        &["--overwrite", "-o", "json"],
    )
    .await;
    ok(&applied);
    assert_eq!(json(&applied)["outcome"], "applied");
    let pipeline = user
        .client
        .pipelines
        .get(&user.group, &format!("{prefix}run"))
        .await?;
    assert_eq!(pipeline.sla, 123);
    let kit = find(
        &user.client,
        EntityKinds::ToolSet,
        &user.group,
        &format!("{prefix}kit"),
    )
    .await?;
    let thorium::models::EntityMetadata::ToolSet(meta) = &kit.metadata else {
        panic!("expected a toolset");
    };
    assert_eq!(meta.tools.len(), 2);
    let lead = find(
        &user.client,
        EntityKinds::Agent,
        &user.group,
        &format!("{prefix}lead"),
    )
    .await?;
    assert_eq!(lead.description.as_deref(), Some("the lead"));
    let thorium::models::EntityMetadata::Agent(agent) = &lead.metadata else {
        panic!("expected an agent");
    };
    assert_eq!(agent.config.max_turns, Some(9));
    assert_eq!(agent.sub_agents.len(), 1);
    let flow = find(
        &user.client,
        EntityKinds::WorkflowCatalog,
        &user.group,
        &format!("{prefix}flow"),
    )
    .await?;
    let thorium::models::EntityMetadata::WorkflowCatalog(workflow) = &flow.metadata else {
        panic!("expected a workflow");
    };
    assert!(!workflow.auto_instance);
    // importing again changes nothing
    let again = import(&user, "in-place-again", &manifest, &["-o", "json"]).await;
    ok(&again);
    assert_eq!(
        json(&again)["outcome"],
        "nothing_to_do",
        "{}",
        stdout(&again)
    );
    Ok(())
}

/// A changed spec is published as a new version, a label taken by another spec is published as
/// LABEL+HASH with --overwrite, --skip-conflicts leaves it alone, and no flag fails listing it
#[tokio::test]
async fn changed_specs_publish_new_versions_and_label_conflicts() -> Result<(), Error> {
    // a tool with one version
    let user = TestUser::new("toolbox-conflicts").await?;
    let name = unique("tool");
    let manifest = |versions: &[(&str, Value)]| {
        write_manifest(&json!({
            "format_version": 2, "name": "t",
            "tools": {name.clone(): tool(&user.group, versions)}
        }))
    };
    let v1 = manifest(&[("1.0.0", image_spec("registry.local/a:1"))]);
    ok(&import(&user, "conflicts-first", &v1, &[]).await);
    // a new label with a new spec is published alone with --overwrite
    let v2 = manifest(&[
        ("1.0.0", image_spec("registry.local/a:1")),
        ("1.1.0", image_spec("registry.local/a:2")),
    ]);
    let plain = import(&user, "conflicts-refused", &v2, &[]).await;
    assert!(
        !plain.status.success(),
        "an import with differences and no flag succeeded"
    );
    assert!(stderr(&plain).contains("--overwrite") || stdout(&plain).contains("--overwrite"));
    assert_eq!(
        tool_labels(&user.client, &user.group).await?[&name],
        ["1.0.0"]
    );
    let skipped = import(&user, "conflicts-skip", &v2, &["--skip-conflicts"]).await;
    ok(&skipped);
    assert_eq!(
        tool_labels(&user.client, &user.group).await?[&name],
        ["1.0.0"]
    );
    ok(&import(&user, "conflicts-overwrite", &v2, &["--overwrite"]).await);
    assert_eq!(
        tool_labels(&user.client, &user.group).await?[&name],
        ["1.0.0", "1.1.0"]
    );
    // the label 1.0.0 with another spec is published as 1.0.0+HASH
    let conflict = manifest(&[
        ("1.0.0", image_spec("registry.local/a:9")),
        ("1.1.0", image_spec("registry.local/a:2")),
    ]);
    ok(&import(&user, "conflicts-label", &conflict, &["--overwrite"]).await);
    let labels = tool_labels(&user.client, &user.group).await?;
    assert_eq!(labels[&name].len(), 3, "{labels:?}");
    assert!(
        labels[&name]
            .iter()
            .any(|label| label.starts_with("1.0.0+")),
        "{labels:?}"
    );
    // importing the conflicting toolbox again is a no-op
    let again = import(&user, "conflicts-again", &conflict, &["--overwrite"]).await;
    assert!(
        stdout(&again).contains("nothing to do"),
        "{}",
        stdout(&again)
    );
    Ok(())
}

/// `toolbox update` publishes new versions of tools that exist and creates nothing
#[tokio::test]
async fn update_only_touches_what_exists() -> Result<(), Error> {
    // one tool imported, then a toolbox that grows it and adds another
    let user = TestUser::new("toolbox-update").await?;
    let (old, new) = (unique("old"), unique("new"));
    let first = write_manifest(&json!({
        "format_version": 2, "name": "t",
        "tools": {old.clone(): tool(&user.group, &[("1.0.0", image_spec("registry.local/o:1"))])}
    }));
    ok(&import(&user, "update-first", &first, &[]).await);
    let second = write_manifest(&json!({
        "format_version": 2, "name": "t",
        "tools": {
            old.clone(): tool(&user.group, &[
                ("1.0.0", image_spec("registry.local/o:1")),
                ("2.0.0", image_spec("registry.local/o:2"))
            ]),
            new.clone(): tool(&user.group, &[("1.0.0", image_spec("registry.local/n:1"))])
        }
    }));
    let path = second.to_string_lossy().into_owned();
    // a dry run changes nothing
    let dry = toolbox(
        &user,
        "update-dry",
        &["update", &path, "--dry-run", "-o", "json"],
    )
    .await;
    ok(&dry);
    assert_eq!(
        tool_labels(&user.client, &user.group).await?[&old],
        ["1.0.0"]
    );
    // the update grows the old tool and doesn't create the new one
    let update = toolbox(
        &user,
        "update",
        &["update", &path, "--overwrite", "-o", "json"],
    )
    .await;
    ok(&update);
    let report = json(&update);
    assert_eq!(report["outcome"], "applied", "{report}");
    let tools = tool_labels(&user.client, &user.group).await?;
    assert_eq!(tools[&old], ["1.0.0", "2.0.0"]);
    assert!(!tools.contains_key(&new), "{tools:?}");
    let not_imported: Vec<&Value> = report["results"]
        .as_array()
        .expect("results")
        .iter()
        .filter(|result| result["action"] == "not_imported")
        .collect();
    assert_eq!(not_imported.len(), 1);
    // an import creates the new tool
    ok(&import(&user, "update-import", &second, &[]).await);
    assert!(
        tool_labels(&user.client, &user.group)
            .await?
            .contains_key(&new)
    );
    Ok(())
}

/// `-t` imports a tool and what it depends on, and nothing else
#[tokio::test]
async fn selecting_a_tool_imports_its_dependencies() -> Result<(), Error> {
    // a chain of two tools and an unrelated tool
    let user = TestUser::new("toolbox-select").await?;
    let prefix = unique("sel");
    let (base, scan, other) = (
        format!("{prefix}base"),
        format!("{prefix}scan"),
        format!("{prefix}other"),
    );
    let manifest = write_manifest(&json!({
        "format_version": 2, "name": "t",
        "tools": {
            base.clone(): tool(&user.group, &[("1", image_spec("registry.local/b:1"))]),
            scan.clone(): tool(&user.group, &[("1", image_spec_depending_on("registry.local/s:1", &base))]),
            other.clone(): tool(&user.group, &[("1", image_spec("registry.local/o:1"))])
        }
    }));
    ok(&import(&user, "select", &manifest, &["-t", &scan]).await);
    let tools = tool_labels(&user.client, &user.group).await?;
    assert_eq!(tools.keys().cloned().collect::<Vec<_>>(), [base, scan]);
    Ok(())
}

/// A failure partway through stops the import, keeps what was applied, and `--rollback-on-failure`
/// undoes it
#[tokio::test]
async fn a_failed_import_stops_and_can_roll_back() -> Result<(), Error> {
    // the last tool is agentic with a function name no model can call, which only the API refuses
    let user = TestUser::new("toolbox-rollback").await?;
    let prefix = unique("rb");
    let mut bad = serde_json::to_value(generators::gen_agentic_spec()).expect("a spec");
    bad["Agentic"]["function_name"] = json!("not a valid name");
    let manifest = write_manifest(&json!({
        "format_version": 2, "name": "t",
        "tools": {
            format!("{prefix}a"): tool(&user.group, &[("1", image_spec("registry.local/a:1"))]),
            format!("{prefix}b"): tool(&user.group, &[("1", image_spec("registry.local/b:1"))]),
            format!("{prefix}c"): tool(&user.group, &[("1", bad)])
        }
    }));
    let path = manifest.to_string_lossy().into_owned();
    // with one worker the tools are written in order, so a and b land before c fails
    let kept = user
        .run(
            "rollback-kept",
            &["--workers", "1", "toolbox", "import", &path, "-o", "json"],
        )
        .await;
    assert_eq!(kept.status.code(), Some(1), "{}", stderr(&kept));
    let report = json(&kept);
    assert_eq!(report["outcome"], "failed", "{report}");
    assert!(
        report["errors"][0]
            .as_str()
            .expect("an error")
            .contains(&format!("{prefix}c"))
    );
    let tools = tool_labels(&user.client, &user.group).await?;
    assert_eq!(
        tools.keys().cloned().collect::<Vec<_>>(),
        [format!("{prefix}a"), format!("{prefix}b")]
    );
    // delete what was kept, then fail again with --rollback-on-failure
    for name in tools.keys() {
        let mut found = user
            .client
            .entities
            .list_details(
                &EntityListOpts::default()
                    .kind(EntityKinds::Tool)
                    .groups(vec![user.group.clone()])
                    .limit(100),
            )
            .await?;
        let tool = found
            .data
            .drain(..)
            .find(|tool| tool.name == *name)
            .expect("a tool");
        for instance in user.client.entities.tool_instances(tool.id).await? {
            user.client.entities.delete(instance.id).await?;
        }
        user.client.entities.delete(tool.id).await?;
    }
    let rolled = user
        .run(
            "rollback-undone",
            &[
                "--workers",
                "1",
                "toolbox",
                "import",
                &path,
                "--rollback-on-failure",
                "-o",
                "json",
            ],
        )
        .await;
    assert_eq!(rolled.status.code(), Some(1), "{}", stderr(&rolled));
    let report = json(&rolled);
    assert_eq!(report["outcome"], "rolled_back", "{report}");
    assert_eq!(report["rolled_back"].as_array().expect("undone").len(), 2);
    assert!(tool_labels(&user.client, &user.group).await?.is_empty());
    Ok(())
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
    let dir = std::env::temp_dir().join(unique("thorctl-admin-"));
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

/// Run `thorctl` with an admin's keys
///
/// # Arguments
///
/// * `keys` - The admin's keys file
/// * `args` - The arguments after the global flags
async fn as_admin(keys: &Path, args: &[&str]) -> Output {
    // the same environment the test user's commands run in
    tokio::time::timeout(
        Duration::from_secs(300),
        Command::new(env!("CARGO_BIN_EXE_thorctl"))
            .arg("--keys")
            .arg(keys)
            .arg("--skip-update")
            .args(args)
            .env("NO_PROXY", "127.0.0.1,localhost,::1")
            .env("no_proxy", "127.0.0.1,localhost,::1")
            .stdin(std::process::Stdio::null())
            .output(),
    )
    .await
    .expect("thorctl timed out")
    .expect("thorctl ran")
}

/// The real toolbox, as a legacy file and as its converted copy, imports into two fresh groups
/// with the same spec hashes, again as a no-op, and a developer is refused before any write
#[tokio::test]
async fn the_real_toolbox_imports_from_both_formats() -> Result<(), Error> {
    // two groups the user owns, and an admin
    let user = TestUser::new("toolbox-real").await?;
    let second = unique("ctlgroup2");
    user.admin
        .groups
        .create(
            &thorium::models::GroupRequest::new(&second).owners(
                thorium::models::GroupUsersRequest::default()
                    .direct("thorium")
                    .direct(&user.username),
            ),
        )
        .await?;
    let keys = admin_keys(&user).await?;
    // a developer is refused, because tools of the toolbox run as root
    let refused = import(
        &user,
        "real-developer",
        Path::new(REAL_TOOLBOX),
        &["--group-override", &user.group],
    )
    .await;
    assert!(!refused.status.success());
    assert!(
        stderr(&refused).contains("only an admin") || stdout(&refused).contains("only an admin")
    );
    assert!(tool_labels(&user.client, &user.group).await?.is_empty());
    // the admin imports the legacy file into the first group
    let legacy = as_admin(
        &keys,
        &[
            "toolbox",
            "import",
            REAL_TOOLBOX,
            "--group-override",
            &user.group,
            "-o",
            "json",
        ],
    )
    .await;
    assert_eq!(legacy.status.code(), Some(0), "{}", stderr(&legacy));
    let report = json(&legacy);
    assert_eq!(report["outcome"], "applied", "{report}");
    // and the converted file into the second
    let v2 = as_admin(
        &keys,
        &[
            "toolbox",
            "import",
            REAL_TOOLBOX_V2,
            "--group-override",
            &second,
            "-o",
            "json",
        ],
    )
    .await;
    assert_eq!(v2.status.code(), Some(0), "{}", stderr(&v2));
    // 59 tools and 32 pipelines in each, with the same hashes
    let from_legacy = latest_hashes(&user.client, &user.group).await?;
    let from_v2 = latest_hashes(&user.client, &second).await?;
    assert_eq!(from_legacy.len(), 59);
    assert_eq!(from_legacy, from_v2);
    // again is a no-op for both
    for (group, manifest) in [(&user.group, REAL_TOOLBOX), (&second, REAL_TOOLBOX_V2)] {
        let again = as_admin(
            &keys,
            &[
                "toolbox",
                "import",
                manifest,
                "--group-override",
                group,
                "-o",
                "json",
            ],
        )
        .await;
        assert_eq!(again.status.code(), Some(0), "{}", stderr(&again));
        assert_eq!(json(&again)["outcome"], "nothing_to_do");
    }
    Ok(())
}

/// A missing group is created with --create-groups, and the import is refused without it
#[tokio::test]
async fn create_groups_creates_missing_groups() -> Result<(), Error> {
    // a toolbox for a group that doesn't exist
    let user = TestUser::new("toolbox-groups").await?;
    let keys = admin_keys(&user).await?;
    let group = unique("newgroup");
    let manifest = write_manifest(&json!({
        "format_version": 2, "name": "t",
        "groups": {group.clone(): {"description": "made by an import"}},
        "tools": {unique("tool"): tool(&group, &[("1", image_spec("registry.local/g:1"))])}
    }));
    let path = manifest.to_string_lossy().into_owned();
    // refused without the flag, naming it
    let refused = as_admin(&keys, &["toolbox", "import", &path]).await;
    assert!(!refused.status.success());
    assert!(
        stdout(&refused).contains("--create-groups")
            || stderr(&refused).contains("--create-groups")
    );
    assert!(user.admin.groups.get(&group).await.is_err());
    // created with it
    let created = as_admin(&keys, &["toolbox", "import", &path, "--create-groups"]).await;
    assert_eq!(created.status.code(), Some(0), "{}", stderr(&created));
    let made = user.admin.groups.get(&group).await?;
    assert_eq!(made.description.as_deref(), Some("made by an import"));
    Ok(())
}

/// A network policy of a toolbox is created with the tools that name it, reused when it matches,
/// left alone when it differs, and changed only with --update-network-policy
#[tokio::test]
async fn network_policies_are_created_reused_and_updated_on_request() -> Result<(), Error> {
    // a tool that names a policy the toolbox defines, imported by an admin
    let user = TestUser::new("toolbox-policies").await?;
    let keys = admin_keys(&user).await?;
    let policy = unique("policy");
    let toolbox = |forced: bool| {
        let mut spec = image_spec("registry.local/p:1");
        spec["Image"]["environment"]["K8s"]["network_policies"] = json!([policy.clone()]);
        write_manifest(&json!({
            "format_version": 2, "name": "t",
            "network_policies": [{
                "name": policy.clone(), "groups": [user.group.clone()],
                "ingress": null, "egress": null,
                "forced_policy": forced, "default_policy": false
            }],
            "tools": {unique("tool"): tool(&user.group, &[("1", spec)])}
        }))
    };
    let first = toolbox(false);
    let path = first.to_string_lossy().into_owned();
    // the first import creates the policy
    let created = as_admin(&keys, &["toolbox", "import", &path, "-o", "json"]).await;
    assert_eq!(created.status.code(), Some(0), "{}", stderr(&created));
    assert_eq!(json(&created)["policies_created"], json!([policy]));
    assert!(
        !user
            .admin
            .network_policies
            .get(&policy, None)
            .await?
            .forced_policy
    );
    // importing it again reuses the policy
    let again = as_admin(&keys, &["toolbox", "import", &path, "-o", "json"]).await;
    assert_eq!(json(&again)["outcome"], "nothing_to_do");
    // a toolbox whose policy differs leaves the policy alone, and says so
    let changed = toolbox(true);
    let changed_path = changed.to_string_lossy().into_owned();
    let left = as_admin(
        &keys,
        &[
            "toolbox",
            "import",
            &changed_path,
            "--overwrite",
            "-o",
            "json",
        ],
    )
    .await;
    assert_eq!(left.status.code(), Some(0), "{}", stderr(&left));
    assert!(
        json(&left)["notes"][0]
            .as_str()
            .expect("a note")
            .contains("--update-network-policy")
    );
    assert!(
        !user
            .admin
            .network_policies
            .get(&policy, None)
            .await?
            .forced_policy
    );
    // with the flag the policy is brought in line
    let updated = as_admin(
        &keys,
        &[
            "toolbox",
            "import",
            &changed_path,
            "--update-network-policy",
            "-o",
            "json",
        ],
    )
    .await;
    assert_eq!(updated.status.code(), Some(0), "{}", stderr(&updated));
    let report = json(&updated);
    assert_eq!(report["policies_updated"], json!([policy]), "{report}");
    assert!(
        user.admin
            .network_policies
            .get(&policy, None)
            .await?
            .forced_policy
    );
    Ok(())
}
