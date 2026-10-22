//! Tests for the toolset resolver, run against an in-memory graph

use std::collections::BTreeSet;
use std::time::Instant;
use uuid::Uuid;

use super::{
    ResolvedToolSet, ToolSetBuild, build_tool_set_instance, is_current_with_scheme,
    newest_tool_set_instance, resolve_tool_set,
};
use crate::models::entities::workflows::resolve::test_graph::{
    Graph, MARS, MINERVA, agentic, image, with_effects,
};
use crate::models::entities::workflows::{GraphEdge, GraphRead};
use crate::models::{
    AssociationKind, Directionality, EntityKinds, EntityMetadata, ToolEffect, ToolSetExclusion,
    ToolSetExclusionReason, ToolSetInclusion, ToolSetInstanceStatus, ToolSetInstanceTrigger,
    ToolSetLimits, ToolSetMember, ToolSetSelector,
};

/// A third group only some tools are in
const VULCAN: &str = "vulcan-rslv";

/// The selector most tests use
fn ghidra() -> ToolSetSelector {
    // tools tagged toolset=ghidra
    ToolSetSelector::new("toolset", "ghidra")
}

/// Resolve a toolset with the default limits
///
/// # Arguments
///
/// * `graph` - The graph to read
/// * `tool_set` - The toolset to resolve
async fn resolve(graph: &Graph, tool_set: Uuid) -> ResolvedToolSet {
    // resolve under the default limits
    resolve_with(graph, tool_set, ToolSetLimits::default()).await
}

/// Resolve a toolset with some limits
///
/// # Arguments
///
/// * `graph` - The graph to read
/// * `tool_set` - The toolset to resolve
/// * `limits` - The toolset limits
async fn resolve_with(graph: &Graph, tool_set: Uuid, limits: ToolSetLimits) -> ResolvedToolSet {
    // resolve and fail the test on a read error
    resolve_tool_set(&graph.reader, tool_set, &limits)
        .await
        .unwrap()
}

/// Get a resolved toolset instance's errors, or none for a ready instance
///
/// # Arguments
///
/// * `resolved` - The resolved toolset
fn errors(resolved: &ResolvedToolSet) -> Vec<String> {
    // a ready instance has no errors
    match &resolved.instance.status {
        ToolSetInstanceStatus::Ready => Vec::new(),
        ToolSetInstanceStatus::Invalid { errors } => errors.clone(),
    }
}

/// Check that a resolved toolset instance is ready
///
/// # Arguments
///
/// * `resolved` - The resolved toolset
fn assert_ready(resolved: &ResolvedToolSet) {
    // a ready instance has no errors to show
    assert_eq!(errors(resolved), Vec::<String>::new());
}

/// Check that a resolved toolset instance is invalid with an error holding every fragment
///
/// # Arguments
///
/// * `resolved` - The resolved toolset
/// * `fragments` - The text one error must hold
fn assert_invalid(resolved: &ResolvedToolSet, fragments: &[&str]) {
    // one error must hold every fragment
    let found = errors(resolved);
    assert!(
        found
            .iter()
            .any(|error| fragments.iter().all(|fragment| error.contains(fragment))),
        "no error holds {fragments:?}: {found:?}"
    );
}

/// Get each member's tool instance and inclusion, in tool instance order
///
/// # Arguments
///
/// * `resolved` - The resolved toolset
fn members(resolved: &ResolvedToolSet) -> Vec<(Uuid, ToolSetInclusion)> {
    // the members by tool instance
    let pairs = resolved
        .instance
        .members
        .iter()
        .map(|member| (member.tool_instance, member.inclusion))
        .collect::<Vec<(Uuid, ToolSetInclusion)>>();
    by_id(&pairs)
}

/// Get each exclusion's tool and reason
///
/// # Arguments
///
/// * `resolved` - The resolved toolset
fn excluded(resolved: &ResolvedToolSet) -> Vec<(Uuid, ToolSetExclusionReason)> {
    // the exclusions in their stored order
    resolved
        .instance
        .excluded
        .iter()
        .map(|exclusion| (exclusion.tool, exclusion.reason))
        .collect()
}

/// Sort a list of pairs by their first item
///
/// # Arguments
///
/// * `pairs` - The pairs to sort
fn by_id<T: Clone>(pairs: &[(Uuid, T)]) -> Vec<(Uuid, T)> {
    // copy and sort by id
    let mut sorted = pairs.to_vec();
    sorted.sort_by_key(|(id, _)| *id);
    sorted
}

