//! Tests for the workflow resolver, run against an in-memory graph

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;
use uuid::Uuid;

use super::fixture::GraphRead;
use super::test_graph::{Graph, MARS, MINERVA, agentic, container, harness, image};
use super::{
    DedupeKey, GraphEdge, ResolvedWorkflow, WorkflowBuild, build_workflow_instance,
    is_current_with_scheme, newest_instance, resolve_workflow,
};
use crate::models::{
    AgentConfig, AssociationKind, CrawlLimits, Directionality, EntityKinds, EntityMetadata,
    InstancedTool, ModelAuthLevel, ModelAuthSource, ModelMatcher, ToolChoice, ToolKind,
    ToolSetEntity, ToolSetInclusion, WorkflowCatalogEntity, WorkflowInstanceStatus,
    WorkflowInstanceTrigger, WorkflowLimits,
};

mod bindings;

/// The kinds of edge an agent's node read asks for
const AGENT_EDGES: [AssociationKind; 3] = [
    AssociationKind::DelegatesTo,
    AssociationKind::Calls,
    AssociationKind::RunsIn,
];

/// Resolve a workflow with the default limits
///
/// # Arguments
///
/// * `graph` - The graph to read
/// * `workflow` - The workflow to resolve
async fn resolve(graph: &Graph, workflow: Uuid) -> ResolvedWorkflow {
    // resolve under the default crawl limits
    resolve_with(graph, workflow, CrawlLimits::default()).await
}

/// Resolve a workflow with some limits
///
/// # Arguments
///
/// * `graph` - The graph to read
/// * `workflow` - The workflow to resolve
/// * `limits` - The crawl limits
async fn resolve_with(graph: &Graph, workflow: Uuid, limits: CrawlLimits) -> ResolvedWorkflow {
    // resolve and fail the test on a read error
    resolve_workflow(&graph.reader, workflow, &limits)
        .await
        .unwrap()
}

/// Get a resolved instance's errors, or none for a ready instance
///
/// # Arguments
///
/// * `resolved` - The resolved workflow
fn errors(resolved: &ResolvedWorkflow) -> Vec<String> {
    // a ready instance has no errors
    match &resolved.instance.status {
        WorkflowInstanceStatus::Ready => Vec::new(),
        WorkflowInstanceStatus::Invalid { errors } => errors.clone(),
    }
}

/// Check that a resolved instance is ready
///
/// # Arguments
///
/// * `resolved` - The resolved workflow
fn assert_ready(resolved: &ResolvedWorkflow) {
    // a ready instance has no errors to show
    assert_eq!(errors(resolved), Vec::<String>::new());
}

/// Check that a resolved instance is invalid with an error holding every fragment
///
/// # Arguments
///
/// * `resolved` - The resolved workflow
/// * `fragments` - The text one error must hold
fn assert_invalid(resolved: &ResolvedWorkflow, fragments: &[&str]) {
    // one error must hold every fragment
    let found = errors(resolved);
    assert!(
        found
            .iter()
            .any(|error| fragments.iter().all(|fragment| error.contains(fragment))),
        "no error holds {fragments:?}: {found:?}"
    );
}

/// Get the tool instance ids an agent of a resolved instance was given
///
/// # Arguments
///
/// * `resolved` - The resolved workflow
/// * `agent` - The agent
fn tools_of(resolved: &ResolvedWorkflow, agent: Uuid) -> Vec<Uuid> {
    // the agent must be in the instance
    resolved.instance.agent(agent).unwrap().tools.clone()
}

/// Build a set of ids
///
/// # Arguments
///
/// * `ids` - The ids
fn set(ids: &[Uuid]) -> BTreeSet<Uuid> {
    // copy the ids into a set
    ids.iter().copied().collect()
}

/// The entry agent must run in a Harness tool, which is pinned in its snapshot and listed in the
/// instance's tools; a sub-agent needs no harness
#[tokio::test]
async fn entry_agent_harness() {
    // an entry agent without a harness makes the instance invalid
    let mut g = Graph::default();
    let lead = g.agent("lead", &[MINERVA]);
    let helper = g.agent("helper", &[MINERVA]);
    g.edge(AssociationKind::DelegatesTo, lead, helper);
    let wf = g.bare_workflow("Triage", &[MINERVA], &[lead], Some(lead));
    assert_invalid(
        &resolve(&g, wf).await,
        &["entry agent `lead`", "no harness"],
    );
    // a harness followed at latest is pinned in the snapshot and listed as a Harness tool
    let (root, instance) = g.tool("runtime", &[MINERVA], harness(1));
    g.edge(AssociationKind::RunsIn, lead, root);
    let resolved = resolve(&g, wf).await;
    assert_ready(&resolved);
    let snapshot = resolved.instance.agent(lead).unwrap();
    assert_eq!(snapshot.harness, Some(instance));
    assert!(!snapshot.tools.contains(&instance));
    assert_eq!(resolved.instance.agent(helper).unwrap().harness, None);
    let listed = resolved.instance.tool(instance).unwrap();
    assert_eq!(listed.kind, ToolKind::Harness);
    assert_eq!(listed.tool, root);
    assert_eq!(listed.function_name, None);
    assert!(!listed.pinned && !listed.dependency);
    assert!(resolved.follows_tools.contains(&root));
    // a harness edge to a tool that isn't a Harness makes the instance invalid
    let mut g = Graph::default();
    let lead = g.agent("lead", &[MINERVA]);
    let (scan, _) = g.tool("scan", &[MINERVA], agentic("scan"));
    g.edge(AssociationKind::RunsIn, lead, scan);
    let wf = g.workflow("Triage", &[MINERVA], &[lead], Some(lead));
    assert_invalid(
        &resolve(&g, wf).await,
        &["agent `lead`", "`scan@1.0.0`", "Agentic", "not a Harness"],
    );
    // the same holds for a pinned Image instance as a sub-agent's harness
    let mut g = Graph::default();
    let lead = g.agent("lead", &[MINERVA]);
    let helper = g.agent("helper", &[MINERVA]);
    let (_, image_instance) = g.tool("ghidra-cli", &[MINERVA], image(60));
    g.edge(AssociationKind::DelegatesTo, lead, helper);
    g.edge(AssociationKind::RunsIn, helper, image_instance);
    let wf = g.workflow("Triage", &[MINERVA], &[lead], Some(lead));
    assert_invalid(
        &resolve(&g, wf).await,
        &[
            "agent `helper`",
            "`ghidra-cli@1.0.0`",
            "Image",
            "not a Harness",
        ],
    );
}

/// A sub-agent needs a function name and a model description, and its delegation tool's name
/// can't collide with one of its parent's tools or another delegation
#[tokio::test]
async fn delegation_fields() {
    // build a lead calling scan and delegating to a helper with some config
    let delegating = |config: AgentConfig| {
        let mut g = Graph::default();
        let (scan, _) = g.tool("scan", &[MINERVA], agentic("scan"));
        let lead = g.agent("lead", &[MINERVA]);
        let helper = g.agent_with("helper", &[MINERVA], config);
        g.edge(AssociationKind::Calls, lead, scan);
        g.edge(AssociationKind::DelegatesTo, lead, helper);
        let wf = g.workflow("Triage", &[MINERVA], &[lead], Some(lead));
        (g, wf, lead)
    };
    let complete = AgentConfig {
        function_name: Some("helper".to_owned()),
        model_description: Some("Delegate to the helper".to_owned()),
        ..AgentConfig::default()
    };
    // a complete sub-agent is ready
    let (g, wf, _) = delegating(complete.clone());
    assert_ready(&resolve(&g, wf).await);
    // a sub-agent without a function name or a model description is invalid
    let missing_name = AgentConfig {
        function_name: None,
        ..complete.clone()
    };
    let (g, wf, _) = delegating(missing_name);
    assert_invalid(
        &resolve(&g, wf).await,
        &["`lead` delegates to `helper`", "function_name"],
    );
    let missing_description = AgentConfig {
        model_description: None,
        ..complete.clone()
    };
    let (g, wf, _) = delegating(missing_description);
    assert_invalid(
        &resolve(&g, wf).await,
        &["`lead` delegates to `helper`", "model_description"],
    );
    // a delegation named like one of the parent's tools is invalid
    let colliding = AgentConfig {
        function_name: Some("scan".to_owned()),
        ..complete.clone()
    };
    let (g, wf, _) = delegating(colliding);
    assert_invalid(
        &resolve(&g, wf).await,
        &["agent `lead`", "`scan`", "`helper`", "`scan@1.0.0`"],
    );
    // two delegations with one name are invalid
    let (mut g, wf, lead) = delegating(complete.clone());
    let twin = g.agent_with("twin", &[MINERVA], complete);
    g.edge(AssociationKind::DelegatesTo, lead, twin);
    assert_invalid(
        &resolve(&g, wf).await,
        &["agent `lead`", "`helper`", "`twin`"],
    );
}

/// One agent following one tool at latest gives that agent, that tool unpinned, and a followed root
#[tokio::test]
async fn single_latest_tool() {
    // a lead that follows one tool at latest
    let mut g = Graph::default();
    let (root, instance) = g.tool("scan", &[MINERVA], agentic("scan"));
    let lead = g.agent("lead", &[MINERVA]);
    g.edge(AssociationKind::Calls, lead, root);
    let wf = g.workflow("Triage", &[MINERVA], &[lead], Some(lead));
    // resolve it
    let resolved = resolve(&g, wf).await;
    assert_ready(&resolved);
    assert_eq!(resolved.instance.agents.len(), 1);
    assert_eq!(tools_of(&resolved, lead), vec![instance]);
    let tool = resolved.instance.tool(instance).unwrap();
    assert_eq!(tool.tool, root);
    assert!(!tool.pinned);
    assert!(!tool.dependency);
    assert_eq!(tool.function_name.as_deref(), Some("scan"));
    assert_eq!(tool.name, "scan@1.0.0");
    assert_eq!(resolved.follows_tools, set(&[root]));
    assert_eq!(resolved.instance.entry_agent, Some(lead));
    assert_eq!(resolved.instance.workflow, wf);
}

/// A sub-agent reached only through a delegation is in the instance
#[tokio::test]
async fn sub_agent_through_delegation() {
    // a lead that delegates to a helper the workflow doesn't orchestrate
    let mut g = Graph::default();
    let lead = g.agent("lead", &[MINERVA]);
    let helper = g.agent("helper", &[MINERVA]);
    g.edge(AssociationKind::DelegatesTo, lead, helper);
    let wf = g.workflow("Triage", &[MINERVA], &[lead], Some(lead));
    // resolve it
    let resolved = resolve(&g, wf).await;
    assert_ready(&resolved);
    assert!(resolved.instance.agent(helper).is_some());
    assert_eq!(
        resolved.instance.agent(lead).unwrap().sub_agents,
        vec![helper]
    );
}

/// A latest edge resolves to the tool's latest, skipping deprecated instances
#[tokio::test]
async fn latest_skips_deprecated() {
    // three versions, the newest deprecated, followed at latest
    let mut g = Graph::default();
    let (root, _v1) = g.tool("scan", &[MINERVA], agentic("scan"));
    let v2 = g.publish(root, agentic("scan2"), "2.0.0");
    let v3 = g.publish(root, agentic("scan3"), "3.0.0");
    g.deprecate(root, v3);
    let lead = g.agent("lead", &[MINERVA]);
    g.edge(AssociationKind::Calls, lead, root);
    let wf = g.workflow("Triage", &[MINERVA], &[lead], Some(lead));
    // resolve it
    let resolved = resolve(&g, wf).await;
    assert_ready(&resolved);
    assert_eq!(tools_of(&resolved, lead), vec![v2]);
}

/// A pinned edge resolves to exactly that instance and follows no root
#[tokio::test]
async fn pinned_edge() {
    // a lead pinned to the older of two versions
    let mut g = Graph::default();
    let (root, v1) = g.tool("scan", &[MINERVA], agentic("scan"));
    g.publish(root, agentic("scan2"), "2.0.0");
    let lead = g.agent("lead", &[MINERVA]);
    g.edge(AssociationKind::Calls, lead, v1);
    let wf = g.workflow("Triage", &[MINERVA], &[lead], Some(lead));
    // resolve it
    let resolved = resolve(&g, wf).await;
    assert_ready(&resolved);
    assert_eq!(tools_of(&resolved, lead), vec![v1]);
    assert!(resolved.instance.tool(v1).unwrap().pinned);
    assert_eq!(resolved.follows_tools, BTreeSet::new());
}

/// A container backing adds the Image instance it pins as a dependency
#[tokio::test]
async fn container_dependency() {
    // a lead following a container tool that pins an Image instance
    let mut g = Graph::default();
    let (image_root, image_instance) = g.tool("ghidra-cli", &[MINERVA], image(60));
    let (root, instance) = g.tool("ghidra", &[MINERVA], container("ghidra", image_instance));
    let lead = g.agent("lead", &[MINERVA]);
    g.edge(AssociationKind::Calls, lead, root);
    let wf = g.workflow("Triage", &[MINERVA], &[lead], Some(lead));
    // resolve it
    let resolved = resolve(&g, wf).await;
    assert_ready(&resolved);
    assert_eq!(tools_of(&resolved, lead), vec![instance]);
    let dependency = resolved.instance.tool(image_instance).unwrap();
    assert!(dependency.dependency);
    assert!(dependency.pinned);
    assert_eq!(dependency.tool, image_root);
    assert_eq!(dependency.kind, ToolKind::Image);
    assert_eq!(dependency.function_name, None);
    assert_eq!(resolved.instance.tools.len(), 3);
}

