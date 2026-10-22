//! Tests for binding every agent's tools to a mechanism of the entry agent's harness

use uuid::Uuid;

use super::super::fixture::GraphRead;
use super::super::test_graph::{
    Graph, MINERVA, agentic, container, harness_supporting, harness_with, image, native,
};
use super::{assert_invalid, assert_ready, errors, resolve};
use crate::models::{
    AssociationKind, Directionality, HarnessCapabilities, HarnessToolPolicy, InstancedToolBinding,
    ToolEffect, ToolMechanism, ToolSetInclusion, ToolSpec,
};

/// Get the bindings an agent of a resolved instance was given
///
/// # Arguments
///
/// * `resolved` - The resolved workflow
/// * `agent` - The agent
fn bindings_of(resolved: &super::ResolvedWorkflow, agent: Uuid) -> Vec<InstancedToolBinding> {
    // the agent must be in the instance
    resolved.instance.agent(agent).unwrap().bindings.clone()
}

/// An agentic tool spec served by Thorium's MCP server that declares some capabilities
///
/// # Arguments
///
/// * `function_name` - The function name a model calls the tool by
/// * `change` - Changes the tool's capabilities
fn capable(
    function_name: &str,
    change: impl FnOnce(&mut crate::models::ToolCapabilities),
) -> ToolSpec {
    // an MCP tool with changed capabilities
    let mut spec = agentic(function_name);
    if let ToolSpec::Agentic(tool) = &mut spec {
        change(&mut tool.capabilities);
    }
    spec
}

/// Binding picks the highest-priority mechanism both the tool and the entry agent's harness
/// support, records the bindings sorted per agent, and binds a sub-agent's tools against the
/// entry agent's harness
#[tokio::test]
async fn binding_priority() {
    // each harness's mechanisms and the mechanism a container tool binds to in it
    let cases = [
        (
            vec![ToolMechanism::Cli, ToolMechanism::Mcp],
            ToolMechanism::Cli,
        ),
        (
            vec![ToolMechanism::ThoriumApi, ToolMechanism::Mcp],
            ToolMechanism::ThoriumApi,
        ),
        (vec![ToolMechanism::Mcp], ToolMechanism::Mcp),
    ];
    for (tag, (mechanisms, expected)) in (1_u64..).zip(cases) {
        // a lead calling a container tool and a helper calling an MCP tool
        let mut g = Graph::default();
        let (_, ghidra) = g.tool("ghidra-cli", &[MINERVA], image(60));
        let (_, decompile) = g.tool("decompile", &[MINERVA], container("decompile", ghidra));
        let (_, scan) = g.tool("scan", &[MINERVA], agentic("scan"));
        let lead = g.agent("lead", &[MINERVA]);
        let helper = g.agent("helper", &[MINERVA]);
        g.edge(AssociationKind::Calls, lead, decompile);
        g.edge(AssociationKind::Calls, lead, scan);
        g.edge(AssociationKind::Calls, helper, scan);
        g.edge(AssociationKind::DelegatesTo, lead, helper);
        // the lead runs in a harness with these mechanisms, and the helper in one with none
        let (_, runtime) = g.tool("runtime", &[MINERVA], harness_supporting(tag, &mechanisms));
        let (_, other) = g.tool("other", &[MINERVA], harness_supporting(tag + 10, &[]));
        g.edge(AssociationKind::RunsIn, lead, runtime);
        g.edge(AssociationKind::RunsIn, helper, other);
        let wf = g.workflow("Triage", &[MINERVA], &[lead], Some(lead));
        let resolved = resolve(&g, wf).await;
        assert_ready(&resolved);
        // the lead's tools are bound to the expected mechanism, sorted by tool instance
        let mut expected_lead = vec![
            InstancedToolBinding {
                tool_instance: decompile,
                mechanism: expected,
            },
            InstancedToolBinding {
                tool_instance: scan,
                mechanism: expected,
            },
        ];
        expected_lead.sort();
        assert_eq!(bindings_of(&resolved, lead), expected_lead);
        // the helper's tool is bound against the lead's harness, not its own
        assert_eq!(
            bindings_of(&resolved, helper),
            vec![InstancedToolBinding {
                tool_instance: scan,
                mechanism: expected,
            }]
        );
        // the Image dependency and the harnesses get no binding
        let bound = resolved
            .instance
            .agents
            .iter()
            .flat_map(|agent| agent.bindings.iter().map(|binding| binding.tool_instance))
            .collect::<Vec<Uuid>>();
        assert!(!bound.contains(&ghidra) && !bound.contains(&runtime));
    }
}

