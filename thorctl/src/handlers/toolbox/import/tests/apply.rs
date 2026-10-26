//! Tests for applying an import, rolling it back, and importing again, against the in-memory fake

use serde_json::{Value, json};
use std::collections::BTreeMap;

use super::{
    FakeApi, NoPrompts, REAL_TOOLBOX, REAL_TOOLBOX_V2, hash_of, image_spec,
    image_spec_depending_on, manifest, quiet, settings, tool_json,
};
use crate::handlers::agents::fixtures::{tool as tool_entity, toolset_with};
use crate::handlers::toolbox::convert::{ConvertOptions, convert_document};
use crate::handlers::toolbox::import::report::{Action, ImportReport, Outcome};
use crate::handlers::toolbox::import::{Decider, Settings, execute};
use crate::handlers::toolbox::v2::plan_types::{
    Conflict, ConflictPolicy, ItemKind, ItemRef, Plan, Resolution,
};
use crate::handlers::toolbox::v2::types::ToolboxManifestV2;

/// Run an import with no one to ask
///
/// # Arguments
///
/// * `api` - The fake Thorium
/// * `manifest` - The manifest to import
/// * `settings` - What the import is told to do
async fn run(api: &FakeApi, manifest: &ToolboxManifestV2, settings: &Settings) -> ImportReport {
    // nothing is asked
    execute(api, manifest, settings, &mut NoPrompts, &quiet())
        .await
        .expect("an import")
}

/// A toolbox of tools that depend on each other, and a pipeline that runs the last one
fn chain() -> Value {
    // pipeline -> c -> b -> a
    json!({
        "name": "chain",
        "tools": {
            "a": tool_json(&["lab"], &[("1", image_spec("a:1"))]),
            "b": tool_json(&["lab"], &[("1", image_spec_depending_on("b:1", &["ref:tool/a"]))]),
            "c": tool_json(&["lab"], &[("1", image_spec_depending_on("c:1", &["ref:tool/b"]))])
        },
        "pipelines": {"run": {"versions": {"latest": {
            "config": {"group": "lab", "name": "run", "order": [["c"]]},
            "stage_tools": {"c": {"tool": "c", "version": "1"}}
        }}}}
    })
}

/// Importing creates tools in dependency order, then the pipelines that run them
#[tokio::test]
async fn an_import_creates_items_in_dependency_order() {
    // import the chain into a group that exists
    let api = FakeApi::with_groups(&["lab"]);
    let report = run(&api, &manifest(&chain()), &settings()).await;
    assert_eq!(report.outcome, Outcome::Applied, "{:?}", report.errors);
    // a before b before c before the pipeline
    assert_eq!(
        api.writes(),
        [
            "create tool a 1",
            "create tool b 1",
            "create tool c 1",
            "create pipeline run"
        ]
    );
    assert_eq!(report.count(Action::Created), 4);
    // everything exists afterwards
    assert_eq!(api.tool_names(), ["a", "b", "c"]);
    assert_eq!(api.pipeline_names(), ["run"]);
}

/// Importing the same toolbox again changes nothing
#[tokio::test]
async fn a_second_import_changes_nothing() {
    // import twice
    let api = FakeApi::with_groups(&["lab"]);
    let manifest = manifest(&chain());
    run(&api, &manifest, &settings()).await;
    let writes = api.writes().len();
    let again = run(&api, &manifest, &settings()).await;
    // nothing to do, and no write
    assert_eq!(again.outcome, Outcome::NothingToDo);
    assert_eq!(api.writes().len(), writes);
    assert_eq!(again.count(Action::Unchanged), 4);
    assert!(again.success());
}