/// Explicit edges resolve a tool to its latest instance and keep a tool instance as it is, and a
/// tool followed at latest with no latest instance makes the instance invalid
#[tokio::test]
async fn explicit_members() {
    // a toolset that includes one tool at latest and pins an older instance of another
    let mut g = Graph::default();
    let (scan, _) = g.tool("scan", &[MINERVA], agentic("scan"));
    let scan_v2 = g.publish(scan, agentic("scan2"), "2.0.0");
    let (hash, hash_v1) = g.tool("hash", &[MINERVA], agentic("hash"));
    g.publish(hash, agentic("hash2"), "2.0.0");
    let ts = g.tool_set("kit", &[MINERVA], false, None);
    g.edge(AssociationKind::Includes, ts, scan);
    g.edge(AssociationKind::Includes, ts, hash_v1);
    // resolve it
    let resolved = resolve(&g, ts).await;
    assert_ready(&resolved);
    assert_eq!(
        members(&resolved),
        by_id(&[
            (scan_v2, ToolSetInclusion::ExplicitLatest),
            (hash_v1, ToolSetInclusion::ExplicitPinned)
        ])
    );
    let member = resolved
        .instance
        .members
        .iter()
        .find(|member| member.tool_instance == scan_v2)
        .unwrap();
    assert_eq!(member.tool, scan);
    assert_eq!(member.name, "scan@2.0.0");
    assert_eq!(member.version, "2.0.0");
    assert_eq!(member.function_name, "scan2");
    assert_eq!(member.spec_hash, g.spec_hash(scan_v2));
    assert_eq!(member.groups, vec![MINERVA.to_owned()]);
    // only the member followed at latest is followed
    assert_eq!(resolved.follows_tools, BTreeSet::from([scan]));
    assert_eq!(resolved.instance.tool_set, ts);
    assert_eq!(resolved.instance.tool_set_groups, vec![MINERVA.to_owned()]);
    // deprecating every instance of the latest tool leaves it without a latest
    let scan_v1 = g.reader.entity(scan_v2).unwrap().clone();
    let EntityMetadata::ToolInstance(meta) = &scan_v1.metadata else {
        panic!("not a tool instance");
    };
    let first = meta.previous.unwrap();
    g.deprecate(scan, scan_v2);
    g.deprecate(scan, first);
    let resolved = resolve(&g, ts).await;
    assert_invalid(
        &resolved,
        &["toolset `kit`", "`scan`", "no latest instance"],
    );
    assert_eq!(
        members(&resolved),
        vec![(hash_v1, ToolSetInclusion::ExplicitPinned)]
    );
}

/// The selector matches agentic tools tagged in every group of the toolset, at latest, and
/// records why each other tagged tool was left out
#[tokio::test]
async fn selector_matches() {
    // a toolset in minerva and mars that selects toolset=ghidra
    let mut g = Graph::default();
    let both = [MINERVA, MARS];
    let ts = g.tool_set("ghidra-kit", &both, false, Some(ghidra()));
    // tagged in both groups: a member at its latest instance
    let (decompile, _) = g.tool("decompile", &both, agentic("decompile"));
    let decompile_v2 = g.publish(decompile, agentic("decompile2"), "2.0.0");
    g.tag_in(decompile, "toolset", "ghidra", &both);
    // tagged only in minerva: left out, naming mars
    let (rename, _) = g.tool("rename", &both, agentic("rename"));
    g.tag_in(rename, "toolset", "ghidra", &[MINERVA]);
    // tagged only in a group the toolset isn't in: not considered
    let (strings, _) = g.tool("strings", &[MINERVA, MARS, VULCAN], agentic("strings"));
    g.tag_in(strings, "toolset", "ghidra", &[VULCAN]);
    // an Image tool tagged in both: not agentic
    let (cli, _) = g.tool("ghidra-cli", &both, image(30));
    g.tag_in(cli, "toolset", "ghidra", &both);
    // a tool whose only instance is deprecated: no latest
    let (old, old_v1) = g.tool("old", &both, agentic("old"));
    g.deprecate(old, old_v1);
    g.tag_in(old, "toolset", "ghidra", &both);
    // the tag only on a tool instance, not its root: not matched
    let (xrefs, xrefs_v1) = g.tool("xrefs", &both, agentic("xrefs"));
    g.tag_in(xrefs_v1, "toolset", "ghidra", &both);
    // resolve it
    g.reader.clear_reads();
    let resolved = resolve(&g, ts).await;
    assert_ready(&resolved);
    assert_eq!(
        members(&resolved),
        vec![(decompile_v2, ToolSetInclusion::Selector)]
    );
    assert_eq!(
        excluded(&resolved),
        by_id(&[
            (rename, ToolSetExclusionReason::NotTaggedInAllGroups),
            (cli, ToolSetExclusionReason::NotAgentic),
            (old, ToolSetExclusionReason::NoLatest),
        ])
    );
    let partial = resolved
        .instance
        .excluded
        .iter()
        .find(|exclusion| exclusion.tool == rename)
        .unwrap();
    assert_eq!(partial.missing_groups, vec![MARS.to_owned()]);
    assert_eq!(partial.inclusion, ToolSetInclusion::Selector);
    assert_eq!(partial.tool_instance, None);
    assert_eq!(partial.describe(), format!("not tagged in `{MARS}`"));
    assert!(
        resolved
            .instance
            .excluded
            .iter()
            .all(|exclusion| exclusion.tool != strings && exclusion.tool != xrefs)
    );
    assert_eq!(resolved.follows_tools, BTreeSet::from([decompile]));
    assert_eq!(resolved.instance.selector, Some(ghidra()));
    // each group is searched on its own, for tools only
    let lookups = g
        .reader
        .reads()
        .into_iter()
        .filter(|read| matches!(read, GraphRead::Tagged { .. }))
        .collect::<Vec<GraphRead>>();
    let lookup = |group: &str| GraphRead::Tagged {
        kind: EntityKinds::Tool,
        key: "toolset".to_owned(),
        value: "ghidra".to_owned(),
        groups: vec![group.to_owned()],
    };
    assert_eq!(lookups, vec![lookup(MARS), lookup(MINERVA)]);
    // an intrinsic tool instance tag isn't matched either
    let ts = g.tool_set(
        "by-function",
        &both,
        false,
        Some(ToolSetSelector::new("ToolFunction", "xrefs")),
    );
    g.tag_in(xrefs_v1, "ToolFunction", "xrefs", &both);
    let resolved = resolve(&g, ts).await;
    assert_eq!(resolved.instance.members, Vec::<ToolSetMember>::new());
    assert_eq!(resolved.instance.excluded, Vec::<ToolSetExclusion>::new());
}