/// A tool needing file I/O the harness lacks, one with an effect the harness denies, and tools
/// with no mechanism in common with the harness each make the instance invalid, every one named
#[tokio::test]
async fn binding_refusals() {
    // a harness supporting only the CLI, without file I/O, that denies destructive tools
    let mut g = Graph::default();
    let capabilities = HarnessCapabilities {
        mechanisms: [ToolMechanism::Cli].into_iter().collect(),
        ..HarnessCapabilities::default()
    };
    let policy = HarnessToolPolicy {
        denied_effects: [ToolEffect::Destructive].into_iter().collect(),
        ..HarnessToolPolicy::default()
    };
    let (_, runtime) = g.tool("runtime", &[MINERVA], harness_with(1, capabilities, policy));
    // a tool reading files, one writing them, and one that deletes data
    let (_, reader) = g.tool(
        "reader",
        &[MINERVA],
        capable("reader", |caps| caps.file_inputs = true),
    );
    let (_, writer) = g.tool(
        "writer",
        &[MINERVA],
        capable("writer", |caps| caps.file_outputs = true),
    );
    let (_, wiper) = g.tool(
        "wiper",
        &[MINERVA],
        capable("wiper", |caps| {
            caps.effects.insert(ToolEffect::Destructive);
        }),
    );
    let (_, scan) = g.tool("scan", &[MINERVA], agentic("scan"));
    let lead = g.agent("lead", &[MINERVA]);
    for tool in [reader, writer, wiper, scan] {
        g.edge(AssociationKind::Calls, lead, tool);
    }
    g.edge(AssociationKind::RunsIn, lead, runtime);
    let wf = g.workflow("Triage", &[MINERVA], &[lead], Some(lead));
    let resolved = resolve(&g, wf).await;
    // every failing tool is reported
    assert_invalid(
        &resolved,
        &[
            "agent `lead`",
            "`reader@1.0.0`",
            "file_io",
            "`runtime@1.0.0`",
        ],
    );
    assert_invalid(&resolved, &["`writer@1.0.0`", "file_io"]);
    assert_invalid(
        &resolved,
        &["`runtime@1.0.0`", "Destructive", "`wiper@1.0.0`"],
    );
    assert_eq!(errors(&resolved).len(), 3, "{:?}", errors(&resolved));
    // the bindable tool is still bound
    assert_eq!(
        bindings_of(&resolved, lead),
        vec![InstancedToolBinding {
            tool_instance: scan,
            mechanism: ToolMechanism::Cli,
        }]
    );
    // a harness with no mechanisms names each tool, what it needs, and the harness
    let mut g = Graph::default();
    let (_, bare) = g.tool("bare", &[MINERVA], harness_supporting(2, &[]));
    let (_, scan) = g.tool("scan", &[MINERVA], agentic("scan"));
    let (_, strings) = g.tool("strings", &[MINERVA], agentic("strings"));
    let lead = g.agent("lead", &[MINERVA]);
    g.edge(AssociationKind::Calls, lead, scan);
    g.edge(AssociationKind::Calls, lead, strings);
    g.edge(AssociationKind::RunsIn, lead, bare);
    let wf = g.workflow("Triage", &[MINERVA], &[lead], Some(lead));
    let resolved = resolve(&g, wf).await;
    for tool in ["`scan@1.0.0`", "`strings@1.0.0`"] {
        assert_invalid(
            &resolved,
            &[
                tool,
                "needs ThoriumApi, Cli, or Mcp",
                "harness `bare@1.0.0` supports none",
            ],
        );
    }
}