/// One function name twice in one agent is invalid, and once in each of two agents is fine
#[tokio::test]
async fn function_name_collisions() {
    // two tools that a model would both call `scan`
    let mut g = Graph::default();
    let (first, _) = g.tool("scan-a", &[MINERVA], agentic("scan"));
    let (second, _) = g.tool("scan-b", &[MINERVA], agentic("scan"));
    let lead = g.agent("lead", &[MINERVA]);
    g.edge(AssociationKind::Calls, lead, first);
    g.edge(AssociationKind::Calls, lead, second);
    let wf = g.workflow("Triage", &[MINERVA], &[lead], Some(lead));
    let resolved = resolve(&g, wf).await;
    assert_invalid(
        &resolved,
        &["lead", "`scan`", "scan-a@1.0.0", "scan-b@1.0.0"],
    );
    // the same two tools split across two agents are fine
    let mut g = Graph::default();
    let (first, _) = g.tool("scan-a", &[MINERVA], agentic("scan"));
    let (second, _) = g.tool("scan-b", &[MINERVA], agentic("scan"));
    let lead = g.agent("lead", &[MINERVA]);
    let helper = g.agent("helper", &[MINERVA]);
    g.edge(AssociationKind::Calls, lead, first);
    g.edge(AssociationKind::Calls, helper, second);
    g.edge(AssociationKind::DelegatesTo, lead, helper);
    let wf = g.workflow("Triage", &[MINERVA], &[lead], Some(lead));
    assert_ready(&resolve(&g, wf).await);
}

/// A latest edge to a tool with no latest instance is invalid, and still follows the tool
#[tokio::test]
async fn latest_without_instance() {
    // a tool whose only instance is deprecated, followed at latest
    let mut g = Graph::default();
    let (root, v1) = g.tool("scan", &[MINERVA], agentic("scan"));
    g.deprecate(root, v1);
    let lead = g.agent("lead", &[MINERVA]);
    g.edge(AssociationKind::Calls, lead, root);
    let wf = g.workflow("Triage", &[MINERVA], &[lead], Some(lead));
    let resolved = resolve(&g, wf).await;
    assert_invalid(&resolved, &["lead", "`scan`", "no latest instance"]);
    assert_eq!(tools_of(&resolved, lead), Vec::<Uuid>::new());
    assert_eq!(resolved.follows_tools, set(&[root]));
}

/// A missing entry agent, an entry agent reached only by delegation, and no agents are invalid
#[tokio::test]
async fn entry_agent_reasons() {
    // no entry agent
    let mut g = Graph::default();
    let lead = g.agent("lead", &[MINERVA]);
    let wf = g.workflow("Triage", &[MINERVA], &[lead], None);
    assert_invalid(&resolve(&g, wf).await, &["Triage", "no entry agent"]);
    // an entry agent that is only a delegation target
    let helper = g.agent("helper", &[MINERVA]);
    g.edge(AssociationKind::DelegatesTo, lead, helper);
    let wf = g.workflow("Triage", &[MINERVA], &[lead], Some(helper));
    let resolved = resolve(&g, wf).await;
    assert!(resolved.instance.agent(helper).is_some());
    assert_invalid(
        &resolved,
        &[
            &format!("entry agent `{helper}`"),
            "not one of workflow `Triage`'s agents",
        ],
    );
    // a workflow with no agents
    let wf = g.workflow("Empty", &[MINERVA], &[], Some(lead));
    assert_invalid(&resolve(&g, wf).await, &["Empty", "no agents"]);
}

/// The crawl limits make a workflow invalid just past them, and lower limits apply
#[tokio::test]
async fn crawl_limits() {
    // 32 agents are allowed and 33 are not
    let limits = CrawlLimits::default();
    for (count, ready) in [(32, true), (33, false)] {
        let mut g = Graph::default();
        let agents = (0..count)
            .map(|i| g.agent(&format!("agent-{i}"), &[MINERVA]))
            .collect::<Vec<Uuid>>();
        let wf = g.workflow("Wide", &[MINERVA], &agents, Some(agents[0]));
        let resolved = resolve(&g, wf).await;
        if ready {
            assert_ready(&resolved);
        } else {
            assert_invalid(
                &resolved,
                &["Wide", "33 agents", "max_agents_per_workflow (32)"],
            );
            assert_eq!(resolved.instance.agents.len(), 32);
        }
    }
    // an agent with 128 tools is allowed and with 129 is not
    for (count, ready) in [(128, true), (129, false)] {
        let mut g = Graph::default();
        let lead = g.agent("lead", &[MINERVA]);
        for i in 0..count {
            let (root, _) = g.tool(
                &format!("tool-{i}"),
                &[MINERVA],
                agentic(&format!("tool_{i}")),
            );
            g.edge(AssociationKind::Calls, lead, root);
        }
        let wf = g.workflow("Tooled", &[MINERVA], &[lead], Some(lead));
        let resolved = resolve(&g, wf).await;
        if ready {
            assert_ready(&resolved);
            assert_eq!(tools_of(&resolved, lead).len(), 128);
        } else {
            assert_invalid(
                &resolved,
                &["lead", "129 tools", "max_tools_per_agent (128)"],
            );
        }
    }
    // a delegation chain 8 hops deep is allowed and 9 hops deep is not
    for (hops, ready) in [(8, true), (9, false)] {
        let mut g = Graph::default();
        let chain = (0..=hops)
            .map(|i| g.agent(&format!("hop-{i}"), &[MINERVA]))
            .collect::<Vec<Uuid>>();
        for pair in chain.windows(2) {
            g.edge(AssociationKind::DelegatesTo, pair[0], pair[1]);
        }
        let wf = g.workflow("Deep", &[MINERVA], &[chain[0]], Some(chain[0]));
        let resolved = resolve(&g, wf).await;
        if ready {
            assert_ready(&resolved);
            assert_eq!(resolved.instance.agents.len(), 9);
        } else {
            assert_invalid(&resolved, &["9 DelegatesTo hops", "max_crawl_depth (8)"]);
            assert_eq!(resolved.instance.agents.len(), 9);
            assert!(resolved.instance.agent(chain[9]).is_none());
            assert_eq!(
                resolved.instance.agent(chain[8]).unwrap().sub_agents,
                Vec::<Uuid>::new()
            );
        }
    }
    // lower configured limits apply
    let lower = CrawlLimits {
        max_agents_per_workflow: 2,
        max_tools_per_agent: 1,
        max_crawl_depth: 1,
    };
    let mut g = Graph::default();
    let lead = g.agent("lead", &[MINERVA]);
    let helper = g.agent("helper", &[MINERVA]);
    let deep = g.agent("deep", &[MINERVA]);
    g.edge(AssociationKind::DelegatesTo, lead, helper);
    g.edge(AssociationKind::DelegatesTo, helper, deep);
    for i in 0..2 {
        let (root, _) = g.tool(
            &format!("tool-{i}"),
            &[MINERVA],
            agentic(&format!("tool_{i}")),
        );
        g.edge(AssociationKind::Calls, lead, root);
    }
    let wf = g.workflow("Small", &[MINERVA], &[lead], Some(lead));
    assert_ready(&resolve_with(&g, wf, limits).await);
    let resolved = resolve_with(&g, wf, lower).await;
    assert_invalid(&resolved, &["2 tools", "max_tools_per_agent (1)"]);
    assert_invalid(&resolved, &["2 DelegatesTo hops", "max_crawl_depth (1)"]);
    let mut g = Graph::default();
    let agents = (0..3)
        .map(|i| g.agent(&format!("agent-{i}"), &[MINERVA]))
        .collect::<Vec<Uuid>>();
    let wf = g.workflow("Three", &[MINERVA], &agents, Some(agents[0]));
    assert_invalid(
        &resolve_with(&g, wf, lower).await,
        &["3 agents", "max_agents_per_workflow (2)"],
    );
}

/// The resolver only asks for forward rows, and ignores reverse rows a reader returns anyway
#[tokio::test]
async fn crawl_direction() {
    // a reverse calls row on the lead names an agent outside the workflow
    let mut g = Graph::default();
    let lead = g.agent("lead", &[MINERVA]);
    let outsider = g.agent("outsider", &[MINERVA]);
    g.reader.row(
        lead,
        GraphEdge {
            kind: AssociationKind::Calls,
            direction: Directionality::From,
            other: outsider,
            other_kind: EntityKinds::Agent,
        },
    );
    g.reader.row(
        lead,
        GraphEdge {
            kind: AssociationKind::DelegatesTo,
            direction: Directionality::From,
            other: outsider,
            other_kind: EntityKinds::Agent,
        },
    );
    let wf = g.workflow("Triage", &[MINERVA], &[lead], Some(lead));
    // every read asks for forward rows only
    // resolve it
    let resolved = resolve(&g, wf).await;
    assert_ready(&resolved);
    assert!(g.reader.reads().iter().all(|read| match read {
        GraphRead::Node { direction, .. } => *direction == Directionality::To,
        GraphRead::Tagged { .. } => true,
    }));
    // a reader that returns every row anyway still doesn't pull the outsider in
    g.reader.return_every_direction(true);
    // resolve it
    let resolved = resolve(&g, wf).await;
    assert_ready(&resolved);
    assert!(resolved.instance.agent(outsider).is_none());
    assert_eq!(g.reader.node_reads(outsider), 0);
}

/// An agent shared with another workflow doesn't pull in that workflow or its other agents
#[tokio::test]
async fn crawl_scope() {
    // one agent in two workflows, the second with another agent
    let mut g = Graph::default();
    let shared = g.agent("shared", &[MINERVA]);
    let other = g.agent("other", &[MINERVA]);
    let wf = g.workflow("Triage", &[MINERVA], &[shared], Some(shared));
    let second = g.workflow("Hunt", &[MINERVA], &[shared, other], Some(other));
    // resolve it
    let resolved = resolve(&g, wf).await;
    assert_ready(&resolved);
    assert_eq!(resolved.instance.agents.len(), 1);
    assert_eq!(g.reader.node_reads(second), 0);
    assert_eq!(g.reader.node_reads(other), 0);
}

/// Lineage and unrelated edge kinds are never followed or asked for
#[tokio::test]
async fn crawl_kinds() {
    // a pinned tool, an unrelated agent, and a lineage edge from the workflow
    let mut g = Graph::default();
    let (root, instance) = g.tool("scan", &[MINERVA], agentic("scan"));
    let lead = g.agent("lead", &[MINERVA]);
    let related = g.agent("related", &[MINERVA]);
    g.edge(AssociationKind::Calls, lead, instance);
    g.edge(AssociationKind::AssociatedWith, lead, related);
    let wf = g.workflow("Triage", &[MINERVA], &[lead], Some(lead));
    let old = g.put(
        EntityKinds::WorkflowCatalog,
        "Old",
        &[MINERVA],
        EntityMetadata::WorkflowCatalog(WorkflowCatalogEntity::default()),
    );
    g.edge(AssociationKind::BuiltFrom, wf, old);
    g.edge(AssociationKind::AssociatedWith, wf, related);
    // resolve it
    let resolved = resolve(&g, wf).await;
    assert_ready(&resolved);
    // the related agent, the old catalog, and the pinned instance's tool are never read
    assert!(resolved.instance.agent(related).is_none());
    assert_eq!(g.reader.node_reads(related), 0);
    assert_eq!(g.reader.node_reads(old), 0);
    assert_eq!(g.reader.node_reads(root), 0);
    // no read asks for lineage or unrelated kinds, and only the entry agent's harness instance
    // is asked for the native tools it provides
    let harness = g.given_harness(lead).unwrap();
    for read in g.reader.reads() {
        if let GraphRead::Node { id, kinds, .. } = read {
            assert!(kinds.iter().all(|kind| matches!(
                kind,
                AssociationKind::Orchestrates
                    | AssociationKind::DelegatesTo
                    | AssociationKind::Calls
                    | AssociationKind::RunsIn
                    | AssociationKind::Provides
            )));
            assert!(!kinds.contains(&AssociationKind::Provides) || id == harness);
        }
    }
}

/// A delegation cycle visits each agent once and terminates, and self delegation is allowed
#[tokio::test]
async fn delegation_cycle() {
    // two agents delegating to each other, one also to itself
    let mut g = Graph::default();
    let a = g.agent("a", &[MINERVA]);
    let b = g.agent("b", &[MINERVA]);
    g.edge(AssociationKind::DelegatesTo, a, b);
    g.edge(AssociationKind::DelegatesTo, b, a);
    g.edge(AssociationKind::DelegatesTo, b, b);
    let wf = g.workflow("Loop", &[MINERVA], &[a], Some(a));
    // resolve it
    let resolved = resolve(&g, wf).await;
    assert_ready(&resolved);
    assert_eq!(resolved.instance.agents.len(), 2);
    assert_eq!(g.reader.node_reads(a), 1);
    assert_eq!(g.reader.node_reads(b), 1);
    let mut both = vec![a, b];
    both.sort();
    assert_eq!(resolved.instance.agent(b).unwrap().sub_agents, both);
}