/// A tool both included explicitly and selected appears once, as the explicit edge has it
#[tokio::test]
async fn explicit_wins() {
    // a pinned older instance and a latest tool, both also tagged for the selector
    let mut g = Graph::default();
    let (scan, scan_v1) = g.tool("scan", &[MINERVA], agentic("scan"));
    g.publish(scan, agentic("scan2"), "2.0.0");
    let (hash, hash_v1) = g.tool("hash", &[MINERVA], agentic("hash"));
    let ts = g.tool_set("kit", &[MINERVA], false, Some(ghidra()));
    g.tag_in(scan, "toolset", "ghidra", &[MINERVA]);
    g.tag_in(hash, "toolset", "ghidra", &[MINERVA]);
    g.edge(AssociationKind::Includes, ts, scan_v1);
    g.edge(AssociationKind::Includes, ts, hash);
    // each tool appears once with its explicit inclusion and instance
    let resolved = resolve(&g, ts).await;
    assert_ready(&resolved);
    assert_eq!(
        members(&resolved),
        by_id(&[
            (scan_v1, ToolSetInclusion::ExplicitPinned),
            (hash_v1, ToolSetInclusion::ExplicitLatest),
        ])
    );
    assert_eq!(resolved.instance.excluded, Vec::<ToolSetExclusion>::new());
}

/// The read only filter leaves out every member that declares an effect, explicit ones too, with
/// a warning, and keeps them all when it's off
#[tokio::test]
async fn read_only_filter() {
    // an explicit tool that writes Thorium, selected tools that write outside or destroy, and a
    // selected tool with no effects
    let mut g = Graph::default();
    let (writes, writes_v1) = g.tool(
        "tagger",
        &[MINERVA],
        with_effects("tagger", &[ToolEffect::WritesThorium]),
    );
    let (external, external_v1) = g.tool(
        "notify",
        &[MINERVA],
        with_effects("notify", &[ToolEffect::WritesExternal]),
    );
    let (destroy, destroy_v1) = g.tool(
        "wipe",
        &[MINERVA],
        with_effects("wipe", &[ToolEffect::Destructive]),
    );
    let (reads, reads_v1) = g.tool("decompile", &[MINERVA], agentic("decompile"));
    for tool in [external, destroy, reads] {
        g.tag_in(tool, "toolset", "ghidra", &[MINERVA]);
    }
    let read_only = g.tool_set("ghidra-read-only", &[MINERVA], true, Some(ghidra()));
    g.edge(AssociationKind::Includes, read_only, writes);
    // only the tool with no effects is kept
    let resolved = resolve(&g, read_only).await;
    assert_ready(&resolved);
    assert_eq!(
        members(&resolved),
        vec![(reads_v1, ToolSetInclusion::Selector)]
    );
    assert_eq!(
        excluded(&resolved),
        by_id(&[
            (writes, ToolSetExclusionReason::NotReadOnly),
            (external, ToolSetExclusionReason::NotReadOnly),
            (destroy, ToolSetExclusionReason::NotReadOnly),
        ])
    );
    let explicit = resolved
        .instance
        .excluded
        .iter()
        .find(|exclusion| exclusion.tool == writes)
        .unwrap();
    assert_eq!(explicit.tool_instance, Some(writes_v1));
    assert_eq!(explicit.inclusion, ToolSetInclusion::ExplicitLatest);
    assert_eq!(resolved.instance.warnings.len(), 3);
    assert!(
        resolved
            .instance
            .warnings
            .iter()
            .any(|warning| warning.contains("tagger@1.0.0") && warning.contains("WritesThorium")),
        "{:?}",
        resolved.instance.warnings
    );
    // the same members without the filter are all kept
    let open = g.tool_set("ghidra", &[MINERVA], false, Some(ghidra()));
    g.edge(AssociationKind::Includes, open, writes);
    let resolved = resolve(&g, open).await;
    assert_ready(&resolved);
    assert_eq!(
        members(&resolved),
        by_id(&[
            (writes_v1, ToolSetInclusion::ExplicitLatest),
            (external_v1, ToolSetInclusion::Selector),
            (destroy_v1, ToolSetInclusion::Selector),
            (reads_v1, ToolSetInclusion::Selector),
        ])
    );
    assert_eq!(resolved.instance.excluded, Vec::<ToolSetExclusion>::new());
    assert_eq!(resolved.instance.warnings, Vec::<String>::new());
}

