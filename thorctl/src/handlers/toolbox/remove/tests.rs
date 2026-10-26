//! Tests for removing a toolbox's items from Thorium

use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use thorium::Thorium;
use thorium::models::{EntityKinds, EntityMetadata, ToolKind, ToolSetEntity};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use uuid::Uuid;

use super::{Kept, RemovalPlan, Step, execute, plan_removal};
use crate::handlers::progress::{Bar, BarKind};
use crate::handlers::toolbox::export::live::{LiveEntity, LiveView};
use crate::handlers::toolbox::export::snapshot::{SnapTool, Snapshot};
use crate::handlers::toolbox::export::tests::{
    built, container_spec, image_spec, instance, pipeline, snap_tool, tool_entity,
};
use crate::handlers::toolbox::v2::load::parse_any;
use crate::handlers::toolbox::v2::plan_types::ItemKind;
use crate::handlers::toolbox::v2::types::ToolboxManifestV2;

/// A toolbox of a pipeline, a workflow, an agent, a toolset, and two tools where `caller` pins a
/// version of `base`
fn snapshot() -> (Snapshot, SnapTool, SnapTool) {
    // the two tools, with `caller` pinning `base`'s first version
    let base = snap_tool(
        "base",
        &["g"],
        ToolKind::Image,
        vec![
            instance(Uuid::new_v4(), "2.0.0", image_spec("b:2")),
            instance(Uuid::new_v4(), "1.0.0", image_spec("b:1")),
        ],
    );
    let pinned = base.instances[1].id;
    let caller = snap_tool(
        "caller",
        &["g"],
        ToolKind::Agentic,
        vec![instance(
            Uuid::new_v4(),
            "1.0.0",
            container_spec("call_base", pinned),
        )],
    );
    // the entities built on them
    let root = tool_entity(caller.id, "caller", &["g"], ToolKind::Agentic, None);
    let named = |name: &str, kind, metadata| thorium::models::Entity {
        id: Uuid::new_v4(),
        name: name.to_owned(),
        kind,
        metadata,
        ..tool_entity(Uuid::new_v4(), "x", &["g"], ToolKind::Image, None)
    };
    let toolset = named(
        "kit",
        EntityKinds::ToolSet,
        EntityMetadata::ToolSet(ToolSetEntity {
            tools: vec![root.clone()],
            ..ToolSetEntity::default()
        }),
    );
    let agent = named(
        "bot",
        EntityKinds::Agent,
        EntityMetadata::Agent(thorium::models::AgentEntity {
            latest_tools: vec![root],
            ..thorium::models::AgentEntity::default()
        }),
    );
    let workflow = named(
        "flow",
        EntityKinds::WorkflowCatalog,
        EntityMetadata::WorkflowCatalog(thorium::models::WorkflowCatalogEntity {
            agents: vec![agent.clone()],
            ..thorium::models::WorkflowCatalogEntity::default()
        }),
    );
    let stage = caller.id;
    let snapshot = Snapshot {
        tools: vec![base.clone(), caller.clone()],
        toolsets: vec![toolset],
        agents: vec![agent],
        workflows: vec![workflow],
        pipelines: vec![pipeline("g", "scan", &[("caller", stage)])],
        ..Snapshot::default()
    };
    (snapshot, base, caller)
}

/// What Thorium holds when the whole toolbox was imported
///
/// # Arguments
///
/// * `snapshot` - What the toolbox was built from
fn live_of(snapshot: &Snapshot) -> LiveView {
    // every item of the toolbox exists, with the ids it was built from
    let mut live = LiveView {
        tools: snapshot.tools.clone(),
        pipelines: snapshot.pipelines.clone(),
        ..LiveView::default()
    };
    for (kind, found) in [
        (ItemKind::Toolset, &snapshot.toolsets),
        (ItemKind::Agent, &snapshot.agents),
        (ItemKind::Workflow, &snapshot.workflows),
    ] {
        live.entities.extend(found.iter().map(|entity| LiveEntity {
            kind,
            id: entity.id,
            name: entity.name.clone(),
            groups: entity.groups.clone(),
        }));
    }
    live
}

/// Name the steps of a plan in order
///
/// # Arguments
///
/// * `plan` - The plan
fn labels(plan: &RemovalPlan) -> Vec<String> {
    // each step's label
    plan.steps.iter().map(Step::label).collect()
}