/// A native tool binds to Native only when the entry agent's harness instance provides it,
/// whether an agent calls it directly or through a toolset, and the crawl reads `Provides` only
/// forward from the entry agent's harness instance
#[tokio::test]
async fn native_tools() {
    // a harness instance that provides a native tool, and one that doesn't
    let mut g = Graph::default();
    let (_, decompile) = g.tool("ghidra_decompile", &[MINERVA], native("ghidra_decompile"));
    let (_, providing) = g.tool(
        "PalHarness",
        &[MINERVA],
        harness_supporting(1, &[ToolMechanism::Cli]),
    );
    let (_, plain) = g.tool(
        "plain",
        &[MINERVA],
        harness_supporting(2, &[ToolMechanism::Cli]),
    );
    g.edge(AssociationKind::Provides, providing, decompile);
    // another harness also provides it, which the crawl must never read back to
    let (_, elsewhere) = g.tool("elsewhere", &[MINERVA], harness_supporting(3, &[]));
    g.edge(AssociationKind::Provides, elsewhere, decompile);
    // a lead calling it directly in the providing harness binds it to Native
    let lead = g.agent("lead", &[MINERVA]);
    g.edge(AssociationKind::Calls, lead, decompile);
    g.edge(AssociationKind::RunsIn, lead, providing);
    let wf = g.workflow("Triage", &[MINERVA], &[lead], Some(lead));
    g.reader.clear_reads();
    let resolved = resolve(&g, wf).await;
    assert_ready(&resolved);
    assert_eq!(
        bindings_of(&resolved, lead),
        vec![InstancedToolBinding {
            tool_instance: decompile,
            mechanism: ToolMechanism::Native,
        }]
    );
    // Provides is only read from the entry agent's harness instance, and only forward
    let provides_reads = g
        .reader
        .reads()
        .into_iter()
        .filter(|read| {
            matches!(read, GraphRead::Node { kinds, .. } if kinds.contains(&AssociationKind::Provides))
        })
        .collect::<Vec<GraphRead>>();
    assert_eq!(
        provides_reads,
        vec![GraphRead::Node {
            id: providing,
            kinds: vec![AssociationKind::Provides],
            direction: Directionality::To,
        }]
    );
    assert_eq!(g.reader.node_reads(elsewhere), 0);
    // the same lead in a harness that doesn't provide it is invalid
    let mut g2 = Graph::default();
    let (_, unprovided) = g2.tool("ghidra_decompile", &[MINERVA], native("ghidra_decompile"));
    let (_, plain2) = g2.tool(
        "plain",
        &[MINERVA],
        harness_supporting(2, &[ToolMechanism::Cli]),
    );
    let lead = g2.agent("lead", &[MINERVA]);
    g2.edge(AssociationKind::Calls, lead, unprovided);
    g2.edge(AssociationKind::RunsIn, lead, plain2);
    let wf = g2.workflow("Triage", &[MINERVA], &[lead], Some(lead));
    assert_invalid(
        &resolve(&g2, wf).await,
        &[
            "agent `lead`",
            "`ghidra_decompile@1.0.0` isn't provided by `plain@1.0.0`",
        ],
    );
    // through a toolset the native tool binds the same way, bound when provided
    let ts = g.tool_set("ghidra-kit", &[MINERVA], false, None);
    g.tool_set_instance(ts, &[(decompile, ToolSetInclusion::ExplicitLatest)], true);
    let kit_lead = g.agent("kit-lead", &[MINERVA]);
    g.edge(AssociationKind::Calls, kit_lead, ts);
    g.edge(AssociationKind::RunsIn, kit_lead, providing);
    let wf = g.workflow("Kit", &[MINERVA], &[kit_lead], Some(kit_lead));
    let resolved = resolve(&g, wf).await;
    assert_ready(&resolved);
    assert_eq!(
        bindings_of(&resolved, kit_lead)[0].mechanism,
        ToolMechanism::Native
    );
    // and invalid when the entry agent's harness doesn't provide it
    let plain_lead = g.agent("plain-lead", &[MINERVA]);
    g.edge(AssociationKind::Calls, plain_lead, ts);
    g.edge(AssociationKind::RunsIn, plain_lead, plain);
    let wf = g.workflow("Plain", &[MINERVA], &[plain_lead], Some(plain_lead));
    assert_invalid(&resolve(&g, wf).await, &["isn't provided by `plain@1.0.0`"]);
    // a sub-agent's native tool is checked against the entry agent's harness, not its own
    let boss = g.agent("boss", &[MINERVA]);
    let helper = g.agent("helper", &[MINERVA]);
    g.edge(AssociationKind::DelegatesTo, boss, helper);
    g.edge(AssociationKind::RunsIn, boss, plain);
    g.edge(AssociationKind::RunsIn, helper, providing);
    g.edge(AssociationKind::Calls, helper, decompile);
    let wf = g.workflow("Delegated", &[MINERVA], &[boss], Some(boss));
    assert_invalid(
        &resolve(&g, wf).await,
        &["agent `helper`", "isn't provided by `plain@1.0.0`"],
    );
}

/// A binding change through a new harness version changes the digest, and identical graphs give
/// identical bindings and digests
#[tokio::test]
async fn bindings_in_digest() {
    // a lead following a CLI-only harness at latest
    let mut g = Graph::default();
    let (_, scan) = g.tool("scan", &[MINERVA], agentic("scan"));
    let lead = g.agent("lead", &[MINERVA]);
    g.edge(AssociationKind::Calls, lead, scan);
    let (runtime, _) = g.tool(
        "runtime",
        &[MINERVA],
        harness_supporting(1, &[ToolMechanism::Cli]),
    );
    g.edge(AssociationKind::RunsIn, lead, runtime);
    let wf = g.workflow("Triage", &[MINERVA], &[lead], Some(lead));
    // identical resolutions give identical bindings and digests
    let first = resolve(&g, wf).await;
    let again = resolve(&g, wf).await;
    assert_eq!(bindings_of(&first, lead), bindings_of(&again, lead));
    assert_eq!(first.instance.digest, again.instance.digest);
    assert_eq!(bindings_of(&first, lead)[0].mechanism, ToolMechanism::Cli);
    // a new harness version supporting MCP only changes the binding and the digest
    g.publish(
        runtime,
        harness_supporting(2, &[ToolMechanism::Mcp]),
        "2.0.0",
    );
    let changed = resolve(&g, wf).await;
    assert_ready(&changed);
    assert_eq!(bindings_of(&changed, lead)[0].mechanism, ToolMechanism::Mcp);
    assert_ne!(changed.instance.digest, first.instance.digest);
}