/// Two members with one function name, more members than the limit, and no members at all
#[tokio::test]
async fn validation() {
    // two different tools a model would call by the same name
    let mut g = Graph::default();
    let (first, _) = g.tool("scan-a", &[MINERVA], agentic("scan"));
    let (second, _) = g.tool("scan-b", &[MINERVA], agentic("scan"));
    let ts = g.tool_set("kit", &[MINERVA], false, None);
    g.edge(AssociationKind::Includes, ts, first);
    g.edge(AssociationKind::Includes, ts, second);
    let resolved = resolve(&g, ts).await;
    assert_invalid(&resolved, &["`scan`", "scan-a@1.0.0", "scan-b@1.0.0"]);
    // 64 members are ready and 65 aren't
    let limits = ToolSetLimits::default();
    assert_eq!(limits.max_tools_per_toolset, 64);
    let ts = g.tool_set("big", &[MINERVA], false, Some(ghidra()));
    for i in 0..64 {
        let (tool, _) = g.tool(
            &format!("tool-{i}"),
            &[MINERVA],
            agentic(&format!("tool_{i}")),
        );
        g.tag_in(tool, "toolset", "ghidra", &[MINERVA]);
    }
    let resolved = resolve(&g, ts).await;
    assert_ready(&resolved);
    assert_eq!(resolved.instance.members.len(), 64);
    let (extra, _) = g.tool("tool-64", &[MINERVA], agentic("tool_64"));
    g.tag_in(extra, "toolset", "ghidra", &[MINERVA]);
    let resolved = resolve(&g, ts).await;
    assert_invalid(&resolved, &["65 members", "max_tools_per_toolset (64)"]);
    // lower configured limits apply
    let low = ToolSetLimits {
        max_tools_per_toolset: 20,
    };
    let resolved = resolve_with(&g, ts, low).await;
    assert_invalid(&resolved, &["65 members", "max_tools_per_toolset (20)"]);
    // and so does the most tools a toolset may consider under them
    let lower = ToolSetLimits {
        max_tools_per_toolset: 10,
    };
    let resolved = resolve_with(&g, ts, lower).await;
    assert_invalid(&resolved, &["more than 40 tools", MINERVA]);
    assert_eq!(resolved.instance.members, Vec::<ToolSetMember>::new());
    // an empty toolset is ready with a warning
    let empty = g.tool_set("empty", &[MINERVA], false, None);
    let resolved = resolve(&g, empty).await;
    assert_ready(&resolved);
    assert_eq!(resolved.instance.members, Vec::<ToolSetMember>::new());
    assert!(
        resolved.instance.warnings[0].contains("no members"),
        "{:?}",
        resolved.instance.warnings
    );
}

/// A member whose groups don't cover the toolset's is left out and makes the instance invalid
#[tokio::test]
async fn member_containment() {
    // a toolset in minerva and mars pinning a tool only in minerva
    let mut g = Graph::default();
    let (_, narrow) = g.tool("scan", &[MINERVA], agentic("scan"));
    let ts = g.tool_set("kit", &[MINERVA, MARS], false, None);
    g.edge(AssociationKind::Includes, ts, narrow);
    let resolved = resolve(&g, ts).await;
    assert_invalid(&resolved, &["scan@1.0.0", MARS, "kit"]);
    assert_eq!(resolved.instance.members, Vec::<ToolSetMember>::new());
}