/// Items are removed dependents first: pipelines, workflows, agents, toolsets, then each tool's
/// versions and the tool, with tools that others depend on last
#[test]
fn removal_follows_dependency_order() {
    // a toolbox that was fully imported
    let (snapshot, _, _) = snapshot();
    let manifest = built(&snapshot);
    let plan = plan_removal(&manifest, &live_of(&snapshot));
    assert_eq!(
        labels(&plan),
        [
            "pipeline g/scan",
            "workflow g/flow",
            "agent g/bot",
            "toolset g/kit",
            // the tool that pins another goes first, then the one it pins
            "tool g/caller@1.0.0",
            "tool g/caller",
            "tool g/base@2.0.0",
            "tool g/base@1.0.0",
            "tool g/base",
        ]
    );
    assert!(plan.missing.is_empty() && plan.kept.is_empty());
}

/// Items Thorium doesn't have are reported and not removed
#[test]
fn missing_items_are_reported() {
    // nothing exists in Thorium but the base tool
    let (snapshot, base, _) = snapshot();
    let manifest = built(&snapshot);
    let live = LiveView {
        tools: vec![base],
        ..LiveView::default()
    };
    let plan = plan_removal(&manifest, &live);
    assert_eq!(
        labels(&plan),
        ["tool g/base@2.0.0", "tool g/base@1.0.0", "tool g/base"]
    );
    for item in [
        "pipeline scan",
        "workflow flow",
        "agent bot",
        "toolset kit",
        "tool caller",
    ] {
        assert!(
            plan.missing.contains(&item.to_owned()),
            "{item}: {:?}",
            plan.missing
        );
    }
}

/// A tool a pipeline outside the toolbox runs is left, and so is the tool it pins
#[test]
fn tools_used_outside_the_toolbox_are_kept() {
    // a pipeline that is not in the toolbox runs `caller`
    let (snapshot, _, caller) = snapshot();
    let manifest = built(&snapshot);
    let mut live = live_of(&snapshot);
    live.pipelines
        .push(pipeline("g", "mine", &[("caller", caller.instances[0].id)]));
    let plan = plan_removal(&manifest, &live);
    // the toolbox's own pipeline goes, and neither tool is removed
    assert_eq!(
        labels(&plan),
        [
            "pipeline g/scan",
            "workflow g/flow",
            "agent g/bot",
            "toolset g/kit"
        ]
    );
    assert_eq!(
        plan.kept,
        [
            Kept {
                item: "tool g/caller".to_owned(),
                why: "used by pipeline g/mine".to_owned()
            },
            Kept {
                item: "tool g/base".to_owned(),
                why: "tool caller depends on it and is left in place".to_owned()
            },
        ]
    );
}

/// Versions published beside the toolbox's keep the tool, and conflict versions go with it
#[test]
fn versions_outside_the_toolbox_keep_the_tool() {
    // Thorium also holds a version the toolbox doesn't name, and a conflict-published one
    let (snapshot, mut base, _) = snapshot();
    let manifest = built(&snapshot);
    base.instances
        .insert(0, instance(Uuid::new_v4(), "9.9.9", image_spec("b:9")));
    base.instances.push(instance(
        Uuid::new_v4(),
        "1.0.0+0123abcd",
        image_spec("b:1x"),
    ));
    let live = LiveView {
        tools: vec![base],
        ..LiveView::default()
    };
    let plan = plan_removal(&manifest, &live);
    // only the versions the toolbox names (and its conflict labels) are removed
    assert_eq!(
        labels(&plan),
        [
            "tool g/base@2.0.0",
            "tool g/base@1.0.0",
            "tool g/base@1.0.0+0123abcd"
        ]
    );
    assert_eq!(plan.kept.len(), 1);
    assert!(plan.kept[0].why.contains("9.9.9"), "{:?}", plan.kept);
}

/// A pipeline the toolbox removes doesn't block its tools
#[test]
fn the_toolboxs_own_pipelines_do_not_block() {
    // the toolbox's pipeline is the only user of `caller`
    let (snapshot, _, _) = snapshot();
    let manifest = built(&snapshot);
    let plan = plan_removal(&manifest, &live_of(&snapshot));
    assert_eq!(plan.kept.len(), 0);
    assert!(labels(&plan).contains(&"tool g/caller".to_owned()));
}