/// A changed spec publishes only the new version
#[tokio::test]
async fn a_changed_version_is_published_alone() {
    // import version 1, then a toolbox that adds version 2 and keeps version 1
    let api = FakeApi::with_groups(&["lab"]);
    let first = manifest(&json!({
        "name": "t", "tools": {"a": tool_json(&["lab"], &[("1", image_spec("a:1"))])}
    }));
    run(&api, &first, &settings()).await;
    let second = manifest(&json!({
        "name": "t",
        "tools": {"a": tool_json(&["lab"], &[("1", image_spec("a:1")), ("2", image_spec("a:2"))])}
    }));
    let report = run(&api, &second, &settings()).await;
    // only the new version was written
    assert_eq!(report.outcome, Outcome::Applied, "{:?}", report.errors);
    assert_eq!(api.writes(), ["create tool a 1", "publish 2"]);
    assert_eq!(api.labels("a"), ["1", "2"]);
    assert_eq!(report.count(Action::Updated), 1);
}

/// A toolbox whose tool holds one spec under two labels, and a pipeline that pins the second
fn duplicate_labels() -> Value {
    // 1.0.0 and latest hold the same spec, and the pipeline runs latest
    let mut tool = tool_json(
        &["lab"],
        &[("1.0.0", image_spec("a:1")), ("latest", image_spec("a:1"))],
    );
    tool["latest"] = json!("latest");
    json!({
        "name": "dup",
        "tools": {"a": tool},
        "pipelines": {"run": {"versions": {"latest": {
            "config": {"group": "lab", "name": "run", "order": [["a"]]},
            "stage_tools": {"a": {"tool": "a", "version": "latest"}}
        }}}}
    })
}

/// A label whose spec another label of the same toolbox publishes is pinned to that instance
#[tokio::test]
async fn a_duplicate_label_resolves_to_the_published_spec() {
    // import a tool with two labels for one spec and a pipeline pinning the second label
    let api = FakeApi::with_groups(&["lab"]);
    let report = run(&api, &manifest(&duplicate_labels()), &settings()).await;
    // the spec is published once and the pipeline is created
    assert_eq!(report.outcome, Outcome::Applied, "{:?}", report.errors);
    assert_eq!(api.writes(), ["create tool a 1.0.0", "create pipeline run"]);
    assert_eq!(api.pipeline_names(), ["run"]);
}

/// A label whose spec Thorium already holds under another label is pinned to that instance
#[tokio::test]
async fn a_duplicate_of_a_live_spec_resolves_to_it() {
    // Thorium already holds the spec as 1.0.0
    let api = FakeApi::with_groups(&["lab"]);
    let first = manifest(&json!({
        "name": "dup", "tools": {"a": tool_json(&["lab"], &[("1.0.0", image_spec("a:1"))])}
    }));
    run(&api, &first, &settings()).await;
    // a toolbox adding latest with the same spec and a pipeline pinning it
    let report = run(&api, &manifest(&duplicate_labels()), &settings()).await;
    // nothing is published again and the pipeline is created
    assert_eq!(report.outcome, Outcome::Applied, "{:?}", report.errors);
    assert_eq!(api.writes(), ["create tool a 1.0.0", "create pipeline run"]);
    assert_eq!(api.labels("a"), ["1.0.0"]);
}

/// A label taken by another spec is published as LABEL+HASH with --overwrite
#[tokio::test]
async fn a_label_conflict_is_published_under_a_hash_label() {
    // import 1.0, then another spec under the same label
    let api = FakeApi::with_groups(&["lab"]);
    let first = manifest(&json!({
        "name": "t", "tools": {"a": tool_json(&["lab"], &[("1.0", image_spec("a:1"))])}
    }));
    run(&api, &first, &settings()).await;
    let second = manifest(&json!({
        "name": "t", "tools": {"a": tool_json(&["lab"], &[("1.0", image_spec("a:2"))])}
    }));
    let report = run(&api, &second, &settings()).await;
    // the new spec is published as 1.0+hash8
    assert_eq!(report.outcome, Outcome::Applied, "{:?}", report.errors);
    let prefix: String = hash_of(&image_spec("a:2")).chars().take(8).collect();
    assert_eq!(api.labels("a"), ["1.0".to_owned(), format!("1.0+{prefix}")]);
    // importing it again is a no-op
    let again = run(&api, &second, &settings()).await;
    assert_eq!(again.outcome, Outcome::NothingToDo);
}