/// Build a toolset with a latest member, a pinned member, and a selected member
///
/// Returns the graph, the toolset, and its latest, pinned, and selected tools.
///
/// # Arguments
///
/// * `read_only` - Whether the toolset is read only
fn sample(read_only: bool) -> (Graph, Uuid, Uuid, Uuid, Uuid) {
    // three tools, each included a different way
    let mut g = Graph::default();
    let (latest, _) = g.tool("scan", &[MINERVA], agentic("scan"));
    let (pinned, pinned_v1) = g.tool("hash", &[MINERVA], agentic("hash"));
    let (selected, _) = g.tool("decompile", &[MINERVA], agentic("decompile"));
    g.tag_in(selected, "toolset", "ghidra", &[MINERVA]);
    let ts = g.tool_set("kit", &[MINERVA], read_only, Some(ghidra()));
    g.edge(AssociationKind::Includes, ts, latest);
    g.edge(AssociationKind::Includes, ts, pinned_v1);
    (g, ts, latest, pinned, selected)
}

/// The toolset digest covers the settings and each member's spec hash and inclusion, and nothing
/// else
#[tokio::test]
async fn digest_coverage() {
    // the same content built twice has the same digest
    let (g, ts, latest, _, selected) = sample(false);
    let base = resolve(&g, ts).await;
    let (other, other_ts, ..) = sample(false);
    let again = resolve(&other, other_ts).await;
    assert_ready(&base);
    assert_eq!(base.instance.digest, again.instance.digest);
    assert_ne!(base.referenced_ids(), again.referenced_ids());
    assert!(base.instance.digest.starts_with("v1:"));
    // read_only changes it
    let (g2, ts2, ..) = sample(true);
    assert_ne!(
        resolve(&g2, ts2).await.instance.digest,
        base.instance.digest
    );
    // the selector changes it
    let mut g3 = sample(false);
    if let Some(EntityMetadata::ToolSet(meta)) =
        g3.0.reader
            .entity_mut(g3.1)
            .map(|entity| &mut entity.metadata)
    {
        meta.selector = Some(ToolSetSelector::new("toolset", "radare"));
    }
    g3.0.tag_in(g3.4, "toolset", "radare", &[MINERVA]);
    let changed = resolve(&g3.0, g3.1).await;
    assert_eq!(changed.instance.members.len(), 3);
    assert_ne!(changed.instance.digest, base.instance.digest);
    // a member's spec hash changes it
    let mut g4 = sample(false);
    g4.0.publish(g4.2, agentic("scan-v2"), "2.0.0");
    assert_ne!(
        resolve(&g4.0, g4.1).await.instance.digest,
        base.instance.digest
    );
    // a member's inclusion changes it: the latest tool selected instead of included
    let mut g5 = sample(false);
    g5.0.reader
        .remove_edge(AssociationKind::Includes, g5.1, g5.2);
    g5.0.tag_in(g5.2, "toolset", "ghidra", &[MINERVA]);
    let switched = resolve(&g5.0, g5.1).await;
    assert_eq!(switched.instance.members.len(), 3);
    assert_ne!(switched.instance.digest, base.instance.digest);
    // an exclusion, names, revision, trigger, groups, warnings, and status don't change it
    let mut g6 = sample(false);
    let (partial, _) = g6.0.tool("rename", &[MINERVA, MARS], agentic("rename"));
    g6.0.tag_in(partial, "toolset", "ghidra", &[MARS]);
    let (image_tool, _) = g6.0.tool("cli", &[MINERVA], image(5));
    g6.0.tag_in(image_tool, "toolset", "ghidra", &[MINERVA]);
    let with_excluded = resolve(&g6.0, g6.1).await;
    assert_eq!(with_excluded.instance.excluded.len(), 1);
    assert_eq!(with_excluded.instance.digest, base.instance.digest);
    if let Some(entity) = g6.0.reader.entity_mut(g6.1) {
        entity.name = "renamed".to_owned();
    }
    if let Some(entity) = g6.0.reader.entity_mut(selected) {
        entity.name = "renamed-tool".to_owned();
    }
    assert_eq!(
        resolve(&g6.0, g6.1).await.instance.digest,
        base.instance.digest
    );
    let mut instance = base.instance.clone();
    instance.revision = 7;
    instance.trigger = ToolSetInstanceTrigger::ToolUpdated {
        tool: latest,
        tool_instance: Uuid::new_v4(),
    };
    instance.triggered_by = "europa".to_owned();
    instance.tool_set_groups.push(MARS.to_owned());
    instance.warnings.push("noted".to_owned());
    instance.status = ToolSetInstanceStatus::Invalid {
        errors: vec!["broken".to_owned()],
    };
    for member in &mut instance.members {
        member.name.push_str("-renamed");
        member.tool = Uuid::new_v4();
        member.tool_instance = Uuid::new_v4();
    }
    assert_eq!(instance.compute_digest().unwrap(), base.instance.digest);
}