/// A legacy toolbox is removed as the tools its images converted to
#[tokio::test]
async fn a_legacy_toolbox_removes_its_images_tools() {
    // the small legacy toolbox, read as a v2 manifest
    let text = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/toolbox_v2/small.legacy.json"
    ));
    let bar = Bar::new_or_quiet("test", "test", BarKind::UnboundIO, true);
    let manifest: ToolboxManifestV2 = parse_any(text.as_bytes(), None, &bar)
        .await
        .unwrap()
        .manifest;
    // a Thorium that holds its clamav tool
    let live = LiveView {
        tools: vec![snap_tool(
            "clamav",
            &["static"],
            ToolKind::Image,
            vec![instance(Uuid::new_v4(), "latest", image_spec("c:1"))],
        )],
        ..LiveView::default()
    };
    let plan = plan_removal(&manifest, &live);
    assert_eq!(
        labels(&plan),
        ["tool static/clamav@latest", "tool static/clamav"]
    );
}

/// A mock API answering each request line, with the request lines it received
///
/// # Arguments
///
/// * `router` - Picks the status of each answer from its request line
async fn mock<F>(router: F) -> (String, Arc<Mutex<Vec<String>>>)
where
    F: Fn(&str) -> u16 + Send + Sync + 'static,
{
    // answer each request in the background
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let host = format!("http://{}", listener.local_addr().unwrap());
    let lines = Arc::new(Mutex::new(Vec::new()));
    let seen = lines.clone();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let mut buf = vec![0_u8; 8192];
            let read = stream.read(&mut buf).await.unwrap_or(0);
            let text = String::from_utf8_lossy(&buf[..read]).to_string();
            let line = text.lines().next().unwrap_or_default().to_owned();
            seen.lock().unwrap().push(line.clone());
            let code = router(&line);
            let body = json!({"error": "refused"}).to_string();
            let response = format!(
                "HTTP/1.1 {code} Mock\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.shutdown().await;
        }
    });
    (host, lines)
}

/// Carrying out a plan deletes in order, counts what is gone, and doesn't delete a tool whose
/// version could not be deleted
#[tokio::test]
async fn execution_reports_gone_refused_and_failed_items() {
    // a plan with a pipeline, a refused version, a failing version, and their tools
    let refused = Uuid::new_v4();
    let gone = Uuid::new_v4();
    let step = |name: &str, id| Step::ToolVersion {
        tool: name.to_owned(),
        label: "1".to_owned(),
        id,
        group: "g".to_owned(),
    };
    let tool = |name: &str| Step::Tool {
        name: name.to_owned(),
        id: Uuid::new_v4(),
        group: "g".to_owned(),
    };
    let plan = RemovalPlan {
        steps: vec![
            Step::Pipeline {
                group: "g".to_owned(),
                name: "scan".to_owned(),
            },
            step("pinned", refused),
            tool("pinned"),
            step("vanished", gone),
            tool("vanished"),
        ],
        ..RemovalPlan::default()
    };
    // the API refuses one instance, doesn't know another, and deletes the rest
    let (refused_text, gone_text) = (refused.to_string(), gone.to_string());
    let (host, lines) = mock(move |line| {
        if line.contains(&refused_text) {
            409
        } else if line.contains(&gone_text) {
            404
        } else {
            200
        }
    })
    .await;
    let thorium = Thorium::build(&host).token("token").build().await.unwrap();
    let bar = Bar::new_or_quiet("test", "test", BarKind::UnboundIO, true);
    let outcome = execute(&thorium, &plan, &bar).await;
    // the pipeline and the vanished tool's own delete went through
    assert_eq!(outcome.removed, 2);
    assert_eq!(outcome.gone.len(), 1);
    assert!(outcome.gone[0].contains("vanished@1"));
    // the refused version is reported and its tool was never deleted
    assert_eq!(outcome.refused.len(), 1);
    assert!(outcome.refused[0].contains("tool g/pinned@1"));
    assert_eq!(outcome.failures.len(), 0);
    let sent = lines.lock().unwrap().clone();
    assert_eq!(sent.len(), 4, "{sent:?}");
    assert!(sent[0].starts_with("DELETE /api/pipelines/g/scan"));
    let mut by_kind: HashMap<&str, usize> = HashMap::new();
    for line in &sent {
        *by_kind.entry("entities").or_default() += usize::from(line.contains("/api/entities/"));
    }
    assert_eq!(by_kind["entities"], 3);
}