/// Skipping conflicts writes nothing for items that exist and differ
#[tokio::test]
async fn skip_conflicts_writes_nothing_for_existing_items() {
    // a tool exists with another spec; the toolbox also has a new tool
    let api = FakeApi::with_groups(&["lab"]);
    let first = manifest(&json!({
        "name": "t", "tools": {"a": tool_json(&["lab"], &[("1", image_spec("a:1"))])}
    }));
    run(&api, &first, &settings()).await;
    let second = manifest(&json!({
        "name": "t",
        "tools": {
            "a": tool_json(&["lab"], &[("1", image_spec("a:9"))]),
            "b": tool_json(&["lab"], &[("1", image_spec("b:1"))])
        }
    }));
    let mut skip = settings();
    skip.policy = ConflictPolicy::SkipConflicts;
    let report = run(&api, &second, &skip).await;
    // b is created and a is left alone
    assert_eq!(report.outcome, Outcome::Applied);
    assert_eq!(api.writes(), ["create tool a 1", "create tool b 1"]);
    assert_eq!(api.labels("a"), ["1"]);
    assert_eq!(report.count(Action::Skipped), 1);
}

/// A dry run plans and writes nothing
#[tokio::test]
async fn a_dry_run_writes_nothing() {
    // plan the chain
    let api = FakeApi::with_groups(&["lab"]);
    let mut dry = settings();
    dry.dry_run = true;
    let report = run(&api, &manifest(&chain()), &dry).await;
    // nothing was written, and the plan lists what would be
    assert_eq!(report.outcome, Outcome::DryRun);
    assert!(api.writes().is_empty());
    assert_eq!(report.plan.summary().create, 4);
}

/// A missing group is created first when asked, and rolled back last
#[tokio::test]
async fn groups_are_created_first_and_rolled_back_last() {
    // import into a group that doesn't exist, failing on the pipeline
    let api = FakeApi::with_groups(&[]);
    let mut create = settings();
    create.create_groups = true;
    create.rollback_on_failure = true;
    // writes: group, a, b, c, then the pipeline is the fifth
    api.fail_write(5);
    let chain = chain();
    let report = run(&api, &manifest(&chain), &create).await;
    // everything was undone in reverse order: c, b, a, then the group
    assert_eq!(report.outcome, Outcome::RolledBack, "{:?}", report.errors);
    let writes = api.writes();
    assert_eq!(
        &writes[..4],
        [
            "create group lab",
            "create tool a 1",
            "create tool b 1",
            "create tool c 1"
        ]
    );
    let undone: Vec<&String> = writes[4..]
        .iter()
        .filter(|w| w.starts_with("delete"))
        .collect();
    // each created tool is deleted, c's before b's before a's, and the group last
    assert_eq!(undone.len(), 7, "{writes:?}");
    assert_eq!(writes.last().map(String::as_str), Some("delete group lab"));
    assert!(api.tool_names().is_empty());
    assert!(api.group_names().is_empty());
    assert_eq!(
        report.rolled_back.first().map(String::as_str),
        Some("created tool c")
    );
    assert_eq!(
        report.rolled_back.last().map(String::as_str),
        Some("created group lab")
    );
    assert!(!report.success());
}

/// Without the rollback flag a failure keeps what was applied and reports the rest
#[tokio::test]
async fn a_failure_keeps_what_was_applied() {
    // fail the creation of tool b (the second write)
    let api = FakeApi::with_groups(&["lab"]);
    api.fail_write(2);
    let report = run(&api, &manifest(&chain()), &settings()).await;
    // a stays, b failed, and nothing after it was attempted
    assert_eq!(report.outcome, Outcome::Failed);
    assert_eq!(api.tool_names(), ["a"]);
    let action = |kind: ItemKind, name: &str| {
        report
            .results
            .iter()
            .find(|result| result.item == ItemRef::new(kind, name))
            .map(|result| result.action)
    };
    assert_eq!(action(ItemKind::Tool, "a"), Some(Action::Created));
    assert_eq!(action(ItemKind::Tool, "b"), Some(Action::Failed));
    assert_eq!(action(ItemKind::Tool, "c"), Some(Action::NotAttempted));
    assert_eq!(
        action(ItemKind::Pipeline, "run"),
        Some(Action::NotAttempted)
    );
    assert!(report.errors[0].contains("tool b"), "{:?}", report.errors);
    assert!(
        report.errors[0].contains("injected failure"),
        "{:?}",
        report.errors
    );
    // a later import finishes the job
    let resumed = run(&api, &manifest(&chain()), &settings()).await;
    assert_eq!(resumed.outcome, Outcome::Applied);
    assert_eq!(api.tool_names(), ["a", "b", "c"]);
}