/// The same graph read in different orders serializes to identical bytes, with members sorted by
/// tool and tool instance and exclusions by tool
#[tokio::test]
async fn output_order() {
    // a toolset with several members and exclusions
    let (mut g, ts, ..) = sample(false);
    for i in 0..5 {
        let (tool, _) = g.tool(&format!("extra-{i}"), &[MINERVA], agentic(&format!("x{i}")));
        g.tag_in(tool, "toolset", "ghidra", &[MINERVA]);
        let (old, old_v1) = g.tool(&format!("old-{i}"), &[MINERVA], agentic("p"));
        g.deprecate(old, old_v1);
        g.tag_in(old, "toolset", "ghidra", &[MINERVA]);
        let (cli, _) = g.tool(&format!("cli-{i}"), &[MINERVA], image(i));
        g.tag_in(cli, "toolset", "ghidra", &[MINERVA]);
        g.edge(AssociationKind::Includes, ts, tool);
    }
    let first = resolve(&g, ts).await;
    g.reader.reverse_row_order();
    let second = resolve(&g, ts).await;
    assert_eq!(
        serde_json::to_vec(&first.instance).unwrap(),
        serde_json::to_vec(&second.instance).unwrap()
    );
    // members sort by tool then tool instance, and exclusions by tool
    let keys = first
        .instance
        .members
        .iter()
        .map(|member| (member.tool, member.tool_instance))
        .collect::<Vec<(Uuid, Uuid)>>();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted);
    let tools = first
        .instance
        .excluded
        .iter()
        .map(|exclusion| exclusion.tool)
        .collect::<Vec<Uuid>>();
    let mut sorted_tools = tools.clone();
    sorted_tools.sort();
    assert_eq!(tools, sorted_tools);
    assert_eq!(first.instance.excluded.len(), 10);
}

/// A build is skipped when the newest instance has the same digest and referenced ids; swapping
/// an included tool for an identical one builds the next revision, tagged with the new tool
#[tokio::test]
async fn dedupe_key() {
    // a toolset including one tool at latest
    let mut g = Graph::default();
    let (scan, _) = g.tool("scan", &[MINERVA], agentic("scan"));
    let ts = g.tool_set("kit", &[MINERVA], false, None);
    g.edge(AssociationKind::Includes, ts, scan);
    let limits = ToolSetLimits::default();
    // with no instance the build is new at revision 1
    let ToolSetBuild::New(first) =
        build_tool_set_instance(&g.reader, ts, &limits, ToolSetInstanceTrigger::Manual, "io")
            .await
            .unwrap()
    else {
        panic!("the first build should be new");
    };
    assert_eq!(first.instance.revision, 1);
    assert_eq!(first.instance.previous, None);
    assert_eq!(first.instance.triggered_by, "io");
    let r1 = g.store_tool_set(&first, 1);
    // the stored key from tags and snapshot equals the resolved one
    let stored =
        crate::models::entities::workflows::DedupeKey::of_stored(g.reader.entity(r1).unwrap())
            .unwrap();
    assert_eq!(stored, first.dedupe_key());
    // the same graph is current
    let build = build_tool_set_instance(
        &g.reader,
        ts,
        &limits,
        ToolSetInstanceTrigger::ToolSetUpdated,
        "europa",
    )
    .await
    .unwrap();
    assert!(matches!(build, ToolSetBuild::Current { newest, .. } if newest == r1));
    // an identical tool replacing the included one keeps the digest but builds r2
    let (copy, _) = g.tool("scan-copy", &[MINERVA], agentic("scan"));
    g.reader.remove_edge(AssociationKind::Includes, ts, scan);
    g.edge(AssociationKind::Includes, ts, copy);
    let trigger = ToolSetInstanceTrigger::ToolSetUpdated;
    let ToolSetBuild::New(next) =
        build_tool_set_instance(&g.reader, ts, &limits, trigger.clone(), "europa")
            .await
            .unwrap()
    else {
        panic!("a swapped tool should build a new instance");
    };
    assert_eq!(next.instance.digest, first.instance.digest);
    assert_eq!(next.instance.revision, 2);
    assert_eq!(next.instance.previous, Some(r1));
    assert_eq!(next.instance.trigger, trigger);
    assert_eq!(next.follows_tools, BTreeSet::from([copy]));
    let r2 = g.store_tool_set(&next, 2);
    let tool_set = g.reader.entity(ts).unwrap().clone();
    let newest = newest_tool_set_instance(&g.reader, &tool_set)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(newest.id, r2);
    assert_eq!(newest.tags["ToolSetFollowsTool"].len(), 1);
    assert!(newest.tags["ToolSetFollowsTool"].contains_key(&copy.to_string()));
    // the next revision follows the highest revision the toolset ever gave out
    if let Some(EntityMetadata::ToolSet(meta)) =
        g.reader.entity_mut(ts).map(|entity| &mut entity.metadata)
    {
        meta.read_only = true;
        meta.highest_revision = 5;
    }
    let ToolSetBuild::New(later) =
        build_tool_set_instance(&g.reader, ts, &limits, trigger, "europa")
            .await
            .unwrap()
    else {
        panic!("a changed toolset should build a new instance");
    };
    assert_eq!(later.instance.revision, 6);
    assert_eq!(later.instance.previous, Some(r2));
}