/// A node whose groups don't cover its source's makes the instance invalid, naming both
#[tokio::test]
async fn containment() {
    // an agent missing one of the workflow's groups
    let mut g = Graph::default();
    let lead = g.agent("lead", &[MINERVA]);
    let wf = g.workflow("Triage", &[MINERVA, MARS], &[lead], Some(lead));
    assert_invalid(&resolve(&g, wf).await, &["`lead`", MARS, "`Triage`"]);
    // a tool missing one of its agent's groups
    let mut g = Graph::default();
    let (root, _) = g.tool("scan", &[MINERVA], agentic("scan"));
    let lead = g.agent("lead", &[MINERVA, MARS]);
    g.edge(AssociationKind::Calls, lead, root);
    let wf = g.workflow("Triage", &[MINERVA], &[lead], Some(lead));
    assert_invalid(&resolve(&g, wf).await, &["`scan@1.0.0`", MARS, "`lead`"]);
    // a sub-agent missing one of its delegator's groups
    let mut g = Graph::default();
    let lead = g.agent("lead", &[MINERVA, MARS]);
    let helper = g.agent("helper", &[MINERVA]);
    g.edge(AssociationKind::DelegatesTo, lead, helper);
    let wf = g.workflow("Triage", &[MINERVA], &[lead], Some(lead));
    assert_invalid(&resolve(&g, wf).await, &["`helper`", MARS, "`lead`"]);
}

/// Default models are snapshotted and hashed, and model auth is neither
#[tokio::test]
async fn default_models_and_model_auth() {
    // a workflow resolved before any model setting changes
    let mut g = Graph::default();
    let lead = g.agent("lead", &[MINERVA]);
    let wf = g.workflow("Triage", &[MINERVA], &[lead], Some(lead));
    let plain = resolve(&g, wf).await;
    // a default model is snapshotted and changes the digest
    let model = ModelMatcher {
        model_id: "gpt-test".to_owned(),
        ..ModelMatcher::default()
    };
    g.change_workflow(wf, |meta| meta.default_models = vec![model.clone()]);
    let with_model = resolve(&g, wf).await;
    assert_eq!(with_model.instance.default_models, vec![model]);
    assert_ne!(with_model.instance.digest, plain.instance.digest);
    // model auth changes neither the digest nor the dedupe key
    g.change_workflow(wf, |meta| {
        meta.model_auth = ModelAuthSource {
            levels: BTreeSet::from([ModelAuthLevel::User]),
        };
    });
    let with_auth = resolve(&g, wf).await;
    assert_eq!(with_auth.instance.digest, with_model.instance.digest);
    assert_eq!(with_auth.dedupe_key(), with_model.dedupe_key());
    // the limits are snapshotted and hashed too
    g.change_workflow(wf, |meta| {
        meta.limits = WorkflowLimits {
            max_alive_sub_agents: Some(4),
        };
    });
    let with_limits = resolve(&g, wf).await;
    assert_eq!(with_limits.instance.limits.max_alive_sub_agents, Some(4));
    assert_ne!(with_limits.instance.digest, with_auth.instance.digest);
}

/// The roots an instance is tagged with: every agent, latest calls and harness roots, and latest
/// toolsets, and none for pinned targets, whether or not the instance is ready
#[tokio::test]
async fn instance_tags() {
    // a lead with a latest tool, a pinned tool, a latest toolset, a latest harness, and a helper
    let mut g = Graph::default();
    let (latest_root, _) = g.tool("scan", &[MINERVA], agentic("scan"));
    let (pinned_root, pinned) = g.tool("hash", &[MINERVA], agentic("hash"));
    let (harness_root, _) = g.tool("harness", &[MINERVA], harness(30));
    let tool_set = g.put(
        EntityKinds::ToolSet,
        "kit",
        &[MINERVA],
        EntityMetadata::ToolSet(ToolSetEntity::default()),
    );
    let lead = g.agent("lead", &[MINERVA]);
    let helper = g.agent("helper", &[MINERVA]);
    g.edge(AssociationKind::Calls, lead, latest_root);
    g.edge(AssociationKind::Calls, lead, pinned);
    g.edge(AssociationKind::Calls, lead, tool_set);
    g.edge(AssociationKind::RunsIn, lead, harness_root);
    g.edge(AssociationKind::DelegatesTo, lead, helper);
    let wf = g.workflow("Triage", &[MINERVA], &[lead], Some(lead));
    let resolved = resolve(&g, wf).await;
    assert_eq!(
        resolved
            .instance
            .agents
            .iter()
            .map(|agent| agent.agent)
            .collect::<BTreeSet<Uuid>>(),
        set(&[lead, helper])
    );
    assert_eq!(resolved.follows_tools, set(&[latest_root, harness_root]));
    assert!(!resolved.follows_tools.contains(&pinned_root));
    assert_eq!(resolved.follows_tool_sets, set(&[tool_set]));
    let harness = resolved.instance.agent(lead).unwrap().harness.unwrap();
    assert!(!tools_of(&resolved, lead).contains(&harness));
    assert!(resolved.instance.tool(harness).is_some());
    // an invalid instance follows the same roots
    g.change_workflow(wf, |meta| meta.entry_agent = None);
    let invalid = resolve(&g, wf).await;
    assert!(!invalid.instance.is_ready());
    assert_eq!(invalid.follows_tools, resolved.follows_tools);
    assert_eq!(invalid.follows_tool_sets, resolved.follows_tool_sets);
}

/// Each tool instance is listed once however many agents use it
#[tokio::test]
async fn one_entry_per_tool_instance() {
    // two agents sharing a tool, and two container tools pinning one Image instance
    let mut g = Graph::default();
    let (shared_root, shared) = g.tool("scan", &[MINERVA], agentic("scan"));
    let (_, image_instance) = g.tool("ghidra-cli", &[MINERVA], image(60));
    let (first, _) = g.tool(
        "decompile",
        &[MINERVA],
        container("decompile", image_instance),
    );
    let (second, _) = g.tool(
        "disassemble",
        &[MINERVA],
        container("disassemble", image_instance),
    );
    let lead = g.agent("lead", &[MINERVA]);
    let helper = g.agent("helper", &[MINERVA]);
    g.edge(AssociationKind::Calls, lead, shared_root);
    g.edge(AssociationKind::Calls, helper, shared_root);
    g.edge(AssociationKind::Calls, lead, first);
    g.edge(AssociationKind::Calls, helper, second);
    g.edge(AssociationKind::DelegatesTo, lead, helper);
    let wf = g.workflow("Triage", &[MINERVA], &[lead], Some(lead));
    // resolve it
    let resolved = resolve(&g, wf).await;
    assert_ready(&resolved);
    let count = |id: Uuid| {
        resolved
            .instance
            .tools
            .iter()
            .filter(|tool| tool.tool_instance == id)
            .count()
    };
    assert_eq!(count(shared), 1);
    assert_eq!(count(image_instance), 1);
    let dependency = resolved.instance.tool(image_instance).unwrap();
    assert!(dependency.dependency);
    assert_eq!(dependency.function_name, None);
    assert_eq!(resolved.instance.tools.len(), 5);
}

/// A sub-agent that several agents delegate to has its edges read once
#[tokio::test]
async fn sub_agent_edges_read_once() {
    // three orchestrated agents that all delegate to one helper
    let mut g = Graph::default();
    let helper = g.agent("helper", &[MINERVA]);
    let delegators = (0..3)
        .map(|i| g.agent(&format!("lead-{i}"), &[MINERVA]))
        .collect::<Vec<Uuid>>();
    for delegator in &delegators {
        g.edge(AssociationKind::DelegatesTo, *delegator, helper);
    }
    let wf = g.workflow("Triage", &[MINERVA], &delegators, Some(delegators[0]));
    // resolve it
    let resolved = resolve(&g, wf).await;
    assert_ready(&resolved);
    assert_eq!(g.reader.node_reads(helper), 1);
    // the agent's one read asks for its graph edges forward
    assert!(g.reader.reads().contains(&GraphRead::Node {
        id: helper,
        kinds: AGENT_EDGES.to_vec(),
        direction: Directionality::To,
    }));
}

/// An agent's own tool limit is applied, tool tags don't filter tools, and no tools is fine
#[tokio::test]
async fn agent_max_tools() {
    // three tools over a limit of two
    let mut g = Graph::default();
    let roots = (0..3)
        .map(|i| {
            g.tool(
                &format!("tool-{i}"),
                &[MINERVA],
                agentic(&format!("tool_{i}")),
            )
            .0
        })
        .collect::<Vec<Uuid>>();
    let config = AgentConfig {
        function_name: Some("lead".to_owned()),
        max_tools: Some(2),
        ..AgentConfig::default()
    };
    let lead = g.agent_with("lead", &[MINERVA], config.clone());
    for root in &roots {
        g.edge(AssociationKind::Calls, lead, *root);
    }
    let wf = g.workflow("Triage", &[MINERVA], &[lead], Some(lead));
    assert_invalid(
        &resolve(&g, wf).await,
        &["`lead`", "3 tools", "max_tools (2)"],
    );
    // exactly two is fine
    g.reader.remove_edge(AssociationKind::Calls, lead, roots[2]);
    let two = resolve(&g, wf).await;
    assert_ready(&two);
    // tool tags change the config hash but not the tools
    let mut tagged = config;
    tagged
        .tool_tags
        .insert("phase".to_owned(), BTreeSet::from(["triage".to_owned()]));
    let tagged_lead = g.agent_with("lead", &[MINERVA], tagged);
    g.edge(AssociationKind::Calls, tagged_lead, roots[0]);
    g.edge(AssociationKind::Calls, tagged_lead, roots[1]);
    let tagged_wf = g.workflow("Tagged", &[MINERVA], &[tagged_lead], Some(tagged_lead));
    let with_tags = resolve(&g, tagged_wf).await;
    assert_ne!(
        with_tags.instance.agents[0].config_hash,
        two.instance.agents[0].config_hash
    );
    assert_eq!(
        with_tags.instance.agents[0].tools,
        two.instance.agents[0].tools
    );
    // an agent with no tools is ready
    let bare = g.agent("bare", &[MINERVA]);
    let bare_wf = g.workflow("Bare", &[MINERVA], &[bare], Some(bare));
    let resolved = resolve(&g, bare_wf).await;
    assert_ready(&resolved);
    assert_eq!(tools_of(&resolved, bare), Vec::<Uuid>::new());
}

/// A pin to a deprecated instance is ready with a warning naming it
#[tokio::test]
async fn deprecated_pins() {
    // a lead pinned to a deprecated older version
    let mut g = Graph::default();
    let (root, v1) = g.tool("scan", &[MINERVA], agentic("scan"));
    g.publish(root, agentic("scan2"), "2.0.0");
    g.deprecate(root, v1);
    let lead = g.agent("lead", &[MINERVA]);
    g.edge(AssociationKind::Calls, lead, v1);
    let wf = g.workflow("Triage", &[MINERVA], &[lead], Some(lead));
    // resolve it
    let resolved = resolve(&g, wf).await;
    assert_ready(&resolved);
    assert_eq!(tools_of(&resolved, lead), vec![v1]);
    assert!(
        resolved
            .instance
            .warnings
            .iter()
            .any(|warning| warning.contains("deprecated") && warning.contains("scan@1.0.0")),
        "{:?}",
        resolved.instance.warnings
    );
}

/// The snapshot records every entity's stored groups, even groups the catalog isn't in
#[tokio::test]
async fn snapshot_groups() {
    // a lead and its tool in two groups, in a workflow in one of them
    let mut g = Graph::default();
    let (root, instance) = g.tool("scan", &[MARS, MINERVA], agentic("scan"));
    let lead = g.agent("lead", &[MARS, MINERVA]);
    g.edge(AssociationKind::Calls, lead, root);
    let wf = g.workflow("Triage", &[MINERVA], &[lead], Some(lead));
    // resolve it
    let resolved = resolve(&g, wf).await;
    assert_ready(&resolved);
    let mut both = vec![MARS.to_owned(), MINERVA.to_owned()];
    both.sort();
    assert_eq!(resolved.instance.workflow_groups, vec![MINERVA.to_owned()]);
    assert_eq!(resolved.instance.agent(lead).unwrap().groups, both);
    assert_eq!(resolved.instance.tool(instance).unwrap().groups, both);
}