/// Rolling back a failed publish removes the versions that were published first
#[tokio::test]
async fn rollback_removes_published_versions_before_their_tool() {
    // a tool with three versions fails on the third publish
    let api = FakeApi::with_groups(&["lab"]);
    let toolbox = manifest(&json!({
        "name": "t",
        "tools": {"a": tool_json(&["lab"], &[
            ("1", image_spec("a:1")), ("2", image_spec("a:2")), ("3", image_spec("a:3"))
        ])}
    }));
    let mut rollback = settings();
    rollback.rollback_on_failure = true;
    api.fail_write(3);
    let report = run(&api, &toolbox, &rollback).await;
    // the tool and its second version are gone again
    assert_eq!(report.outcome, Outcome::RolledBack, "{:?}", report.errors);
    assert!(api.tool_names().is_empty());
    assert_eq!(
        report.rolled_back,
        ["published a@2".to_owned(), "created tool a".to_owned()]
    );
}

/// Without a way to ask, differences that need a decision refuse the import before any write
#[tokio::test]
async fn unresolved_conflicts_refuse_the_import() {
    // a tool exists with another spec
    let api = FakeApi::with_groups(&["lab"]);
    let first = manifest(&json!({
        "name": "t", "tools": {"a": tool_json(&["lab"], &[("1", image_spec("a:1"))])}
    }));
    run(&api, &first, &settings()).await;
    let writes = api.writes().len();
    let second = manifest(&json!({
        "name": "t", "tools": {"a": tool_json(&["lab"], &[("1", image_spec("a:2"))])}
    }));
    let mut ask = settings();
    ask.policy = ConflictPolicy::Ask;
    let report = run(&api, &second, &ask).await;
    // refused, naming the flags and the item
    assert_eq!(report.outcome, Outcome::Refused);
    assert!(report.errors[0].contains("--overwrite") && report.errors[0].contains("tool a"));
    assert_eq!(api.writes().len(), writes);
    assert!(!report.success());
}

/// A decider that answers from a list
struct Script {
    /// The answers, in order
    answers: Vec<Resolution>,
    /// How many times a conflict was asked about
    asked: usize,
    /// The answer to the final confirmation
    go: bool,
}

impl Decider for Script {
    /// Answer from the list
    ///
    /// # Arguments
    ///
    /// * `_conflict` - How the item differs
    /// * `_diff` - The difference
    fn resolve(&mut self, _conflict: &Conflict, _diff: Option<&str>) -> Resolution {
        self.asked += 1;
        self.answers.remove(0)
    }

    /// Answer the confirmation
    ///
    /// # Arguments
    ///
    /// * `_plan` - The plan
    fn confirm(&mut self, _plan: &Plan) -> bool {
        self.go
    }

    /// Decline to roll back
    ///
    /// # Arguments
    ///
    /// * `_applied` - What was applied
    fn rollback(&mut self, _applied: &[String]) -> bool {
        false
    }
}