/// A skipped toolset build reads only the toolset's graph and its newest instance
#[tokio::test]
async fn skipped_build_cost() {
    // the sample toolset with its current instance stored
    let (mut g, ts, ..) = sample(false);
    let limits = ToolSetLimits::default();
    let resolved = resolve(&g, ts).await;
    g.store_tool_set(&resolved, 1);
    // the reads of a resolve alone
    g.reader.clear_reads();
    resolve(&g, ts).await;
    let crawl = g.reader.reads();
    // a skipped build adds only the newest instance's tag lookup
    g.reader.clear_reads();
    let build =
        build_tool_set_instance(&g.reader, ts, &limits, ToolSetInstanceTrigger::Manual, "io")
            .await
            .unwrap();
    assert!(matches!(build, ToolSetBuild::Current { .. }));
    let reads = g.reader.reads();
    assert_eq!(reads.len(), crawl.len() + 1);
    assert_eq!(&reads[..crawl.len()], crawl.as_slice());
    assert_eq!(
        reads.last().unwrap(),
        &GraphRead::Tagged {
            kind: EntityKinds::ToolSetInstance,
            key: "ToolSetId".to_owned(),
            value: ts.to_string(),
            groups: vec![MINERVA.to_owned()],
        }
    );
}

/// A newest toolset instance digested under an older scheme is recomputed, and left as stored
#[tokio::test]
async fn dedupe_older_scheme() {
    // the sample toolset with its instance stored under the current scheme
    let (mut g, ts, ..) = sample(false);
    let resolved = resolve(&g, ts).await;
    let id = g.store_tool_set(&resolved, 1);
    let stored = g.reader.entity(id).unwrap().clone();
    // the candidate is digested under a newer scheme
    let mut candidate = resolved.dedupe_key();
    candidate.digest = resolved.instance.compute_digest_with_scheme("v2:").unwrap();
    assert!(candidate.digest.starts_with("v2:"));
    assert!(is_current_with_scheme("v2:", &candidate, &stored).unwrap());
    // another candidate under the newer scheme isn't current
    let mut other = candidate.clone();
    other.digest = "v2:00".to_owned();
    assert!(!is_current_with_scheme("v2:", &other, &stored).unwrap());
    // the stored instance keeps its older digest
    match &g.reader.entity(id).unwrap().metadata {
        EntityMetadata::ToolSetInstance(meta) => assert!(meta.digest.starts_with("v1:")),
        _ => panic!("not a toolset instance"),
    }
}

