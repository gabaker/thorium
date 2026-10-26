//! Tests for how a plan and the result of an import are shown

use serde_json::{Value, json};

use super::{FakeApi, NoPrompts, image_spec, manifest, quiet, settings, tool_json};
use crate::handlers::toolbox::import::execute;
use crate::handlers::toolbox::import::plan::PlanDocument;
use crate::handlers::toolbox::import::report::{Action, ImportReport, Outcome};
use crate::utils::output::{OutputFormat, View, render};

/// Run an import and get its report
///
/// # Arguments
///
/// * `api` - The fake Thorium
/// * `document` - The manifest as JSON
/// * `dry_run` - Whether to plan only
async fn report_of(api: &FakeApi, document: &Value, dry_run: bool) -> ImportReport {
    // import with nothing to ask
    let mut settings = settings();
    settings.dry_run = dry_run;
    execute(
        api,
        &manifest(document),
        &settings,
        &mut NoPrompts,
        &quiet(),
    )
    .await
    .expect("an import")
}

/// The JSON of a report names its outcome, results, and plan
#[tokio::test]
async fn the_json_report_has_stable_fields() {
    // import a tool
    let api = FakeApi::with_groups(&["lab"]);
    let report = report_of(
        &api,
        &json!({"name": "t", "tools": {"a": tool_json(&["lab"], &[("1", image_spec("a:1"))])}}),
        false,
    )
    .await;
    let document: Value =
        serde_json::from_str(&render(&report, OutputFormat::Json, false).expect("render"))
            .expect("json");
    // the documented fields
    assert_eq!(document["outcome"], "applied");
    assert_eq!(document["results"][0]["item"]["kind"], "tool");
    assert_eq!(document["results"][0]["item"]["name"], "a");
    assert_eq!(document["results"][0]["action"], "created");
    assert_eq!(document["plan"]["items"][0]["state"], "create");
    assert_eq!(document["plan"]["items"][0]["versions"][0]["class"], "new");
    for field in [
        "groups_created",
        "policies_created",
        "rolled_back",
        "errors",
        "notes",
    ] {
        assert!(document[field].is_array(), "{field}");
    }
}

/// The human report counts what happened and names what failed
#[tokio::test]
async fn the_human_report_summarizes() {
    // import a tool, then fail the next
    let api = FakeApi::with_groups(&["lab"]);
    api.fail_write(2);
    let report = report_of(
        &api,
        &json!({"name": "t", "tools": {
            "a": tool_json(&["lab"], &[("1", image_spec("a:1"))]),
            "b": tool_json(&["lab"], &[("1", image_spec("b:1"))])
        }}),
        false,
    )
    .await;
    let text = report.human(false);
    assert!(text.contains("created tool a"), "{text}");
    assert!(text.contains("failed tool b"), "{text}");
    assert!(text.contains("error: tool b"), "{text}");
    assert!(text.contains("Result: failed"), "{text}");
    assert!(text.contains("1 created"), "{text}");
    assert_eq!(report.outcome, Outcome::Failed);
    assert_eq!(report.count(Action::Failed), 1);
}

/// A dry run's plan is shown as a document with its errors, as a table, and as text
#[tokio::test]
async fn a_plan_renders_in_every_format() {
    // plan a tool that depends on a tool nobody has
    let api = FakeApi::with_groups(&["lab"]);
    let report = report_of(
        &api,
        &json!({"name": "t", "tools": {
            "a": tool_json(&["lab"], &[("1", super::image_spec_depending_on("a:1", &["ref:tool/ghost"]))])
        }}),
        true,
    )
    .await;
    // the problem refuses the import, and the plan is still shown
    assert_eq!(report.outcome, Outcome::Refused);
    let document = PlanDocument {
        plan: report.plan.clone(),
        errors: report.errors.clone(),
    };
    let json: Value =
        serde_json::from_str(&render(&document, OutputFormat::Json, false).expect("render"))
            .expect("json");
    assert!(json["errors"][0].as_str().expect("error").contains("ghost"));
    assert_eq!(json["plan"]["items"][0]["item"]["name"], "a");
    assert!(document.table().contains("KIND"));
    let human = document.human(false);
    assert!(
        human.contains("error:") && human.contains("Plan: 1 to create"),
        "{human}"
    );
}