/// Two tools that differ, imported with a person deciding each
#[tokio::test]
async fn a_person_decides_each_conflict() {
    // two tools exist, and the toolbox changes both
    let api = FakeApi::with_groups(&["lab"]);
    let first = manifest(&json!({
        "name": "t",
        "tools": {
            "a": tool_json(&["lab"], &[("1", image_spec("a:1"))]),
            "b": tool_json(&["lab"], &[("1", image_spec("b:1"))])
        }
    }));
    run(&api, &first, &settings()).await;
    let second = manifest(&json!({
        "name": "t",
        "tools": {
            "a": tool_json(&["lab"], &[("1", image_spec("a:1")), ("2", image_spec("a:2"))]),
            "b": tool_json(&["lab"], &[("1", image_spec("b:1")), ("2", image_spec("b:2"))])
        }
    }));
    // apply a, skip b
    let mut ask = settings();
    ask.policy = ConflictPolicy::Ask;
    ask.can_prompt = true;
    ask.confirm = true;
    let mut script = Script {
        answers: vec![Resolution::Apply, Resolution::Skip],
        asked: 0,
        go: true,
    };
    let report = execute(&api, &second, &ask, &mut script, &quiet())
        .await
        .expect("import");
    assert_eq!(script.asked, 2);
    assert_eq!(report.outcome, Outcome::Applied, "{:?}", report.errors);
    assert_eq!(api.labels("a"), ["1", "2"]);
    assert_eq!(api.labels("b"), ["1"]);
    // quitting writes nothing
    let mut quit = Script {
        answers: vec![Resolution::Quit],
        asked: 0,
        go: true,
    };
    let writes = api.writes().len();
    let report = execute(&api, &second, &ask, &mut quit, &quiet())
        .await
        .expect("import");
    assert_eq!(report.outcome, Outcome::Cancelled);
    assert_eq!(api.writes().len(), writes);
    // declining the confirmation writes nothing either
    let third = manifest(&json!({
        "name": "t",
        "tools": {"c": tool_json(&["lab"], &[("1", image_spec("c:1"))])}
    }));
    let mut decline = Script {
        answers: Vec::new(),
        asked: 0,
        go: false,
    };
    let report = execute(&api, &third, &ask, &mut decline, &quiet())
        .await
        .expect("import");
    assert_eq!(report.outcome, Outcome::Cancelled);
    assert_eq!(api.writes().len(), writes);
}

/// An update touches only what exists
#[tokio::test]
async fn an_update_only_touches_existing_items() {
    // tool a exists; the toolbox adds a version to it and a whole new tool
    let api = FakeApi::with_groups(&["lab"]);
    let first = manifest(&json!({
        "name": "t", "tools": {"a": tool_json(&["lab"], &[("1", image_spec("a:1"))])}
    }));
    run(&api, &first, &settings()).await;
    let second = manifest(&json!({
        "name": "t",
        "tools": {
            "a": tool_json(&["lab"], &[("1", image_spec("a:1")), ("2", image_spec("a:2"))]),
            "b": tool_json(&["lab"], &[("1", image_spec("b:1"))])
        }
    }));
    let mut update = settings();
    update.create = false;
    let report = run(&api, &second, &update).await;
    // a gained a version; b was not created
    assert_eq!(report.outcome, Outcome::Applied, "{:?}", report.errors);
    assert_eq!(api.tool_names(), ["a"]);
    assert_eq!(api.labels("a"), ["1", "2"]);
    assert_eq!(report.count(Action::NotImported), 1);
    assert_eq!(report.count(Action::Updated), 1);
}

/// The real legacy toolbox imports as tools and pipelines, and again as a no-op
#[tokio::test]
async fn the_real_toolbox_imports_and_reimports() {
    // convert the legacy toolbox in memory and import it into the group it names
    let converted = convert_document(
        REAL_TOOLBOX.as_bytes(),
        None,
        &ConvertOptions::default(),
        &quiet(),
    )
    .await
    .expect("convert");
    let api = FakeApi::with_groups(&["static"]).admin();
    let report = run(&api, &converted.manifest, &settings()).await;
    assert_eq!(report.outcome, Outcome::Applied, "{:?}", report.errors);
    // 59 tools, 32 pipelines, and the one network policy
    assert_eq!(api.tool_names().len(), 59);
    assert_eq!(api.pipeline_names().len(), 32);
    assert_eq!(report.policies_created, ["allow-all"]);
    assert_eq!(report.count(Action::Created), 59 + 32);
    // again is a no-op
    let writes = api.writes().len();
    let again = run(&api, &converted.manifest, &settings()).await;
    assert_eq!(again.outcome, Outcome::NothingToDo, "{:?}", again.errors);
    assert_eq!(api.writes().len(), writes);
}