/// Resolving the largest toolset the limits allow stays fast, and toolsets far past them are
/// refused without reading their candidates
#[tokio::test]
async fn worst_case_under_limits() {
    // a toolset in two groups with as many explicit edges and selected tools as it may consider
    let limits = ToolSetLimits::default();
    let cap = limits.max_candidates();
    assert_eq!(cap, 256);
    let both = [MINERVA, MARS];
    let mut g = Graph::default();
    let ts = g.tool_set("huge", &both, true, Some(ghidra()));
    for i in 0..cap {
        let (explicit, _) = g.tool(&format!("explicit-{i}"), &both, agentic(&format!("e{i}")));
        g.edge(AssociationKind::Includes, ts, explicit);
        let (selected, _) = g.tool(&format!("selected-{i}"), &both, agentic(&format!("s{i}")));
        g.tag_in(selected, "toolset", "ghidra", &both);
    }
    g.reader.clear_reads();
    let start = Instant::now();
    let resolved = resolve(&g, ts).await;
    let elapsed = start.elapsed();
    let reads = g.reader.reads().len();
    assert_eq!(resolved.instance.members.len(), cap * 2);
    assert_invalid(&resolved, &["512 members", "max_tools_per_toolset (64)"]);
    // one toolset read, a lookup per group, and a read per explicit root and per member
    assert_eq!(reads, 1 + 2 + cap + cap * 2);
    assert!(elapsed.as_secs_f64() < 5.0, "resolving took {elapsed:?}");
    println!("largest considered toolset ({reads} reads) resolved in {elapsed:?}");
    // a toolset with a hundred thousand edges and tagged tools is refused without reading them
    let mut g = Graph::default();
    let ts = g.tool_set("wide", &both, false, Some(ghidra()));
    for _ in 0..100_000 {
        g.reader.row(
            ts,
            GraphEdge {
                kind: AssociationKind::Includes,
                direction: Directionality::To,
                other: Uuid::new_v4(),
                other_kind: EntityKinds::Tool,
            },
        );
    }
    for i in 0..2_000 {
        let (tool, _) = g.tool(&format!("tagged-{i}"), &both, agentic(&format!("t{i}")));
        g.tag_in(tool, "toolset", "ghidra", &both);
    }
    g.reader.clear_reads();
    let start = Instant::now();
    let resolved = resolve(&g, ts).await;
    let elapsed = start.elapsed();
    assert_invalid(&resolved, &["100000 Includes edges", "256"]);
    assert_invalid(&resolved, &["`toolset=ghidra`", "more than 256", MARS]);
    assert_eq!(resolved.instance.members, Vec::<ToolSetMember>::new());
    assert_eq!(g.reader.reads().len(), 2);
    assert!(elapsed.as_secs_f64() < 5.0, "resolving took {elapsed:?}");
    println!("over-limit toolset with 100000 edges and 2000 tagged tools resolved in {elapsed:?}");
}

/// The Thorium client's bounded tag lookup asks for at most the bound, in pages no larger
#[test]
fn bounded_tag_lookup() {
    // a bounded lookup in one group
    let groups = vec![MINERVA.to_owned()];
    let opts = crate::models::entities::workflows::resolve::bounded_tag_lookup(
        EntityKinds::Tool,
        "toolset",
        "ghidra",
        &groups,
        257,
    );
    assert_eq!(opts.groups, groups);
    assert_eq!(opts.kinds, vec![EntityKinds::Tool]);
    assert_eq!(opts.tags["toolset"], vec!["ghidra".to_owned()]);
    assert_eq!(opts.limit, Some(257));
    assert_eq!(opts.page_size, 257);
}

/// Every tool a toolset includes at latest is followed, whatever it resolves to, so its next
/// publish finds the toolset even when it left no member this time
#[tokio::test]
async fn explicit_latest_always_followed() {
    // a read only toolset including a tool that declares an effect
    let mut g = Graph::default();
    let (writes, _) = g.tool(
        "writer",
        &[MINERVA],
        with_effects("writer", &[ToolEffect::WritesThorium]),
    );
    let ts = g.tool_set("kit", &[MINERVA], true, None);
    g.edge(AssociationKind::Includes, ts, writes);
    let resolved = resolve(&g, ts).await;
    assert_ready(&resolved);
    assert_eq!(members(&resolved).len(), 0);
    assert!(resolved.follows_tools.contains(&writes));
    assert!(resolved.referenced_ids().contains(&writes));
    // a toolset including a tool with no latest instance
    let mut g = Graph::default();
    let (scan, scan_v1) = g.tool("scan", &[MINERVA], agentic("scan"));
    g.deprecate(scan, scan_v1);
    let ts = g.tool_set("kit", &[MINERVA], false, None);
    g.edge(AssociationKind::Includes, ts, scan);
    let resolved = resolve(&g, ts).await;
    assert_invalid(&resolved, &["no latest instance"]);
    assert!(resolved.follows_tools.contains(&scan));
    // a toolset including a tool that doesn't cover its groups
    let mut g = Graph::default();
    let (narrow, _) = g.tool("narrow", &[MINERVA], agentic("narrow"));
    let ts = g.tool_set("kit", &[MINERVA, MARS], false, None);
    g.edge(AssociationKind::Includes, ts, narrow);
    let resolved = resolve(&g, ts).await;
    assert_invalid(&resolved, &[MARS]);
    assert!(resolved.follows_tools.contains(&narrow));
}

/// A toolset instance's dedupe errors don't depend on the order they were found in
#[test]
fn dedupe_errors_sorted() {
    // the same errors found in two orders
    let found = |errors: &[&str]| ToolSetInstanceStatus::Invalid {
        errors: errors.iter().map(|error| (*error).to_owned()).collect(),
    };
    let first = super::status_errors(&found(&["tool `b` is gone", "tool `a` is gone"]));
    let second = super::status_errors(&found(&["tool `a` is gone", "tool `b` is gone"]));
    // both come out in one order
    assert_eq!(first, second);
}