/// Instanced tools order by tool and then tool instance, and sorting is stable
#[test]
fn instanced_tool_order() {
    // two instances of one tool and one instance of another, listed out of order
    let (low, high) = {
        let mut pair = [Uuid::new_v4(), Uuid::new_v4()];
        pair.sort();
        (pair[0], pair[1])
    };
    let build = |tool: Uuid, tool_instance: Uuid, name: &str| InstancedTool {
        tool,
        tool_instance,
        name: name.to_owned(),
        groups: vec![MINERVA.to_owned()],
        kind: ToolKind::Agentic,
        version: "1.0.0".to_owned(),
        spec_hash: "v1:aa".to_owned(),
        function_name: None,
        pinned: false,
        dependency: false,
    };
    let mut tools = vec![
        build(high, low, "c"),
        build(low, high, "b"),
        build(low, low, "a"),
    ];
    tools.sort();
    let names = tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<Vec<&str>>();
    assert_eq!(names, ["a", "b", "c"]);
    // sorting an already sorted list keeps its order
    let again = {
        let mut copy = tools.clone();
        copy.sort();
        copy
    };
    assert_eq!(again, tools);
}

/// The same graph read in two edge orders serializes to identical bytes, with every list sorted
#[tokio::test]
async fn output_order() {
    // a lead delegating to four helpers that each call three tools
    let mut g = Graph::default();
    let lead = g.agent("lead", &[MINERVA]);
    let helpers = (0..4)
        .map(|i| g.agent(&format!("helper-{i}"), &[MINERVA]))
        .collect::<Vec<Uuid>>();
    for helper in &helpers {
        g.edge(AssociationKind::DelegatesTo, lead, *helper);
        for i in 0..3 {
            let name = format!("{helper}-{i}");
            let (root, _) = g.tool(&name, &[MINERVA], agentic(&format!("tool_{i}")));
            g.edge(AssociationKind::Calls, *helper, root);
        }
    }
    let wf = g.workflow("Triage", &[MINERVA], &[lead], Some(lead));
    let first = resolve(&g, wf).await;
    g.reader.reverse_row_order();
    let second = resolve(&g, wf).await;
    assert_ready(&first);
    assert_eq!(
        serde_json::to_vec(&first.instance).unwrap(),
        serde_json::to_vec(&second.instance).unwrap()
    );
    // every list is in its canonical order
    let instance = &first.instance;
    assert!(
        instance
            .agents
            .windows(2)
            .all(|pair| pair[0].agent < pair[1].agent)
    );
    assert!(instance.tools.windows(2).all(|pair| pair[0] <= pair[1]));
    for agent in &instance.agents {
        assert!(agent.tools.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(agent.sub_agents.windows(2).all(|pair| pair[0] < pair[1]));
    }
}

/// Build a two-agent workflow with a latest tool, a pinned tool, and a container tool
///
/// Returns the graph, the workflow, the entry agent, and the sub-agent.
///
/// # Arguments
///
/// * `prompt` - The entry agent's prompt, which tells otherwise equal graphs apart
fn sample(prompt: &str) -> (Graph, Uuid, Uuid, Uuid) {
    // the tools: one followed at latest, one pinned, and one backed by an Image instance
    let mut g = Graph::default();
    let (latest_root, _) = g.tool("scan", &[MINERVA], agentic("scan"));
    let (_, pinned) = g.tool("hash", &[MINERVA], agentic("hash"));
    let (_, image_instance) = g.tool("ghidra-cli", &[MINERVA], image(60));
    let (backed, _) = g.tool("ghidra", &[MINERVA], container("ghidra", image_instance));
    let config = AgentConfig {
        prompt: Some(prompt.to_owned()),
        function_name: Some("lead".to_owned()),
        ..AgentConfig::default()
    };
    let lead = g.agent_with("lead", &[MINERVA], config);
    let helper = g.agent("helper", &[MINERVA]);
    g.edge(AssociationKind::Calls, lead, latest_root);
    g.edge(AssociationKind::Calls, lead, pinned);
    g.edge(AssociationKind::Calls, helper, backed);
    g.edge(AssociationKind::DelegatesTo, lead, helper);
    let wf = g.workflow("Triage", &[MINERVA], &[lead], Some(lead));
    (g, wf, lead, helper)
}

/// The workflow digest only covers content, so rebuilding a graph with new ids keeps it
#[tokio::test]
async fn digest_ignores_ids() {
    // build the same content twice, with new ids
    let (first, first_wf, _, _) = sample("triage");
    let (second, second_wf, _, _) = sample("triage");
    let one = resolve(&first, first_wf).await;
    let two = resolve(&second, second_wf).await;
    assert_ready(&one);
    assert_eq!(one.instance.digest, two.instance.digest);
    assert_ne!(one.referenced_ids(), two.referenced_ids());
    assert!(one.instance.digest.starts_with("v1:"));
}

/// Two identical agents hash differently from one
#[tokio::test]
async fn digest_counts_identical_agents() {
    // one workflow with one twin agent and another with both twins
    let mut g = Graph::default();
    let first = g.agent("twin", &[MINERVA]);
    let second = g.agent("twin", &[MINERVA]);
    let one = g.workflow("One", &[MINERVA], &[first], Some(first));
    let two = g.workflow("Two", &[MINERVA], &[first, second], Some(first));
    let single = resolve(&g, one).await;
    let double = resolve(&g, two).await;
    assert_ready(&double);
    assert_eq!(double.instance.agents.len(), 2);
    assert_ne!(single.instance.digest, double.instance.digest);
}

/// The workflow digest changes with a delegation, the entry agent, a tool, the harness, a
/// dependency, and the limits
#[tokio::test]
async fn digest_changes() {
    // start from the sample graph's digest
    let (mut g, wf, lead, helper) = sample("triage");
    let base = resolve(&g, wf).await.instance.digest;
    let mut seen = vec![base.clone()];
    // a new delegation from the helper back to the lead
    g.edge(AssociationKind::DelegatesTo, helper, lead);
    let changed = resolve(&g, wf).await.instance.digest;
    assert!(!seen.contains(&changed));
    seen.push(changed);
    g.reader
        .remove_edge(AssociationKind::DelegatesTo, helper, lead);
    assert_eq!(resolve(&g, wf).await.instance.digest, base);
    // the helper as the entry agent
    g.edge(AssociationKind::Orchestrates, wf, helper);
    let both = resolve(&g, wf).await.instance.digest;
    g.change_workflow(wf, |meta| meta.entry_agent = Some(helper));
    let changed = resolve(&g, wf).await.instance.digest;
    assert_ne!(changed, both);
    g.change_workflow(wf, |meta| meta.entry_agent = Some(lead));
    g.reader
        .remove_edge(AssociationKind::Orchestrates, wf, helper);
    // a new spec for the tool the lead follows at latest
    let resolved = resolve(&g, wf).await;
    let scan = resolved
        .instance
        .tools
        .iter()
        .find(|tool| tool.name == "scan@1.0.0")
        .unwrap()
        .tool;
    g.publish(scan, agentic("scan_v2"), "2.0.0");
    let changed = resolve(&g, wf).await.instance.digest;
    assert!(!seen.contains(&changed));
    seen.push(changed);
    // a harness followed at latest in place of the pinned one, then a new harness spec
    let given = g.given_harness(lead).unwrap();
    g.reader.remove_edge(AssociationKind::RunsIn, lead, given);
    let (harness_root, _) = g.tool("harness", &[MINERVA], harness(30));
    g.edge(AssociationKind::RunsIn, lead, harness_root);
    let with_harness = resolve(&g, wf).await.instance.digest;
    assert!(!seen.contains(&with_harness));
    seen.push(with_harness);
    g.publish(harness_root, harness(31), "2.0.0");
    let changed = resolve(&g, wf).await.instance.digest;
    assert!(!seen.contains(&changed));
    seen.push(changed);
    // a dependency whose spec changes, through a new container tool pinning it
    let (_, image_v1) = g.tool("ghidra-cli-2", &[MINERVA], image(90));
    let (backed, _) = g.tool("ghidra-2", &[MINERVA], container("ghidra_two", image_v1));
    g.edge(AssociationKind::Calls, helper, backed);
    let with_dependency = resolve(&g, wf).await.instance.digest;
    assert!(!seen.contains(&with_dependency));
    seen.push(with_dependency);
    // the limits
    g.change_workflow(wf, |meta| meta.limits.max_alive_sub_agents = Some(4));
    let changed = resolve(&g, wf).await.instance.digest;
    assert!(!seen.contains(&changed));
}

/// The dedupe key is the digest plus the referenced ids
#[tokio::test]
async fn dedupe_key() {
    // a rebuild of the same graph has the same key
    let (mut g, wf, lead, helper) = sample("triage");
    let first = resolve(&g, wf).await;
    assert_eq!(resolve(&g, wf).await.dedupe_key(), first.dedupe_key());
    // the referenced ids name the agents, followed roots, and pinned instances
    let referenced = first.referenced_ids();
    assert!(referenced.contains(&lead));
    assert!(referenced.contains(&helper));
    for tool in &first.instance.tools {
        if tool.pinned {
            assert!(referenced.contains(&tool.tool_instance));
        } else {
            assert!(referenced.contains(&tool.tool));
            assert!(!referenced.contains(&tool.tool_instance));
        }
    }
    // replacing the sub-agent with an identical clone keeps the digest but not the ids
    let clone = g.agent("helper", &[MINERVA]);
    let backed = first
        .instance
        .tools
        .iter()
        .find(|tool| tool.name == "ghidra@1.0.0")
        .unwrap()
        .tool;
    g.edge(AssociationKind::Calls, clone, backed);
    g.reader
        .remove_edge(AssociationKind::DelegatesTo, lead, helper);
    g.edge(AssociationKind::DelegatesTo, lead, clone);
    let cloned = resolve(&g, wf).await;
    assert_eq!(cloned.instance.digest, first.instance.digest);
    assert_ne!(cloned.dedupe_key(), first.dedupe_key());
    assert!(cloned.referenced_ids().contains(&clone));
    assert!(!cloned.referenced_ids().contains(&helper));
    // switching from following the scan tool at latest to pinning its latest instance
    let scan = first
        .instance
        .tools
        .iter()
        .find(|tool| tool.name == "scan@1.0.0")
        .unwrap()
        .clone();
    g.reader
        .remove_edge(AssociationKind::Calls, lead, scan.tool);
    g.edge(AssociationKind::Calls, lead, scan.tool_instance);
    let pinned = resolve(&g, wf).await;
    assert_eq!(pinned.instance.digest, cloned.instance.digest);
    assert_ne!(pinned.dedupe_key(), cloned.dedupe_key());
    assert!(!pinned.follows_tools.contains(&scan.tool));
    assert!(pinned.referenced_ids().contains(&scan.tool_instance));
}

/// Splitting a shared sub-agent into two identical copies makes a new key naming both
#[tokio::test]
async fn dedupe_key_split_sub_agent() {
    // two leads sharing one helper
    let mut g = Graph::default();
    let first = g.agent("lead-a", &[MINERVA]);
    let second = g.agent("lead-b", &[MINERVA]);
    let shared = g.agent("helper", &[MINERVA]);
    g.edge(AssociationKind::DelegatesTo, first, shared);
    g.edge(AssociationKind::DelegatesTo, second, shared);
    let wf = g.workflow("Triage", &[MINERVA], &[first, second], Some(first));
    let before = resolve(&g, wf).await;
    // give the second lead its own identical copy
    let copy = g.agent("helper", &[MINERVA]);
    g.reader
        .remove_edge(AssociationKind::DelegatesTo, second, shared);
    g.edge(AssociationKind::DelegatesTo, second, copy);
    let after = resolve(&g, wf).await;
    assert_ne!(after.dedupe_key(), before.dedupe_key());
    assert!(after.referenced_ids().contains(&shared));
    assert!(after.referenced_ids().contains(&copy));
}

/// A stored instance's key comes from its tags and snapshot and equals its resolved key
#[tokio::test]
async fn stored_dedupe_key() {
    // store the resolved instance as the API would
    let (mut g, wf, _, _) = sample("triage");
    let resolved = resolve(&g, wf).await;
    let id = g.store(&resolved, 1);
    let stored = DedupeKey::of_stored(g.reader.entity(id).unwrap()).unwrap();
    assert_eq!(stored, resolved.dedupe_key());
}

/// A build is skipped when the newest instance has the same key, and otherwise takes the next
/// revision and supersedes the newest instance
#[tokio::test]
async fn build_skips_current() {
    // the sample graph with no instance yet
    let (mut g, wf, lead, _) = sample("triage");
    let limits = CrawlLimits::default();
    // with no instance the build is new at revision 1
    let WorkflowBuild::New(first) = build_workflow_instance(
        &g.reader,
        wf,
        &limits,
        WorkflowInstanceTrigger::Manual,
        "io",
    )
    .await
    .unwrap() else {
        panic!("the first build should be new");
    };
    assert_eq!(first.instance.revision, 1);
    assert_eq!(first.instance.previous, None);
    assert_eq!(first.instance.triggered_by, "io");
    let r1 = g.store(&first, 1);
    // the same graph is current
    let trigger = WorkflowInstanceTrigger::AgentUpdated { agent: lead };
    let build = build_workflow_instance(&g.reader, wf, &limits, trigger.clone(), "europa")
        .await
        .unwrap();
    assert!(matches!(build, WorkflowBuild::Current { newest, .. } if newest == r1));
    // a changed graph is new, after the highest revision ever given out
    g.change_workflow(wf, |meta| {
        meta.limits.max_alive_sub_agents = Some(2);
        meta.highest_revision = 3;
    });
    let WorkflowBuild::New(next) =
        build_workflow_instance(&g.reader, wf, &limits, trigger.clone(), "europa")
            .await
            .unwrap()
    else {
        panic!("a changed graph should build a new instance");
    };
    assert_eq!(next.instance.revision, 4);
    assert_eq!(next.instance.previous, Some(r1));
    assert_eq!(next.instance.trigger, trigger);
    assert_eq!(next.instance.triggered_by, "europa");
    // the newest instance is the one with the highest revision, whatever its status
    let r4 = g.store(&next, 4);
    let workflow = g.reader.entity(wf).unwrap().clone();
    let newest = newest_instance(&g.reader, &workflow)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(newest.id, r4);
}

/// A skipped build reads only the crawl and the newest instance
#[tokio::test]
async fn skipped_build_cost() {
    // the sample graph with its current instance stored
    let (mut g, wf, _, _) = sample("triage");
    let limits = CrawlLimits::default();
    let resolved = resolve(&g, wf).await;
    g.store(&resolved, 1);
    // the reads of a crawl alone
    g.reader.clear_reads();
    resolve(&g, wf).await;
    let crawl = g.reader.reads();
    // a skipped build adds only the read of the newest instance the workflow names
    g.reader.clear_reads();
    let build = build_workflow_instance(
        &g.reader,
        wf,
        &limits,
        WorkflowInstanceTrigger::Manual,
        "io",
    )
    .await
    .unwrap();
    assert!(matches!(build, WorkflowBuild::Current { .. }));
    let reads = g.reader.reads();
    assert_eq!(reads.len(), crawl.len() + 1);
    assert_eq!(&reads[..crawl.len()], crawl.as_slice());
    let newest = match &g.reader.entity(wf).unwrap().metadata {
        EntityMetadata::WorkflowCatalog(meta) => meta.newest_instance.unwrap(),
        _ => panic!("not a workflow"),
    };
    assert_eq!(
        reads.last().unwrap(),
        &GraphRead::Node {
            id: newest,
            kinds: Vec::new(),
            direction: Directionality::To,
        }
    );
}

/// A build reads only the newest instance however many revisions the workflow has
#[tokio::test]
async fn build_cost_flat_over_revisions() {
    // the sample graph with fifty stored revisions of its current instance
    let (mut g, wf, _, _) = sample("triage");
    let resolved = resolve(&g, wf).await;
    for revision in 1..=50 {
        g.store(&resolved, revision);
    }
    // the build reads the crawl and one instance
    g.reader.clear_reads();
    resolve(&g, wf).await;
    let crawl = g.reader.reads().len();
    g.reader.clear_reads();
    let WorkflowBuild::Current { newest, .. } = build(&g, wf).await else {
        panic!("the unchanged graph should be current");
    };
    assert_eq!(g.reader.reads().len(), crawl + 1);
    assert_eq!(g.reader.node_reads(newest), 1);
}

/// A newest instance digested under an older scheme is recomputed, and left as stored
#[tokio::test]
async fn dedupe_older_scheme() {
    // the sample graph with its current instance stored under the current scheme
    let (mut g, wf, _, _) = sample("triage");
    let resolved = resolve(&g, wf).await;
    let id = g.store(&resolved, 1);
    let stored = g.reader.entity(id).unwrap().clone();
    // the candidate is digested under a newer scheme
    let mut candidate = resolved.dedupe_key();
    candidate.digest = resolved
        .instance
        .compute_digest_with_scheme("v2:", &std::collections::BTreeMap::new())
        .unwrap();
    assert!(candidate.digest.starts_with("v2:"));
    assert!(
        is_current_with_scheme(&g.reader, "v2:", &candidate, &stored)
            .await
            .unwrap()
    );
    // a different candidate under the newer scheme isn't current
    let mut other = candidate.clone();
    other.digest = "v2:00".to_owned();
    assert!(
        !is_current_with_scheme(&g.reader, "v2:", &other, &stored)
            .await
            .unwrap()
    );
    // the stored instance keeps its older digest
    match &g.reader.entity(id).unwrap().metadata {
        EntityMetadata::WorkflowInstance(meta) => assert!(meta.digest.starts_with("v1:")),
        _ => panic!("not an instance"),
    }
}

/// Resolving the largest workflow the limits allow, and graphs far past them, stays fast and
/// reads each entity at most once
#[tokio::test]
async fn worst_case_under_limits() {
    // 32 agents that each delegate to every agent and call 128 distinct container tools
    let limits = CrawlLimits::default();
    let mut g = Graph::default();
    let agents = (0..limits.max_agents_per_workflow)
        .map(|i| g.agent(&format!("agent-{i}"), &[MINERVA, MARS]))
        .collect::<Vec<Uuid>>();
    for (i, agent) in agents.iter().enumerate() {
        for target in &agents {
            g.edge(AssociationKind::DelegatesTo, *agent, *target);
        }
        for t in 0..limits.max_tools_per_agent {
            let (_, image_instance) = g.tool(
                &format!("image-{i}-{t}"),
                &[MINERVA, MARS],
                image(u64::try_from(t).unwrap()),
            );
            let (root, _) = g.tool(
                &format!("tool-{i}-{t}"),
                &[MINERVA, MARS],
                container(&format!("tool_{t}"), image_instance),
            );
            g.edge(AssociationKind::Calls, *agent, root);
        }
    }
    let wf = g.workflow("Huge", &[MINERVA], &agents, Some(agents[0]));
    let start = Instant::now();
    let resolved = resolve(&g, wf).await;
    let elapsed = start.elapsed();
    assert_ready(&resolved);
    assert_eq!(resolved.instance.agents.len(), 32);
    assert_eq!(resolved.instance.tools.len(), 32 * 128 * 2 + 1);
    assert!(elapsed.as_secs_f64() < 5.0, "resolving took {elapsed:?}");
    for agent in &agents {
        assert_eq!(g.reader.node_reads(*agent), 1);
    }
    println!("largest allowed workflow resolved in {elapsed:?}");
    // an agent with a hundred thousand tool and delegation edges is refused without reading them
    let mut g = Graph::default();
    let lead = g.agent("lead", &[MINERVA]);
    let (root, _) = g.tool("scan", &[MINERVA], agentic("scan"));
    let helper = g.agent("helper", &[MINERVA]);
    for _ in 0..100_000 {
        g.reader.row(
            lead,
            GraphEdge {
                kind: AssociationKind::Calls,
                direction: Directionality::To,
                other: Uuid::new_v4(),
                other_kind: EntityKinds::Tool,
            },
        );
        g.reader.row(
            lead,
            GraphEdge {
                kind: AssociationKind::DelegatesTo,
                direction: Directionality::To,
                other: Uuid::new_v4(),
                other_kind: EntityKinds::Agent,
            },
        );
    }
    g.edge(AssociationKind::Calls, lead, root);
    g.edge(AssociationKind::DelegatesTo, lead, helper);
    let wf = g.workflow("Wide", &[MINERVA], &[lead], Some(lead));
    g.reader.clear_reads();
    let start = Instant::now();
    let resolved = resolve(&g, wf).await;
    let elapsed = start.elapsed();
    assert_invalid(&resolved, &["max_tools_per_agent (128)"]);
    assert_invalid(&resolved, &["max_agents_per_workflow (32)"]);
    assert!(g.reader.reads().len() <= 2 + limits.max_agents_per_workflow);
    assert!(elapsed.as_secs_f64() < 5.0, "resolving took {elapsed:?}");
    println!("over-limit agent with 200000 edges resolved in {elapsed:?}");
}

/// The Thorium client's tag lookup sends only the groups it's given, the kind, and the tag
#[test]
fn tag_lookup_sends_only_given_groups() {
    // a lookup in two groups
    let groups = vec![MINERVA.to_owned(), MARS.to_owned()];
    let opts = super::tag_lookup(EntityKinds::WorkflowInstance, "WorkflowId", "abc", &groups);
    assert_eq!(opts.groups, groups);
    assert_eq!(opts.kinds, vec![EntityKinds::WorkflowInstance]);
    assert_eq!(opts.tags.len(), 1);
    assert_eq!(opts.tags["WorkflowId"], vec!["abc".to_owned()]);
}

/// The API's outgoing edges in an entity's metadata become forward graph edges
#[test]
fn outgoing_edges_from_metadata() {
    // an agent with a sub-agent, a latest tool, a pinned tool, a toolset, and a harness
    let mut g = Graph::default();
    let helper = g.agent("helper", &[MINERVA]);
    let (root, instance) = g.tool("scan", &[MINERVA], agentic("scan"));
    let (harness, _) = g.tool("harness", &[MINERVA], harness(30));
    let tool_set = g.put(
        EntityKinds::ToolSet,
        "kit",
        &[MINERVA],
        EntityMetadata::ToolSet(ToolSetEntity::default()),
    );
    let lead = g.agent("lead", &[MINERVA]);
    let wf = g.workflow("Triage", &[MINERVA], &[], Some(lead));
    let read = |id: Uuid| g.reader.entity(id).unwrap().clone();
    let mut entity = read(lead);
    if let EntityMetadata::Agent(meta) = &mut entity.metadata {
        meta.sub_agents.push(read(helper));
        meta.latest_tools.push(read(root));
        meta.pinned_tools.push(read(instance));
        meta.tool_sets.push(read(tool_set));
        meta.harness = Some(Box::new(read(harness)));
    }
    let edges = super::outgoing_edges(&entity);
    let expect = |kind, other, other_kind| GraphEdge {
        kind,
        direction: Directionality::To,
        other,
        other_kind,
    };
    assert_eq!(
        edges,
        vec![
            expect(AssociationKind::DelegatesTo, helper, EntityKinds::Agent),
            expect(AssociationKind::Calls, root, EntityKinds::Tool),
            expect(AssociationKind::Calls, instance, EntityKinds::ToolInstance),
            expect(AssociationKind::Calls, tool_set, EntityKinds::ToolSet),
            expect(AssociationKind::RunsIn, harness, EntityKinds::Tool),
        ]
    );
    // a workflow's agents are its orchestrated agents
    let mut workflow = read(wf);
    if let EntityMetadata::WorkflowCatalog(meta) = &mut workflow.metadata {
        meta.agents.push(read(lead));
    }
    assert_eq!(
        super::outgoing_edges(&workflow),
        vec![expect(
            AssociationKind::Orchestrates,
            lead,
            EntityKinds::Agent
        )]
    );
    // a toolset's included tools are its Includes edges
    let mut kit = read(tool_set);
    if let EntityMetadata::ToolSet(meta) = &mut kit.metadata {
        meta.tools.push(read(root));
        meta.tools.push(read(instance));
    }
    assert_eq!(
        super::outgoing_edges(&kit),
        vec![
            expect(AssociationKind::Includes, root, EntityKinds::Tool),
            expect(
                AssociationKind::Includes,
                instance,
                EntityKinds::ToolInstance
            ),
        ]
    );
    // a tool has none
    assert_eq!(super::outgoing_edges(&read(root)), Vec::new());
}

/// Build a lead agent that calls some targets in a one-agent workflow
///
/// Returns the workflow and the lead.
///
/// # Arguments
///
/// * `g` - The graph to add to
/// * `targets` - What the lead calls
fn calling(g: &mut Graph, targets: &[Uuid]) -> (Uuid, Uuid) {
    // a lead calling each target, orchestrated by its own workflow
    let lead = g.agent("lead", &[MINERVA]);
    for target in targets {
        g.edge(AssociationKind::Calls, lead, *target);
    }
    let wf = g.workflow("Triage", &[MINERVA], &[lead], Some(lead));
    (wf, lead)
}

/// Sort a list of ids
///
/// # Arguments
///
/// * `ids` - The ids to sort
fn sorted_ids(ids: &[Uuid]) -> Vec<Uuid> {
    // copy and sort
    let mut sorted = ids.to_vec();
    sorted.sort();
    sorted
}

/// An agent's tools are its own tools plus its toolsets' members, each tool instance once; one
/// tool at two instances makes the instance invalid, naming both sources
#[tokio::test]
async fn effective_tools() {
    // a lead that follows scan itself and gets scan and hash from a toolset
    let mut g = Graph::default();
    let (scan, scan_v1) = g.tool("scan", &[MINERVA], agentic("scan"));
    let (_, hash_v1) = g.tool("hash", &[MINERVA], agentic("hash"));
    let ts = g.tool_set("kit", &[MINERVA], false, None);
    let tsi = g.tool_set_instance(
        ts,
        &[
            (scan_v1, ToolSetInclusion::ExplicitLatest),
            (hash_v1, ToolSetInclusion::ExplicitPinned),
        ],
        true,
    );
    let (wf, lead) = calling(&mut g, &[scan, ts]);
    // the same scan instance from both sources is listed once
    let resolved = resolve(&g, wf).await;
    assert_ready(&resolved);
    assert_eq!(tools_of(&resolved, lead), sorted_ids(&[scan_v1, hash_v1]));
    assert_eq!(resolved.instance.tools.len(), 3);
    assert_eq!(resolved.instance.agent(lead).unwrap().tool_sets, vec![tsi]);
    // the lead's own edge moves to scan 2.0.0 while the toolset keeps 1.0.0
    g.publish(scan, agentic("scan"), "2.0.0");
    let resolved = resolve(&g, wf).await;
    assert_invalid(
        &resolved,
        &[
            "agent `lead`",
            "two instances",
            "`scan@1.0.0` from toolset instance `kit#r1`",
            "`scan@2.0.0` from its own Calls edge",
        ],
    );
    // two toolsets giving one tool at two instances name both toolset instances
    let mut g = Graph::default();
    let (scan, scan_v1) = g.tool("scan", &[MINERVA], agentic("scan"));
    let scan_v2 = g.publish(scan, agentic("scan"), "2.0.0");
    let first = g.tool_set("first", &[MINERVA], false, None);
    g.tool_set_instance(first, &[(scan_v1, ToolSetInclusion::ExplicitPinned)], true);
    let second = g.tool_set("second", &[MINERVA], false, None);
    g.tool_set_instance(second, &[(scan_v2, ToolSetInclusion::Selector)], true);
    let (wf, _) = calling(&mut g, &[first, second]);
    let resolved = resolve(&g, wf).await;
    assert_invalid(
        &resolved,
        &[
            "`scan@1.0.0` from toolset instance `first#r1`",
            "`scan@2.0.0` from toolset instance `second#r1`",
        ],
    );
}

/// A toolset edge needs a ready toolset instance: a latest edge to a toolset with none, or a
/// pin to one that isn't ready or doesn't exist, makes the instance invalid
#[tokio::test]
async fn toolset_needs_ready_instance() {
    // a lead that follows a toolset with no instance yet
    let mut g = Graph::default();
    let (_, scan_v1) = g.tool("scan", &[MINERVA], agentic("scan"));
    let ts = g.tool_set("kit", &[MINERVA], false, None);
    let (wf, lead) = calling(&mut g, &[ts]);
    let resolved = resolve(&g, wf).await;
    assert_invalid(
        &resolved,
        &["agent `lead`", "toolset `kit`", "no Ready instance"],
    );
    assert_eq!(
        resolved.instance.agent(lead).unwrap().tool_sets,
        Vec::<Uuid>::new()
    );
    assert_eq!(resolved.follows_tool_sets, set(&[ts]));
    // an invalid instance isn't the toolset's latest, so it's still invalid
    let broken = g.tool_set_instance(ts, &[(scan_v1, ToolSetInclusion::ExplicitPinned)], false);
    assert_invalid(&resolve(&g, wf).await, &["no Ready instance"]);
    // the first ready instance makes it ready
    let ready = g.tool_set_instance(ts, &[(scan_v1, ToolSetInclusion::ExplicitPinned)], true);
    let resolved = resolve(&g, wf).await;
    assert_ready(&resolved);
    assert_eq!(
        resolved.instance.agent(lead).unwrap().tool_sets,
        vec![ready]
    );
    assert_eq!(tools_of(&resolved, lead), vec![scan_v1]);
    // pinning the invalid instance makes it invalid, without listing it
    let (wf, lead) = calling(&mut g, &[broken]);
    let resolved = resolve(&g, wf).await;
    assert_invalid(
        &resolved,
        &["agent `lead`", "kit#r1", "Invalid", "only a Ready"],
    );
    assert_eq!(
        resolved.instance.agent(lead).unwrap().tool_sets,
        Vec::<Uuid>::new()
    );
    assert_eq!(tools_of(&resolved, lead), Vec::<Uuid>::new());
    // pinning a toolset instance that doesn't exist makes it invalid
    let missing = Uuid::new_v4();
    let lead = g.agent("lone", &[MINERVA]);
    g.reader.row(
        lead,
        GraphEdge {
            kind: AssociationKind::Calls,
            direction: Directionality::To,
            other: missing,
            other_kind: EntityKinds::ToolSetInstance,
        },
    );
    let wf = g.workflow("Lone", &[MINERVA], &[lead], Some(lead));
    assert_invalid(
        &resolve(&g, wf).await,
        &["agent `lone`", &missing.to_string(), "doesn't exist"],
    );
}

/// The instance records the toolset instances each agent used, follows only latest toolsets,
/// marks members pinned as their toolset included them, follows no member tool, and its digests
/// change with a new toolset instance
#[tokio::test]
async fn toolset_snapshot() {
    // a lead that follows one toolset and pins an instance of another
    let mut g = Graph::default();
    let (latest_root, latest) = g.tool("scan", &[MINERVA], agentic("scan"));
    let (_, pinned) = g.tool("hash", &[MINERVA], agentic("hash"));
    let (_, selected) = g.tool("decompile", &[MINERVA], agentic("decompile"));
    let (_, other) = g.tool("strings", &[MINERVA], agentic("strings"));
    let followed = g.tool_set("kit", &[MINERVA], false, None);
    let followed_r1 = g.tool_set_instance(
        followed,
        &[
            (latest, ToolSetInclusion::ExplicitLatest),
            (pinned, ToolSetInclusion::ExplicitPinned),
            (selected, ToolSetInclusion::Selector),
        ],
        true,
    );
    let fixed = g.tool_set("fixed", &[MINERVA], false, None);
    let fixed_r1 = g.tool_set_instance(fixed, &[(other, ToolSetInclusion::Selector)], true);
    let (wf, lead) = calling(&mut g, &[followed, fixed_r1]);
    let resolved = resolve(&g, wf).await;
    assert_ready(&resolved);
    let agent = resolved.instance.agent(lead).unwrap();
    assert_eq!(agent.tool_sets, sorted_ids(&[followed_r1, fixed_r1]));
    assert_eq!(agent.tools, sorted_ids(&[latest, pinned, selected, other]));
    // only the latest toolset edge is followed, and no member tool is
    assert_eq!(resolved.follows_tool_sets, set(&[followed]));
    assert!(resolved.follows_tools.is_empty());
    assert!(!resolved.follows_tools.contains(&latest_root));
    let id = g.store(&resolved, 1);
    let stored = g.reader.entity(id).unwrap();
    assert!(!stored.tags.contains_key("InstanceFollowsTool"));
    assert_eq!(stored.tags["InstanceFollowsToolSet"].len(), 1);
    // members are pinned only when their toolset pinned them
    let pinned_of = |tool: Uuid| resolved.instance.tool(tool).unwrap().pinned;
    assert!(!pinned_of(latest));
    assert!(pinned_of(pinned));
    assert!(!pinned_of(selected));
    assert!(!pinned_of(other));
    // the referenced ids hold both toolset instances and the followed toolset
    let referenced = resolved.referenced_ids();
    assert!(referenced.contains(&followed_r1));
    assert!(referenced.contains(&fixed_r1));
    assert!(referenced.contains(&followed));
    // an agent's toolset digest doesn't depend on the order of its tools
    let mut reordered = agent.clone();
    reordered.tools.reverse();
    assert_eq!(
        reordered
            .compute_toolset_digest(&resolved.instance.tools)
            .unwrap(),
        agent.toolset_digest
    );
    // a new toolset instance with the same members but another digest changes the workflow
    // digest and not the agent's toolset digest
    if let Some(EntityMetadata::ToolSet(meta)) = g
        .reader
        .entity_mut(followed)
        .map(|entity| &mut entity.metadata)
    {
        meta.read_only = true;
    }
    g.tool_set_instance(
        followed,
        &[
            (latest, ToolSetInclusion::ExplicitLatest),
            (pinned, ToolSetInclusion::ExplicitPinned),
            (selected, ToolSetInclusion::Selector),
        ],
        true,
    );
    let rebuilt = resolve(&g, wf).await;
    assert_ready(&rebuilt);
    assert_ne!(rebuilt.instance.digest, resolved.instance.digest);
    assert_eq!(
        rebuilt.instance.agent(lead).unwrap().toolset_digest,
        agent.toolset_digest
    );
    // a member with another spec hash changes the agent's toolset digest too
    let (_, newer) = g.tool("hash", &[MINERVA], agentic("hash-v2"));
    g.tool_set_instance(
        followed,
        &[
            (latest, ToolSetInclusion::ExplicitLatest),
            (newer, ToolSetInclusion::ExplicitPinned),
            (selected, ToolSetInclusion::Selector),
        ],
        true,
    );
    let changed = resolve(&g, wf).await;
    assert_ne!(
        changed.instance.agent(lead).unwrap().toolset_digest,
        agent.toolset_digest
    );
    assert_ne!(changed.instance.digest, rebuilt.instance.digest);
}

/// Toolset members count toward an agent's tool limit after merging, and share its function
/// names
#[tokio::test]
async fn merged_tool_limits() {
    // 65 own tools and a toolset of 64 other members: 129 effective tools
    let mut g = Graph::default();
    let own = (0..65)
        .map(|i| {
            g.tool(
                &format!("own-{i}"),
                &[MINERVA],
                agentic(&format!("own_{i}")),
            )
        })
        .collect::<Vec<(Uuid, Uuid)>>();
    let members = (0..64)
        .map(|i| {
            let (_, instance) = g.tool(
                &format!("member-{i}"),
                &[MINERVA],
                agentic(&format!("m_{i}")),
            );
            (instance, ToolSetInclusion::Selector)
        })
        .collect::<Vec<(Uuid, ToolSetInclusion)>>();
    let ts = g.tool_set("kit", &[MINERVA], false, None);
    g.tool_set_instance(ts, &members, true);
    let mut targets = own.iter().map(|(root, _)| *root).collect::<Vec<Uuid>>();
    targets.push(ts);
    let (wf, _) = calling(&mut g, &targets);
    assert_invalid(
        &resolve(&g, wf).await,
        &["agent `lead`", "129 tools", "max_tools_per_agent (128)"],
    );
    // one own tool also a member at the same instance: 128 effective tools
    let mut overlap = members[..63].to_vec();
    overlap.push((own[0].1, ToolSetInclusion::ExplicitLatest));
    g.tool_set_instance(ts, &overlap, true);
    let resolved = resolve(&g, wf).await;
    assert_ready(&resolved);
    assert_eq!(resolved.instance.agents[0].tools.len(), 128);
    // an own tool and a member of another tool with the same function name
    let (_, clash) = g.tool("clash", &[MINERVA], agentic("own_1"));
    let mut clashing = members[..10].to_vec();
    clashing.push((clash, ToolSetInclusion::Selector));
    g.tool_set_instance(ts, &clashing, true);
    assert_invalid(
        &resolve(&g, wf).await,
        &["`own_1`", "own-1@1.0.0", "clash@1.0.0"],
    );
}

/// Toolset members come only from toolset instances: a new include edge or a newly tagged tool
/// changes nothing until a new toolset instance exists, and neither is read
#[tokio::test]
async fn toolset_crawl_scope() {
    // a lead following a selector toolset whose ready instance holds one member
    let mut g = Graph::default();
    let (scan, scan_v1) = g.tool("scan", &[MINERVA], agentic("scan"));
    let ts = g.tool_set(
        "kit",
        &[MINERVA],
        false,
        Some(crate::models::ToolSetSelector::new("toolset", "ghidra")),
    );
    g.edge(AssociationKind::Includes, ts, scan);
    g.tool_set_instance(ts, &[(scan_v1, ToolSetInclusion::ExplicitLatest)], true);
    let (wf, lead) = calling(&mut g, &[ts]);
    let before = resolve(&g, wf).await;
    assert_ready(&before);
    assert_eq!(tools_of(&before, lead), vec![scan_v1]);
    // a new include edge and a newly tagged tool not yet in any toolset instance
    let (hash, _) = g.tool("hash", &[MINERVA], agentic("hash"));
    g.edge(AssociationKind::Includes, ts, hash);
    let (decompile, _) = g.tool("decompile", &[MINERVA], agentic("decompile"));
    g.tag_in(decompile, "toolset", "ghidra", &[MINERVA]);
    g.reader.clear_reads();
    let after = resolve(&g, wf).await;
    assert_eq!(after.instance, before.instance);
    assert_eq!(after.dedupe_key(), before.dedupe_key());
    // no include edge was read and no tag was looked up
    for read in g.reader.reads() {
        match read {
            GraphRead::Node { kinds, .. } => {
                assert!(!kinds.contains(&AssociationKind::Includes), "{kinds:?}");
            }
            GraphRead::Tagged { .. } => panic!("the crawl looked up a tag: {read:?}"),
        }
    }
}

/// A forced function choice must name one of the agent's effective tools or delegation tools
#[tokio::test]
async fn tool_choice_function() {
    // a lead with an own tool, a toolset member, and a sub-agent it can delegate to
    let choose = |g: &mut Graph, function: &str| {
        let (scan, _) = g.tool("scan", &[MINERVA], agentic("scan"));
        let (_, decompile) = g.tool("decompile", &[MINERVA], agentic("decompile"));
        let ts = g.tool_set("kit", &[MINERVA], false, None);
        g.tool_set_instance(ts, &[(decompile, ToolSetInclusion::Selector)], true);
        let config = AgentConfig {
            tool_choice: ToolChoice::Function(function.to_owned()),
            ..AgentConfig::default()
        };
        let lead = g.agent_with("lead", &[MINERVA], config);
        let helper = g.agent("helper", &[MINERVA]);
        g.edge(AssociationKind::Calls, lead, scan);
        g.edge(AssociationKind::Calls, lead, ts);
        g.edge(AssociationKind::DelegatesTo, lead, helper);
        g.workflow("Triage", &[MINERVA], &[lead], Some(lead))
    };
    // its own tool, a member, and a delegation tool are each ready
    for function in ["scan", "decompile", "helper"] {
        let mut g = Graph::default();
        let wf = choose(&mut g, function);
        assert_ready(&resolve(&g, wf).await);
    }
    // a function it doesn't have makes the instance invalid
    let mut g = Graph::default();
    let wf = choose(&mut g, "yara_scan");
    assert_invalid(
        &resolve(&g, wf).await,
        &["agent `lead`", "`yara_scan`", "no tool or delegation tool"],
    );
}

/// A newest instance digested under an older scheme is recomputed with its toolset instances'
/// digests recomputed under the same scheme
#[tokio::test]
async fn dedupe_older_scheme_toolsets() {
    // a lead following a toolset, with the instance stored under the current scheme
    let mut g = Graph::default();
    let (_, scan_v1) = g.tool("scan", &[MINERVA], agentic("scan"));
    let ts = g.tool_set("kit", &[MINERVA], false, None);
    let tsi = g.tool_set_instance(ts, &[(scan_v1, ToolSetInclusion::ExplicitLatest)], true);
    let (wf, lead) = calling(&mut g, &[ts]);
    let resolved = resolve(&g, wf).await;
    assert_ready(&resolved);
    assert_eq!(resolved.instance.agent(lead).unwrap().tool_sets, vec![tsi]);
    let id = g.store(&resolved, 1);
    let stored = g.reader.entity(id).unwrap().clone();
    // the candidate is digested under a newer scheme with each toolset instance's digest as
    // stored, as a fresh resolve takes it, since toolset instances aren't rebuilt by a bump
    let tool_set = match &g.reader.entity(tsi).unwrap().metadata {
        EntityMetadata::ToolSetInstance(meta) => meta.clone(),
        _ => panic!("not a toolset instance"),
    };
    let digests = BTreeMap::from([(tsi, tool_set.digest.clone())]);
    let mut candidate = resolved.dedupe_key();
    candidate.digest = resolved
        .instance
        .compute_digest_with_scheme("v2:", &digests)
        .unwrap();
    assert!(
        is_current_with_scheme(&g.reader, "v2:", &candidate, &stored)
            .await
            .unwrap()
    );
    // the stored toolset instance keeps its older digest
    assert!(tool_set.digest.starts_with("v1:"));
}

/// Resolving agents at the tool limit through toolsets stays fast, and toolset edges or members
/// far past it are refused without reading them
#[tokio::test]
async fn worst_case_toolsets() {
    // 32 agents that each follow a toolset of 64 members, the most a toolset instance may have
    let limits = CrawlLimits::default();
    let members_per_toolset = crate::models::entities::toolsets::DEFAULT_MAX_TOOLS_PER_TOOLSET;
    let mut g = Graph::default();
    let mut agents = Vec::new();
    for i in 0..limits.max_agents_per_workflow {
        let members = (0..members_per_toolset)
            .map(|t| {
                let (_, instance) = g.tool(
                    &format!("tool-{i}-{t}"),
                    &[MINERVA],
                    agentic(&format!("t_{t}")),
                );
                (instance, ToolSetInclusion::Selector)
            })
            .collect::<Vec<(Uuid, ToolSetInclusion)>>();
        let ts = g.tool_set(&format!("kit-{i}"), &[MINERVA], false, None);
        g.tool_set_instance(ts, &members, true);
        let agent = g.agent(&format!("agent-{i}"), &[MINERVA]);
        g.edge(AssociationKind::Calls, agent, ts);
        agents.push(agent);
    }
    let wf = g.workflow("Huge", &[MINERVA], &agents, Some(agents[0]));
    g.reader.clear_reads();
    let start = Instant::now();
    let resolved = resolve(&g, wf).await;
    let elapsed = start.elapsed();
    let reads = g.reader.reads().len();
    assert_ready(&resolved);
    assert_eq!(resolved.instance.tools.len(), 32 * 64 + 1);
    // the workflow, each agent, its toolset and toolset instance, each member, and the entry
    // agent's harness once
    assert_eq!(reads, 1 + 32 * 3 + 32 * 64 + 1);
    assert!(elapsed.as_secs_f64() < 5.0, "resolving took {elapsed:?}");
    println!("32 agents with 64 toolset members each ({reads} reads) resolved in {elapsed:?}");
    // an agent with a hundred thousand toolset edges is refused without reading them
    let mut g = Graph::default();
    let lead = g.agent("lead", &[MINERVA]);
    for _ in 0..100_000 {
        g.reader.row(
            lead,
            GraphEdge {
                kind: AssociationKind::Calls,
                direction: Directionality::To,
                other: Uuid::new_v4(),
                other_kind: EntityKinds::ToolSet,
            },
        );
    }
    let wf = g.workflow("Wide", &[MINERVA], &[lead], Some(lead));
    g.reader.clear_reads();
    let resolved = resolve(&g, wf).await;
    assert_invalid(
        &resolved,
        &["100000 tools and toolsets", "max_tools_per_agent (128)"],
    );
    // only the workflow, the lead, and the lead's harness are read
    assert_eq!(g.reader.reads().len(), 3);
    // a toolset instance with more members than an agent could ever be given isn't read through
    let mut g = Graph::default();
    let (_, scan_v1) = g.tool("scan", &[MINERVA], agentic("scan"));
    let ts = g.tool_set("bloated", &[MINERVA], false, None);
    let tsi = g.tool_set_instance(ts, &[(scan_v1, ToolSetInclusion::Selector)], true);
    if let Some(EntityMetadata::ToolSetInstance(meta)) =
        g.reader.entity_mut(tsi).map(|entity| &mut entity.metadata)
    {
        let member = meta.members[0].clone();
        meta.members = (0..100_000)
            .map(|_| {
                let mut copy = member.clone();
                copy.tool_instance = Uuid::new_v4();
                copy
            })
            .collect();
    }
    let (wf, lead) = calling(&mut g, &[ts]);
    g.reader.clear_reads();
    let start = Instant::now();
    let resolved = resolve(&g, wf).await;
    let elapsed = start.elapsed();
    assert_invalid(
        &resolved,
        &["agent `lead`", "100000 tools", "max_tools_per_agent (128)"],
    );
    assert_eq!(
        resolved.instance.agent(lead).unwrap().tool_sets,
        Vec::<Uuid>::new()
    );
    // the workflow, the lead, the toolset, its instance, and the lead's harness are read
    assert_eq!(g.reader.reads().len(), 5);
    assert!(elapsed.as_secs_f64() < 5.0, "resolving took {elapsed:?}");
    println!("toolset instance with 100000 members refused in {elapsed:?}");
}

/// Build a workflow's next instance under the default limits
///
/// # Arguments
///
/// * `graph` - The graph to read
/// * `workflow` - The workflow to build
async fn build(graph: &Graph, workflow: Uuid) -> WorkflowBuild {
    // build manually as io, failing the test on a read error
    build_workflow_instance(
        &graph.reader,
        workflow,
        &CrawlLimits::default(),
        WorkflowInstanceTrigger::Manual,
        "io",
    )
    .await
    .unwrap()
}

/// A change that only makes an invalid instance valid, or a valid one invalid, builds a new
/// instance even when the digest and referenced ids stay the same
#[tokio::test]
async fn dedupe_follows_status() {
    // the entry agent is only reached through delegation, so r1 is invalid
    let mut g = Graph::default();
    let lead = g.agent("lead", &[MINERVA]);
    let helper = g.agent("helper", &[MINERVA]);
    g.edge(AssociationKind::DelegatesTo, lead, helper);
    let wf = g.workflow("Triage", &[MINERVA], &[lead], Some(helper));
    let r1 = resolve(&g, wf).await;
    assert_invalid(&r1, &["entry agent", "is not one of workflow"]);
    g.store(&r1, 1);
    // orchestrating the helper too makes the graph valid without changing the key's ids
    g.edge(AssociationKind::Orchestrates, wf, helper);
    let fixed = resolve(&g, wf).await;
    assert_ready(&fixed);
    let WorkflowBuild::New(r2) = build(&g, wf).await else {
        panic!("a graph that became valid must build a ready instance");
    };
    assert!(r2.instance.is_ready());
    g.store(&r2, 2);
    // dropping the helper's Orchestrates edge again makes the next instance invalid
    g.reader
        .remove_edge(AssociationKind::Orchestrates, wf, helper);
    assert!(matches!(build(&g, wf).await, WorkflowBuild::New(_)));
}

/// Switching the entry agent, or moving a delegation, between two agents with the same config
/// but different tools builds a new instance
#[tokio::test]
async fn dedupe_follows_wiring_between_same_config_agents() {
    // two agents with one config, calling different tools
    let mut g = Graph::default();
    let config = AgentConfig {
        system_prompt: Some("You help".to_owned()),
        function_name: Some("helper".to_owned()),
        model_description: Some("Delegate to a helper".to_owned()),
        ..AgentConfig::default()
    };
    let lead = g.agent("lead", &[MINERVA]);
    let one = g.agent_with("helper-one", &[MINERVA], config.clone());
    let two = g.agent_with("helper-two", &[MINERVA], config);
    let (_, runtime) = g.tool("runtime", &[MINERVA], harness(1));
    g.edge(AssociationKind::RunsIn, one, runtime);
    g.edge(AssociationKind::RunsIn, two, runtime);
    let (scan, _) = g.tool("scan", &[MINERVA], agentic("scan"));
    let (wipe, _) = g.tool("wipe", &[MINERVA], agentic("wipe"));
    g.edge(AssociationKind::Calls, one, scan);
    g.edge(AssociationKind::Calls, two, wipe);
    g.edge(AssociationKind::DelegatesTo, lead, one);
    let wf = g.workflow("Triage", &[MINERVA], &[lead, one, two], Some(one));
    let r1 = resolve(&g, wf).await;
    assert_ready(&r1);
    g.store(&r1, 1);
    assert!(matches!(build(&g, wf).await, WorkflowBuild::Current { .. }));
    // the entry agent moves to the identical copy
    g.change_workflow(wf, |meta| meta.entry_agent = Some(two));
    let switched = resolve(&g, wf).await;
    assert_eq!(switched.instance.digest, r1.instance.digest);
    assert_ne!(switched.dedupe_key(), r1.dedupe_key());
    let WorkflowBuild::New(r2) = build(&g, wf).await else {
        panic!("an entry switch must build a new instance");
    };
    assert_eq!(r2.instance.entry_agent, Some(two));
    g.store(&r2, 2);
    // the lead's delegation moves to the identical copy
    g.reader
        .remove_edge(AssociationKind::DelegatesTo, lead, one);
    g.edge(AssociationKind::DelegatesTo, lead, two);
    let moved = resolve(&g, wf).await;
    assert_eq!(moved.instance.digest, r2.instance.digest);
    let WorkflowBuild::New(r3) = build(&g, wf).await else {
        panic!("a moved delegation must build a new instance");
    };
    assert_eq!(r3.instance.agent(lead).unwrap().sub_agents, vec![two]);
}

/// A tag under the agent key that a stored instance carries beside its intrinsic ones doesn't
/// change its dedupe key, which takes its agents from its snapshot
#[tokio::test]
async fn dedupe_agents_from_snapshot() {
    // the sample graph with its current instance stored
    let (mut g, wf, _, _) = sample("triage");
    let resolved = resolve(&g, wf).await;
    let id = g.store(&resolved, 1);
    // an extra agent tag on the stored instance changes nothing
    g.reader
        .tag(id, "InstanceAgent", &Uuid::new_v4().to_string());
    assert!(matches!(build(&g, wf).await, WorkflowBuild::Current { .. }));
}

/// Build the graphs whose only problems are targets that don't cover their sources' groups:
/// a tool, a toolset instance, a container's Image instance, a sub-agent, and an agent of the
/// workflow, each missing mars
pub(crate) fn uncontained_graphs() -> Vec<(Graph, Uuid, Uuid)> {
    // each graph, its workflow, and the target that misses mars
    let mut graphs = Vec::new();
    // a tool missing one of its agent's groups
    let mut g = Graph::default();
    let (root, scan) = g.tool("scan", &[MINERVA], agentic("scan"));
    let lead = g.agent("lead", &[MINERVA, MARS]);
    g.edge(AssociationKind::Calls, lead, root);
    let wf = g.workflow("Triage", &[MINERVA, MARS], &[lead], Some(lead));
    graphs.push((g, wf, scan));
    // a toolset instance missing one of its agent's groups
    let mut g = Graph::default();
    let (_, member) = g.tool("scan", &[MINERVA, MARS], agentic("scan"));
    let ts = g.tool_set("kit", &[MINERVA], false, None);
    let tsi = g.tool_set_instance(ts, &[(member, ToolSetInclusion::ExplicitLatest)], true);
    let lead = g.agent("lead", &[MINERVA, MARS]);
    g.edge(AssociationKind::Calls, lead, ts);
    let wf = g.workflow("Triage", &[MINERVA, MARS], &[lead], Some(lead));
    graphs.push((g, wf, tsi));
    // a container's Image instance missing one of the workflow's groups
    let mut g = Graph::default();
    let (_, image_instance) = g.tool("ghidra-cli", &[MINERVA], image(60));
    let (backed, _) = g.tool(
        "ghidra",
        &[MINERVA, MARS],
        container("ghidra", image_instance),
    );
    let lead = g.agent("lead", &[MINERVA, MARS]);
    g.edge(AssociationKind::Calls, lead, backed);
    let wf = g.workflow("Triage", &[MINERVA, MARS], &[lead], Some(lead));
    graphs.push((g, wf, image_instance));
    // a sub-agent missing one of its delegator's groups
    let mut g = Graph::default();
    let lead = g.agent("lead", &[MINERVA, MARS]);
    let helper = g.agent("helper", &[MINERVA]);
    g.edge(AssociationKind::DelegatesTo, lead, helper);
    let wf = g.workflow("Triage", &[MINERVA, MARS], &[lead], Some(lead));
    graphs.push((g, wf, helper));
    // an agent missing one of the workflow's groups
    let mut g = Graph::default();
    let lead = g.agent("lead", &[MINERVA, MARS]);
    let narrow = g.agent("narrow", &[MINERVA]);
    let wf = g.workflow("Triage", &[MINERVA, MARS], &[lead, narrow], Some(lead));
    graphs.push((g, wf, narrow));
    graphs
}

/// A target that doesn't cover its source's groups makes the instance invalid and is left out
/// of it, so an invalid instance never snapshots an entity into a group it isn't in
#[tokio::test]
async fn uncontained_targets_left_out() {
    // resolve each graph with one uncontained target
    for (g, wf, target) in uncontained_graphs() {
        let resolved = resolve(&g, wf).await;
        assert_invalid(&resolved, &[MARS]);
        let instance = &resolved.instance;
        // the target is listed nowhere in the instance
        assert!(instance.tool(target).is_none(), "{target} is a listed tool");
        assert!(
            instance.agent(target).is_none(),
            "{target} is a listed agent"
        );
        for agent in &instance.agents {
            assert!(
                !agent.tools.contains(&target),
                "{target} given to {}",
                agent.name
            );
            assert!(
                !agent.tool_sets.contains(&target),
                "{target} given to {}",
                agent.name
            );
            assert!(
                !agent.sub_agents.contains(&target),
                "{target} under {}",
                agent.name
            );
        }
    }
}

/// An older-scheme stored instance whose toolset instance is gone is not current, rather than
/// failing the build
#[tokio::test]
async fn dedupe_older_scheme_missing_toolset_instance() {
    // a lead following a toolset, with the instance stored under the current scheme
    let mut g = Graph::default();
    let (_, scan) = g.tool("scan", &[MINERVA], agentic("scan"));
    let ts = g.tool_set("kit", &[MINERVA], false, None);
    let tsi = g.tool_set_instance(ts, &[(scan, ToolSetInclusion::ExplicitLatest)], true);
    let (wf, _) = calling(&mut g, &[ts]);
    let resolved = resolve(&g, wf).await;
    let id = g.store(&resolved, 1);
    let stored = g.reader.entity(id).unwrap().clone();
    // the toolset instance no longer reads as one
    g.reader.entity_mut(tsi).unwrap().metadata = EntityMetadata::ToolSet(ToolSetEntity::default());
    let mut candidate = resolved.dedupe_key();
    candidate.digest = "v2:00".to_owned();
    let current = is_current_with_scheme(&g.reader, "v2:", &candidate, &stored).await;
    assert!(matches!(current, Ok(false)), "{current:?}");
}

/// Give a tool an input schema close to the largest a tool may have
///
/// # Arguments
///
/// * `function_name` - The tool's function name
fn fat(function_name: &str) -> crate::models::ToolSpec {
    // an agentic tool with about 60 KiB of schema properties
    let mut spec = agentic(function_name);
    if let crate::models::ToolSpec::Agentic(tool) = &mut spec {
        let properties = (0..520)
            .map(|index| {
                (
                    format!("p{index}"),
                    serde_json::json!({"type": "string", "description": "x".repeat(80)}),
                )
            })
            .collect::<serde_json::Map<String, serde_json::Value>>();
        tool.input_schema.insert("type".into(), "object".into());
        tool.input_schema
            .insert("properties".into(), serde_json::Value::Object(properties));
    }
    spec
}

/// Agents at the toolset limit whose toolsets all share the same members with schemas near
/// the size cap resolve quickly, reading each member once
#[tokio::test]
async fn worst_case_shared_fat_members() {
    // 64 tools with fat schemas, in 64 toolsets that each hold all of them
    let limits = CrawlLimits::default();
    let mut g = Graph::default();
    let members = (0..limits.max_tools_per_agent)
        .map(|index| {
            let name = format!("tool-{index}");
            let (_, instance) = g.tool(&name, &[MINERVA], fat(&format!("tool_{index}")));
            (instance, ToolSetInclusion::Selector)
        })
        .collect::<Vec<(Uuid, ToolSetInclusion)>>();
    let sets = (0..limits.max_tools_per_agent)
        .map(|index| {
            let ts = g.tool_set(&format!("kit-{index}"), &[MINERVA], false, None);
            g.tool_set_instance(ts, &members, true);
            ts
        })
        .collect::<Vec<Uuid>>();
    // 32 agents that each call every toolset
    let agents = (0..limits.max_agents_per_workflow)
        .map(|index| {
            let agent = g.agent(&format!("agent-{index}"), &[MINERVA]);
            for ts in &sets {
                g.edge(AssociationKind::Calls, agent, *ts);
            }
            agent
        })
        .collect::<Vec<Uuid>>();
    let wf = g.workflow("Huge", &[MINERVA], &agents, Some(agents[0]));
    // the resolve is ready, quick, and reads each member once
    g.reader.clear_reads();
    let start = Instant::now();
    let resolved = resolve(&g, wf).await;
    let elapsed = start.elapsed();
    assert_ready(&resolved);
    for (member, _) in &members {
        assert_eq!(g.reader.node_reads(*member), 1);
    }
    assert!(elapsed.as_secs_f64() < 5.0, "resolving took {elapsed:?}");
    println!("32 agents x 64 shared fat members resolved in {elapsed:?}");
}

/// A graph reader that takes a few milliseconds for every entity read, as the API does
struct SlowGraph<'a>(
    /// The in-memory graph it reads
    &'a crate::models::entities::workflows::MemoryGraph,
);