/// The real toolbox has tools that run as root, which only an admin may publish
#[tokio::test]
async fn the_real_toolbox_needs_an_admin_for_its_root_tools() {
    // a developer who is not an admin imports it
    let converted = convert_document(
        REAL_TOOLBOX.as_bytes(),
        None,
        &ConvertOptions::default(),
        &quiet(),
    )
    .await
    .expect("convert");
    let api = FakeApi::with_groups(&["static"]);
    let report = run(&api, &converted.manifest, &settings()).await;
    // the import is refused before anything is written, naming the tools
    assert_eq!(report.outcome, Outcome::Refused);
    assert!(api.writes().is_empty());
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.contains("cve-bin-tool@latest") && error.contains("only an admin")),
        "{:?}",
        report.errors
    );
}

/// The legacy toolbox and its converted copy plan the same spec hashes
#[tokio::test]
async fn legacy_and_converted_toolboxes_plan_the_same_hashes() {
    // plan both into an empty group
    let legacy = convert_document(
        REAL_TOOLBOX.as_bytes(),
        None,
        &ConvertOptions::default(),
        &quiet(),
    )
    .await
    .expect("convert");
    let v2 = convert_document(
        REAL_TOOLBOX_V2.as_bytes(),
        None,
        &ConvertOptions::default(),
        &quiet(),
    )
    .await
    .expect("parse");
    let mut dry = settings();
    dry.dry_run = true;
    let hashes = |report: &ImportReport| -> BTreeMap<String, Option<String>> {
        report
            .plan
            .items
            .iter()
            .filter(|item| item.item.kind == ItemKind::Tool)
            .map(|item| (item.item.name.clone(), item.versions[0].spec_hash.clone()))
            .collect()
    };
    let from_legacy = run(&FakeApi::with_groups(&["static"]), &legacy.manifest, &dry).await;
    let from_v2 = run(&FakeApi::with_groups(&["static"]), &v2.manifest, &dry).await;
    // the same 59 tools with the same hashes
    assert_eq!(hashes(&from_legacy).len(), 59);
    assert_eq!(hashes(&from_legacy), hashes(&from_v2));
    assert!(hashes(&from_legacy).values().all(Option::is_some));
}

/// Rolling back puts an item that was updated in place back the way it was
#[tokio::test]
async fn rollback_restores_items_updated_in_place() {
    // a toolset that exists with one tool, and the toolbox gives it another and adds a pipeline
    let toolset = toolset_with("kit", |meta| meta.tools = vec![tool_entity("strings")]);
    let api = FakeApi::with_groups(&["lab"]).with_entity(toolset);
    let mut seed = settings();
    seed.rollback_on_failure = true;
    let spec = image_spec("s:1");
    let toolbox = manifest(&json!({
        "name": "t",
        "tools": {"strings": tool_json(&["lab"], &[("1", spec)])},
        "toolsets": {"kit": {"groups": ["lab"], "tools": [{"tool": "strings", "version": "1"}]}},
        "pipelines": {"scan": {"versions": {"latest": {
            "config": {"group": "lab", "name": "scan", "order": [["strings"]]},
            "stage_tools": {"strings": {"tool": "strings"}}
        }}}}
    }));
    // one worker applies the toolset update and then the pipeline; the pipeline create fails
    seed.workers = 1;
    api.fail_write(3);
    let report = run(&api, &toolbox, &seed).await;
    // the toolset was updated, the pipeline failed, and the update was undone with the tool
    assert_eq!(report.outcome, Outcome::RolledBack, "{:?}", report.errors);
    let writes = api.writes();
    assert_eq!(
        writes[..3],
        [
            "create tool strings 1",
            "update toolset kit",
            "update toolset kit"
        ]
    );
    // the tool's version and then the tool are deleted last
    assert_eq!(writes.len(), 5, "{writes:?}");
    assert!(
        writes[3..]
            .iter()
            .all(|write| write.starts_with("delete entity"))
    );
    assert_eq!(
        report.rolled_back,
        [
            "updated toolset kit".to_owned(),
            "created tool strings".to_owned()
        ]
    );
}