#[async_trait::async_trait]
impl super::GraphReader for SlowGraph<'_> {
    /// Read an entity after a short wait
    ///
    /// # Arguments
    ///
    /// * `id` - The entity to read
    /// * `kinds` - The edge kinds to return
    /// * `direction` - The direction of the edges to return
    async fn node(
        &self,
        id: Uuid,
        kinds: &[AssociationKind],
        direction: Directionality,
    ) -> Result<Option<super::GraphNode>, crate::Error> {
        // wait as a round trip to the API would, then read
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        self.0.node(id, kinds, direction).await
    }

    /// Find the entities of a kind tagged with a value in any of some groups
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of entity to find
    /// * `key` - The tag key
    /// * `value` - The tag value
    /// * `groups` - The groups to search
    async fn tagged(
        &self,
        kind: EntityKinds,
        key: &str,
        value: &str,
        groups: &[String],
    ) -> Result<Vec<crate::models::Entity>, crate::Error> {
        // tag lookups are passed through
        self.0.tagged(kind, key, value, groups).await
    }
}

/// Resolving the largest workflow of container-backed tools through a reader with a round
/// trip per entity reads the entities a few at a time, each one once
#[tokio::test]
async fn slow_reader_reads_concurrently() {
    // 32 agents that each call 64 container-backed tools of their own: a read-concurrency check,
    // so the size stays where 5 ms reads finish well inside the time budget
    let limits = CrawlLimits::default();
    let tools_per_agent = 64;
    let mut g = Graph::default();
    let mut agents = Vec::new();
    for agent_index in 0..limits.max_agents_per_workflow {
        let agent = g.agent(&format!("agent-{agent_index}"), &[MINERVA]);
        for tool_index in 0..tools_per_agent {
            let name = format!("tool-{agent_index}-{tool_index}");
            let (_, image_instance) = g.tool(&format!("{name}-cli"), &[MINERVA], image(60));
            let (root, _) = g.tool(
                &name,
                &[MINERVA],
                container(&format!("t_{tool_index}"), image_instance),
            );
            g.edge(AssociationKind::Calls, agent, root);
        }
        agents.push(agent);
    }
    let wf = g.workflow("Wide", &[MINERVA], &agents, Some(agents[0]));
    // the slow resolve is ready, quick, and reads each entity once
    g.reader.clear_reads();
    let start = Instant::now();
    let resolved = resolve_workflow(&SlowGraph(&g.reader), wf, &limits)
        .await
        .unwrap();
    let elapsed = start.elapsed();
    assert_ready(&resolved);
    let reads = g.reader.reads().len();
    // the workflow, each agent, each tool with its instance and Image instance, and the entry
    // agent's harness
    assert_eq!(reads, 1 + 32 + 3 * 32 * tools_per_agent + 1);
    assert!(elapsed.as_secs_f64() < 5.0, "resolving took {elapsed:?}");
    println!("{reads} slow reads resolved in {elapsed:?}");
}

/// A workflow whose stored newest instance is gone, or that names none after giving out a
/// revision, finds its newest instance among its tagged instances
#[tokio::test]
async fn newest_instance_found_when_pointer_missing() {
    // the sample graph with two instances stored
    let (mut g, wf, _, _) = sample("triage");
    let resolved = resolve(&g, wf).await;
    g.store(&resolved, 1);
    let r2 = g.store(&resolved, 2);
    // set the newest instance the workflow names
    let name_newest = |g: &mut Graph, newest: Option<Uuid>| {
        if let Some(EntityMetadata::WorkflowCatalog(meta)) =
            g.reader.entity_mut(wf).map(|entity| &mut entity.metadata)
        {
            meta.newest_instance = newest;
        }
    };
    // a named newest instance that doesn't exist falls back to the tagged instances
    name_newest(&mut g, Some(Uuid::new_v4()));
    let workflow = g.reader.entity(wf).unwrap().clone();
    let newest = newest_instance(&g.reader, &workflow).await.unwrap();
    assert_eq!(newest.map(|entity| entity.id), Some(r2));
    // so does a workflow that names none after giving out revisions
    name_newest(&mut g, None);
    let workflow = g.reader.entity(wf).unwrap().clone();
    let newest = newest_instance(&g.reader, &workflow).await.unwrap();
    assert_eq!(newest.map(|entity| entity.id), Some(r2));
    // a build then supersedes the instance it found
    let extra = g.agent("extra", &[MINERVA]);
    g.edge(AssociationKind::Orchestrates, wf, extra);
    let WorkflowBuild::New(next) = build(&g, wf).await else {
        panic!("a changed graph should build a new instance");
    };
    assert_eq!(next.instance.revision, 3);
    assert_eq!(next.instance.previous, Some(r2));
}

/// Every agent that pins a deprecated tool instance gets its own warning
#[tokio::test]
async fn deprecated_pin_warned_per_agent() {
    // two agents pinning one deprecated instance
    let mut g = Graph::default();
    let (scan, scan_v1) = g.tool("scan", &[MINERVA], agentic("scan"));
    g.deprecate(scan, scan_v1);
    let lead = g.agent("lead", &[MINERVA]);
    let helper = g.agent("helper", &[MINERVA]);
    g.edge(AssociationKind::Calls, lead, scan_v1);
    g.edge(AssociationKind::Calls, helper, scan_v1);
    let wf = g.workflow("Triage", &[MINERVA], &[lead, helper], Some(lead));
    // each pinning agent is warned about
    let resolved = resolve(&g, wf).await;
    for name in ["lead", "helper"] {
        let warned = resolved
            .instance
            .warnings
            .iter()
            .filter(|warning| warning.contains(&format!("agent `{name}` pins deprecated")))
            .count();
        assert_eq!(warned, 1, "{name}: {:?}", resolved.instance.warnings);
    }
}

/// The dedupe key doesn't depend on the order an instance's errors were found in
#[tokio::test]
async fn dedupe_ignores_error_order() {
    // one resolved instance with the same errors in two orders
    let (g, wf, _, _) = sample("triage");
    let mut first = resolve(&g, wf).await;
    let mut second = first.clone();
    first.instance.status = WorkflowInstanceStatus::Invalid {
        errors: vec![
            "agent `a` is broken".to_owned(),
            "agent `b` is broken".to_owned(),
        ],
    };
    second.instance.status = WorkflowInstanceStatus::Invalid {
        errors: vec![
            "agent `b` is broken".to_owned(),
            "agent `a` is broken".to_owned(),
        ],
    };
    // both orders give the same key
    assert!(first.dedupe_key().same_except_digest(&second.dedupe_key()));
    assert_eq!(first.dedupe_key().errors, second.dedupe_key().errors);
}
