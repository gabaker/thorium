//! Tests for mapping, coalescing, and building toolsets from graph events over an in-memory graph

use chrono::Utc;
use std::collections::{BTreeMap, BTreeSet};
use thorium::models::entities::digests::ThorDigest;
use thorium::models::entities::toolsets::{
    TOOL_SET_FOLLOWS_TOOL_TAG, TOOL_SET_ID_TAG, TOOL_SET_SELECTOR_TAG, resolve_tool_set,
};
use thorium::models::entities::workflows::{FOLLOWS_TOOL_TAG, GraphEdge, GraphRead, MemoryGraph};
use thorium::models::{
    AssociationKind, CrawlLimits, Directionality, EntityKinds, EntityMetadata, Event,
    NotificationLevel, ToolEntity, ToolInstanceEntity, ToolSetEntity, ToolSetInstanceTrigger,
    ToolSetLimits, ToolSetSelector, ToolVersion, WorkflowCatalogEntity, WorkflowGraphChange,
};
use thorium::test_utilities::generators;
use uuid::Uuid;

use super::ToolSetInstancer;
use crate::libs::workers::ai::fake::{Created, FakeApi, entity, event};
use crate::libs::workers::ai::{AiWorkerStats, process_batch};

/// Add an agentic tool and its first instance, which is its latest
///
/// # Arguments
///
/// * `graph` - The graph to add to
/// * `name` - The tool's name
/// * `groups` - The tool's groups
fn tool(graph: &mut MemoryGraph, name: &str, groups: &[&str]) -> (Uuid, Uuid) {
    // add the root first, so the instance can name it
    let spec = generators::gen_agentic_spec();
    let root = entity(
        EntityKinds::Tool,
        name,
        groups,
        EntityMetadata::Tool(ToolEntity {
            kind: spec.kind(),
            latest: None,
            latest_version: None,
            latest_spec_hash: None,
            modified: Utc::now(),
            instances: Vec::new(),
            harness_env: None,
            disabled_models: std::collections::BTreeSet::new(),
        }),
    );
    let root_id = root.id;
    graph.insert(root);
    let instance = publish(graph, root_id, "1.0.0");
    (root_id, instance)
}

/// Publish a new instance of a tool with a fresh spec and make it the tool's latest
///
/// # Arguments
///
/// * `graph` - The graph to publish in
/// * `root` - The tool
/// * `version` - The new instance's version label
fn publish(graph: &mut MemoryGraph, root: Uuid, version: &str) -> Uuid {
    // the instance is in its tool's groups
    let tool = graph.entity(root).unwrap().clone();
    let groups = tool
        .groups
        .iter()
        .map(String::as_str)
        .collect::<Vec<&str>>();
    let spec = generators::gen_agentic_spec();
    let spec_hash = ThorDigest::tool_spec(&spec, None).unwrap();
    let meta = ToolInstanceEntity {
        tool: root,
        version: ToolVersion::from(version),
        previous: None,
        spec_hash: spec_hash.clone(),
        deprecated: false,
        spec,
        portability: None,
        provides: Vec::new(),
    };
    let instance = entity(
        EntityKinds::ToolInstance,
        &format!("{}@{version}", tool.name),
        &groups,
        EntityMetadata::ToolInstance(meta),
    );
    let id = instance.id;
    graph.insert(instance);
    // point the tool at it
    if let Some(EntityMetadata::Tool(meta)) = graph.entity_mut(root).map(|e| &mut e.metadata) {
        meta.latest = Some(id);
        meta.latest_spec_hash = Some(spec_hash);
    }
    id
}

/// Add a toolset
///
/// # Arguments
///
/// * `graph` - The graph to add to
/// * `name` - The toolset's name
/// * `groups` - The toolset's groups
/// * `auto_instance` - Whether changes build instances automatically
/// * `selector` - The tag that selects members, if any
fn tool_set(
    graph: &mut MemoryGraph,
    name: &str,
    groups: &[&str],
    auto_instance: bool,
    selector: Option<ToolSetSelector>,
) -> Uuid {
    // a toolset with its settings and no instance yet
    let meta = ToolSetEntity {
        auto_instance,
        selector,
        modified: Utc::now(),
        ..ToolSetEntity::default()
    };
    let tool_set = entity(
        EntityKinds::ToolSet,
        name,
        groups,
        EntityMetadata::ToolSet(meta),
    );
    let id = tool_set.id;
    graph.insert(tool_set);
    id
}

/// Resolve a toolset as it is now and store the result at a revision, tagged as the API tags it
///
/// # Arguments
///
/// * `graph` - The graph to store in
/// * `tool_set` - The toolset
/// * `revision` - The revision to store it at
async fn store(graph: &mut MemoryGraph, tool_set: Uuid, revision: u64) -> Uuid {
    // resolve the toolset and store its instance in the toolset's groups
    let resolved = resolve_tool_set(&*graph, tool_set, &ToolSetLimits::default())
        .await
        .unwrap();
    let mut instance = resolved.instance.clone();
    instance.revision = revision;
    let groups = resolved
        .tool_set
        .groups
        .iter()
        .map(String::as_str)
        .collect::<Vec<&str>>();
    let stored = entity(
        EntityKinds::ToolSetInstance,
        &format!("{}#r{revision}", resolved.tool_set.name),
        &groups,
        EntityMetadata::ToolSetInstance(instance.clone()),
    );
    let id = stored.id;
    graph.insert(stored);
    // tag it with its toolset, the tools it follows, and its selector
    graph.tag(id, TOOL_SET_ID_TAG, &tool_set.to_string());
    for followed in &resolved.follows_tools {
        graph.tag(id, TOOL_SET_FOLLOWS_TOOL_TAG, &followed.to_string());
    }
    if let Some(selector) = &instance.selector {
        graph.tag(id, TOOL_SET_SELECTOR_TAG, &selector.to_string());
    }
    // the toolset gave out this revision
    if let Some(EntityMetadata::ToolSet(meta)) = graph.entity_mut(tool_set).map(|e| &mut e.metadata)
    {
        meta.highest_revision = meta.highest_revision.max(revision);
    }
    id
}

/// Build an instancer over a fake API with the default limits
///
/// # Arguments
///
/// * `api` - The fake API
fn instancer(api: &FakeApi) -> ToolSetInstancer<'_, FakeApi> {
    // the default toolset limits
    ToolSetInstancer::new(api, ToolSetLimits::default())
}

/// Get the ids of the toolsets the created instances belong to
///
/// # Arguments
///
/// * `api` - The fake API
fn built(api: &FakeApi) -> Vec<Uuid> {
    // each created instance's toolset, sorted
    let mut tool_sets = api
        .created_tool_sets()
        .iter()
        .map(|instance| instance.tool_set)
        .collect::<Vec<Uuid>>();
    tool_sets.sort();
    tool_sets
}

/// Get the sorted ids of some events
///
/// # Arguments
///
/// * `events` - The events
fn sorted_ids(events: &[&Event]) -> Vec<Uuid> {
    // the ids, sorted
    let mut ids = events.iter().map(|event| event.id).collect::<Vec<Uuid>>();
    ids.sort();
    ids
}

/// Sort a list of ids
///
/// # Arguments
///
/// * `ids` - The ids to sort
fn sorted(mut ids: Vec<Uuid>) -> Vec<Uuid> {
    // sort in place and hand back
    ids.sort();
    ids
}

/// The latest qualifying event decides a coalesced toolset build: a tool change that only reached
/// an older instance never takes the trigger or the user from an earlier settings change, and
/// with automatic instances off only a manual request decides
#[tokio::test]
async fn latest_qualifying_event_decides_the_tool_set_build() {
    // a toolset whose first instance followed the tool and whose newest doesn't, which has
    // gained another tool since, and a toolset with automatic instances off
    let mut api = FakeApi::default();
    let groups = ["minerva-tq1"];
    let (tool_id, _) = tool(&mut api.graph, "parser", &groups);
    let (other, _) = tool(&mut api.graph, "other", &groups);
    let edited = tool_set(&mut api.graph, "edited", &groups, true, None);
    api.graph.edge(AssociationKind::Includes, edited, tool_id);
    store(&mut api.graph, edited, 1).await;
    api.graph
        .remove_edge(AssociationKind::Includes, edited, tool_id);
    store(&mut api.graph, edited, 2).await;
    api.graph.edge(AssociationKind::Includes, edited, other);
    let manual_only = tool_set(&mut api.graph, "manual-only", &groups, false, None);
    let latest = publish(&mut api.graph, tool_id, "2.0.0");
    // io changes the first toolset and callisto asks for the second, then callisto's publish
    // reaches the first toolset's old instance and io changes the second
    let events = vec![
        event(
            "io-tq1",
            &groups,
            WorkflowGraphChange::ToolSetChanged { tool_set: edited },
        ),
        event(
            "callisto-tq1",
            &groups,
            WorkflowGraphChange::ManualToolSetInstance {
                tool_set: manual_only,
            },
        ),
        event(
            "callisto-tq1",
            &groups,
            WorkflowGraphChange::ToolUpdated {
                tool: tool_id,
                tool_instance: latest,
            },
        ),
        event(
            "io-tq1",
            &groups,
            WorkflowGraphChange::ToolSetChanged {
                tool_set: manual_only,
            },
        ),
    ];
    instancer(&api).process(&events).await;
    // the settings change decides the first build and the manual request decides the second
    let created = api.created_tool_sets();
    let edited_build = created.iter().find(|i| i.tool_set == edited).unwrap();
    assert_eq!(edited_build.triggered_by, "io-tq1");
    assert_eq!(edited_build.trigger, ToolSetInstanceTrigger::ToolSetUpdated);
    let manual_build = created.iter().find(|i| i.tool_set == manual_only).unwrap();
    assert_eq!(manual_build.triggered_by, "callisto-tq1");
    assert_eq!(manual_build.trigger, ToolSetInstanceTrigger::Manual);
}

/// Each kind of graph change maps to the toolsets it rebuilds: the toolset it names, the toolsets
/// whose newest instance follows a published tool or selects one of its tags, and the toolsets
/// selecting a changed tag pair; changes to workflows, agents, and other edges map to none.
/// Lookups search only the event's groups and only the published tool is read by id
#[tokio::test]
// one table drives every kind of change through the same checks
#[allow(clippy::too_many_lines)]
async fn every_change_maps_to_its_tool_sets() {
    // a toolset named by events, one that follows a tool, and one that selects the tool's tag
    let mut api = FakeApi::default();
    let groups = ["minerva-tm1", "mars-tm1"];
    let (tool_id, _) = tool(&mut api.graph, "parser", &groups);
    api.graph.tag(tool_id, "family", "parser");
    let named = tool_set(&mut api.graph, "named", &groups, true, None);
    let follows = tool_set(&mut api.graph, "follows", &groups, true, None);
    api.graph.edge(AssociationKind::Includes, follows, tool_id);
    let follows_r1 = store(&mut api.graph, follows, 1).await;
    let selector = ToolSetSelector::new("family", "parser");
    let selects = tool_set(&mut api.graph, "selects", &groups, true, Some(selector));
    let selects_r1 = store(&mut api.graph, selects, 1).await;
    // one event of every kind
    let new_instance = Uuid::new_v4();
    let pair = |key: &str, value: &str| {
        BTreeMap::from([(key.to_owned(), BTreeSet::from([value.to_owned()]))])
    };
    let edge = |source: Uuid, kind: AssociationKind| WorkflowGraphChange::EdgeChanged {
        source,
        target: Uuid::new_v4(),
        kind,
    };
    let cases = vec![
        (
            WorkflowGraphChange::ToolSetChanged { tool_set: named },
            vec![named],
            Some(ToolSetInstanceTrigger::ToolSetUpdated),
        ),
        (
            WorkflowGraphChange::ManualToolSetInstance { tool_set: named },
            vec![named],
            Some(ToolSetInstanceTrigger::Manual),
        ),
        (
            edge(named, AssociationKind::Includes),
            vec![named],
            Some(ToolSetInstanceTrigger::ToolSetUpdated),
        ),
        (
            WorkflowGraphChange::ToolUpdated {
                tool: tool_id,
                tool_instance: new_instance,
            },
            sorted(vec![follows, selects]),
            Some(ToolSetInstanceTrigger::ToolUpdated {
                tool: tool_id,
                tool_instance: new_instance,
            }),
        ),
        (
            WorkflowGraphChange::ToolTagsChanged {
                tool: tool_id,
                tags: pair("family", "parser"),
            },
            vec![selects],
            Some(ToolSetInstanceTrigger::ToolSetUpdated),
        ),
        (
            WorkflowGraphChange::ToolTagsChanged {
                tool: tool_id,
                tags: pair("family", "other"),
            },
            vec![],
            None,
        ),
        (
            WorkflowGraphChange::WorkflowUpdated {
                workflow: Uuid::new_v4(),
            },
            vec![],
            None,
        ),
        (
            WorkflowGraphChange::ManualInstance {
                workflow: Uuid::new_v4(),
            },
            vec![],
            None,
        ),
        (
            WorkflowGraphChange::AgentUpdated {
                agent: Uuid::new_v4(),
            },
            vec![],
            None,
        ),
        (
            WorkflowGraphChange::ToolSetUpdated {
                tool_set: named,
                tool_set_instance: Uuid::new_v4(),
            },
            vec![],
            None,
        ),
        (edge(named, AssociationKind::Calls), vec![], None),
        (edge(named, AssociationKind::Orchestrates), vec![], None),
    ];
    for (change, expected, trigger) in cases {
        // map one event on its own
        api.graph.clear_reads();
        let ev = event("io-tm1", &groups, change.clone());
        let mapping = instancer(&api).affected(std::slice::from_ref(&ev)).await;
        let found = mapping.tool_sets.keys().copied().collect::<Vec<Uuid>>();
        assert_eq!(found, expected, "{change:?} mapped to the wrong toolsets");
        assert!(mapping.failed.is_empty(), "{change:?} failed to map");
        // an event that rebuilds nothing is still finished
        if expected.is_empty() {
            assert_eq!(mapping.unmapped, vec![ev.id], "{change:?} wasn't finished");
        }
        // each toolset records the event, and the event decides its trigger and user
        for affected in mapping.tool_sets.values() {
            let cause = affected.decide(true, None, None).unwrap();
            assert_eq!(Some(&cause.trigger), trigger.as_ref(), "{change:?} trigger");
            assert_eq!(cause.triggered_by, "io-tm1");
            assert_eq!(affected.events, vec![ev.id]);
            let direct = affected.root == named;
            assert_eq!(affected.direct(), direct, "{change:?} directness");
            assert_eq!(affected.matched().is_empty(), direct, "{change:?} matches");
            // only a manual request is told about the build as a requester
            let manual = matches!(change, WorkflowGraphChange::ManualToolSetInstance { .. });
            assert_eq!(affected.requesters().len(), usize::from(manual));
        }
        // every lookup searched toolset instances in exactly the event's groups, and only a
        // published tool and the matched instances, which name their toolsets, were read by id
        for read in api.graph.reads() {
            match read {
                GraphRead::Tagged {
                    kind,
                    groups: searched,
                    ..
                } => {
                    assert_eq!(kind, EntityKinds::ToolSetInstance, "{change:?}");
                    let mut want = groups.map(str::to_owned).to_vec();
                    want.sort();
                    assert_eq!(searched, want);
                }
                GraphRead::Node { id, .. } => {
                    assert!(
                        (matches!(change, WorkflowGraphChange::ToolUpdated { .. })
                            && id == tool_id)
                            || [follows_r1, selects_r1].contains(&id),
                        "{change:?} read entity {id} while mapping"
                    );
                }
            }
        }
    }
}

/// A tool change only rebuilds the toolsets whose newest instance follows the tool; a toolset
/// whose older instance followed it but whose newest doesn't builds nothing and its event is
/// finished
#[tokio::test]
async fn tool_change_rebuilds_only_newest_followers() {
    // two toolsets that both followed the tool once, one of which dropped it since
    let mut api = FakeApi::default();
    let groups = ["minerva-tn1"];
    let (tool_id, _) = tool(&mut api.graph, "parser", &groups);
    let keeps = tool_set(&mut api.graph, "keeps", &groups, true, None);
    let dropped = tool_set(&mut api.graph, "dropped", &groups, true, None);
    for tool_set in [keeps, dropped] {
        api.graph.edge(AssociationKind::Includes, tool_set, tool_id);
        store(&mut api.graph, tool_set, 1).await;
    }
    api.graph
        .remove_edge(AssociationKind::Includes, dropped, tool_id);
    store(&mut api.graph, dropped, 2).await;
    // the tool gets a new latest instance
    let latest = publish(&mut api.graph, tool_id, "2.0.0");
    let ev = event(
        "io-tn1",
        &groups,
        WorkflowGraphChange::ToolUpdated {
            tool: tool_id,
            tool_instance: latest,
        },
    );
    let outcome = instancer(&api).process(std::slice::from_ref(&ev)).await;
    // only the toolset that still follows the tool is rebuilt, at its next revision
    assert_eq!(built(&api), vec![keeps]);
    let created = api.created_tool_sets();
    assert_eq!(created[0].revision, 2);
    assert_eq!(created[0].members[0].tool_instance, latest);
    assert_eq!(outcome.cleared, vec![ev.id]);
}

/// The events for one toolset coalesce into one build recorded as the latest event's, with
/// every manual requester kept once; events for two toolsets give two builds
#[tokio::test]
async fn tool_set_events_coalesce() {
    // two toolsets
    let mut api = FakeApi::default();
    let groups = ["minerva-tc1"];
    let busy = tool_set(&mut api.graph, "busy", &groups, true, None);
    let quiet = tool_set(&mut api.graph, "quiet", &groups, true, None);
    // three edits by io, a request by callisto, two edits by europa, and an edit of the other
    let changed = |tool_set| WorkflowGraphChange::ToolSetChanged { tool_set };
    let manual = WorkflowGraphChange::ManualToolSetInstance { tool_set: busy };
    let events = vec![
        event("io-tc1", &groups, changed(busy)),
        event("io-tc1", &groups, changed(busy)),
        event("callisto-tc1", &groups, manual.clone()),
        event("io-tc1", &groups, changed(busy)),
        event("callisto-tc1", &groups, manual),
        event("europa-tc1", &groups, changed(busy)),
        event("europa-tc1", &groups, changed(busy)),
        event("io-tc1", &groups, changed(quiet)),
    ];
    let mapping = instancer(&api).affected(&events).await;
    // one build per toolset
    assert_eq!(
        mapping.tool_sets.keys().copied().collect::<Vec<Uuid>>(),
        sorted(vec![busy, quiet])
    );
    let coalesced = &mapping.tool_sets[&busy];
    assert_eq!(coalesced.events.len(), 7);
    let cause = coalesced.decide(true, None, None).unwrap();
    assert_eq!(cause.triggered_by, "europa-tc1");
    assert_eq!(cause.trigger, ToolSetInstanceTrigger::ToolSetUpdated);
    assert_eq!(coalesced.requesters(), vec!["callisto-tc1".to_owned()]);
    assert_eq!(
        cause.recipients,
        vec!["europa-tc1".to_owned(), "callisto-tc1".to_owned()]
    );
    // processing builds each toolset once and tells the busy toolset's users once each
    let outcome = instancer(&api).process(&events).await;
    assert_eq!(built(&api), sorted(vec![busy, quiet]));
    let busy_r1 = api
        .created_tool_sets()
        .into_iter()
        .find(|instance| instance.tool_set == busy)
        .unwrap();
    assert_eq!(busy_r1.triggered_by, "europa-tc1");
    let mut told = api
        .notes()
        .into_iter()
        .map(|(user, _)| user)
        .collect::<Vec<String>>();
    told.sort();
    assert_eq!(told, vec!["callisto-tc1", "europa-tc1", "io-tc1"]);
    assert_eq!(outcome.cleared.len(), events.len());
}

/// A batch builds its toolsets before its workflows: a tool publish that a toolset and a workflow
/// both follow creates the toolset instance first, and the event is finished once both are built
#[tokio::test]
async fn tool_set_phase_runs_before_workflows() {
    // a toolset and a workflow that both follow a tool at latest
    let mut api = FakeApi::default();
    let groups = ["minerva-tp1"];
    let (tool_id, _) = tool(&mut api.graph, "parser", &groups);
    let kit = tool_set(&mut api.graph, "kit", &groups, true, None);
    api.graph.edge(AssociationKind::Includes, kit, tool_id);
    store(&mut api.graph, kit, 1).await;
    let catalog = entity(
        EntityKinds::WorkflowCatalog,
        "flow",
        &groups,
        EntityMetadata::WorkflowCatalog(WorkflowCatalogEntity {
            auto_instance: true,
            ..WorkflowCatalogEntity::default()
        }),
    );
    let workflow = catalog.id;
    let stored = entity(
        EntityKinds::WorkflowInstance,
        "flow#r1",
        &groups,
        EntityMetadata::WorkflowInstance(generators::gen_workflow_instance(&catalog, 1, None, &[])),
    );
    let stored_id = stored.id;
    api.graph.insert(catalog);
    api.graph.insert(stored);
    api.graph
        .tag(stored_id, FOLLOWS_TOOL_TAG, &tool_id.to_string());
    if let Some(EntityMetadata::WorkflowCatalog(meta)) =
        api.graph.entity_mut(workflow).map(|e| &mut e.metadata)
    {
        meta.newest_instance = Some(stored_id);
        meta.highest_revision = 1;
    }
    // the tool is published
    let latest = publish(&mut api.graph, tool_id, "2.0.0");
    let ev = event(
        "io-tp1",
        &groups,
        WorkflowGraphChange::ToolUpdated {
            tool: tool_id,
            tool_instance: latest,
        },
    );
    let outcome = process_batch(
        &api,
        CrawlLimits::default(),
        ToolSetLimits::default(),
        std::slice::from_ref(&ev),
    )
    .await;
    // the toolset was built before the workflow, and the event is finished once
    assert_eq!(api.order(), vec![Created::ToolSet, Created::Workflow]);
    assert_eq!(outcome.cleared, vec![ev.id]);
    assert_eq!(outcome.stats.events_processed, 1);
    assert_eq!(outcome.stats.tool_set_instances_created, 1);
    assert_eq!(outcome.stats.instances_created, 1);
    // a toolset event that fails keeps the event in flight even though the workflow phase has
    // nothing to do with it
    let mut failing = FakeApi::default();
    let broken = tool_set(&mut failing.graph, "broken", &groups, true, None);
    failing.graph.fail_reads(broken, Some(500));
    let ev = event(
        "io-tp1",
        &groups,
        WorkflowGraphChange::ToolSetChanged { tool_set: broken },
    );
    let outcome = process_batch(
        &failing,
        CrawlLimits::default(),
        ToolSetLimits::default(),
        std::slice::from_ref(&ev),
    )
    .await;
    assert_eq!(outcome.in_flight, vec![ev.id]);
    assert_eq!(outcome.cleared, Vec::<Uuid>::new());
}

/// A failed read leaves only its toolset's events in flight: a 500 or a connection error on the
/// toolset read, a member deleted while it was read, a failed tool read, and a failed lookup keep
/// their events with no instance and no notification; a deleted toolset clears its events with
/// no instance and no notification; a healthy toolset in the same batch is built and cleared.
/// Once the reads recover, the next pass builds the toolset once and clears its events
#[tokio::test]
// one scenario walks every failure and the recovery
#[allow(clippy::too_many_lines)]
async fn failed_tool_set_reads_keep_events_in_flight() {
    // toolsets whose reads fail in different ways, a deleted one, and a healthy one
    let mut api = FakeApi::default();
    let groups = ["minerva-tf1"];
    let errored = tool_set(&mut api.graph, "errored", &groups, true, None);
    let unreachable = tool_set(&mut api.graph, "unreachable", &groups, true, None);
    let raced = tool_set(&mut api.graph, "raced", &groups, true, None);
    let healthy = tool_set(&mut api.graph, "healthy", &groups, true, None);
    let deleted = Uuid::new_v4();
    api.graph.row(
        raced,
        GraphEdge {
            kind: AssociationKind::Includes,
            direction: Directionality::To,
            other: Uuid::new_v4(),
            other_kind: EntityKinds::Tool,
        },
    );
    api.graph.fail_reads(errored, Some(500));
    api.graph.fail_reads(unreachable, None);
    // one edit of each
    let changed = |tool_set| WorkflowGraphChange::ToolSetChanged { tool_set };
    let events = [errored, unreachable, raced, deleted, healthy]
        .into_iter()
        .map(|tool_set| event("io-tf1", &groups, changed(tool_set)))
        .collect::<Vec<Event>>();
    let outcome = instancer(&api).process(&events).await;
    // the failed reads keep their events, and the deleted and healthy toolsets are cleared
    let refs = events.iter().collect::<Vec<&Event>>();
    assert_eq!(sorted(outcome.in_flight.clone()), sorted_ids(&refs[0..3]));
    assert_eq!(sorted(outcome.cleared.clone()), sorted_ids(&refs[3..5]));
    // only the healthy toolset was built and only its user was told
    assert_eq!(built(&api), vec![healthy]);
    assert_eq!(api.notes().len(), 1);
    // once the toolset reads again, the retry builds it once and clears its event
    api.graph.recover_reads(errored);
    let retry = instancer(&api).process(&events[0..1]).await;
    assert_eq!(built(&api), sorted(vec![healthy, errored]));
    assert_eq!(retry.cleared, vec![events[0].id]);
    assert_eq!(retry.in_flight, Vec::<Uuid>::new());
    // a failed read of a published tool keeps its event
    let mut tools = FakeApi::default();
    let (tool_id, _) = tool(&mut tools.graph, "parser", &groups);
    tools.graph.fail_reads(tool_id, Some(500));
    let ev = event(
        "io-tf1",
        &groups,
        WorkflowGraphChange::ToolUpdated {
            tool: tool_id,
            tool_instance: Uuid::new_v4(),
        },
    );
    let outcome = instancer(&tools).process(std::slice::from_ref(&ev)).await;
    assert_eq!(outcome.in_flight, vec![ev.id]);
    // a failed lookup keeps its event
    let mut lookups = FakeApi::default();
    lookups.graph.fail_tag_lookups(Some(500));
    let ev = event(
        "io-tf1",
        &groups,
        WorkflowGraphChange::ToolTagsChanged {
            tool: Uuid::new_v4(),
            tags: BTreeMap::from([("family".to_owned(), BTreeSet::from(["x".to_owned()]))]),
        },
    );
    let outcome = instancer(&lookups).process(std::slice::from_ref(&ev)).await;
    assert_eq!(outcome.in_flight, vec![ev.id]);
    assert_eq!(lookups.created_tool_sets(), Vec::new());
}

/// Every toolset counter goes up once per occurrence: created and invalid instances and dedupe
/// skips, beside the shared processed, sent, and failed notification counters; the notices name
/// the toolset, revision, and instance, list an invalid instance's errors, and tell a requester
/// which instance is current
#[tokio::test]
// one scenario builds every outcome in one batch
#[allow(clippy::too_many_lines)]
async fn tool_set_stats_count_each_occurrence() {
    // a toolset built for the first time, one whose newest instance is current, and one whose
    // included tool has no latest instance
    let mut api = FakeApi {
        failing_notes: BTreeSet::from(["callisto-ts1".to_owned()]),
        ..FakeApi::default()
    };
    let groups = ["minerva-ts1"];
    let fresh = tool_set(&mut api.graph, "fresh", &groups, true, None);
    let current = tool_set(&mut api.graph, "current", &groups, true, None);
    let current_r1 = store(&mut api.graph, current, 1).await;
    let broken = tool_set(&mut api.graph, "broken", &groups, true, None);
    let (bare, _) = tool(&mut api.graph, "bare", &groups);
    if let Some(EntityMetadata::Tool(meta)) = api.graph.entity_mut(bare).map(|e| &mut e.metadata) {
        meta.latest = None;
    }
    api.graph.edge(AssociationKind::Includes, broken, bare);
    // edits of each, and requests for the current one by a user whose notice fails
    let changed = |tool_set| WorkflowGraphChange::ToolSetChanged { tool_set };
    let manual = WorkflowGraphChange::ManualToolSetInstance { tool_set: current };
    let events = vec![
        event("io-ts1", &groups, changed(fresh)),
        event("io-ts1", &groups, changed(current)),
        event("io-ts1", &groups, changed(broken)),
        event("europa-ts1", &groups, manual.clone()),
        event("callisto-ts1", &groups, manual),
    ];
    let outcome = instancer(&api).process(&events).await;
    // two instances created, one of them invalid, one dedupe skip, three notices sent and one
    // failed
    let expected = AiWorkerStats {
        events_processed: 5,
        tool_set_instances_created: 2,
        tool_set_skipped: 1,
        tool_set_invalid: 1,
        notifications_sent: 3,
        notification_failures: 1,
        ..outcome.stats.clone()
    };
    assert_eq!(outcome.stats, expected);
    assert_eq!(outcome.stats.instances_created, 0);
    assert_eq!(outcome.cleared.len(), 5);
    // each created instance records its user and trigger
    for instance in api.created_tool_sets() {
        assert_eq!(instance.triggered_by, "io-ts1");
        assert_eq!(instance.trigger, ToolSetInstanceTrigger::ToolSetUpdated);
        assert_eq!(instance.revision, 1);
    }
    let notes = api.notes();
    // the ready build is an Info notice naming the toolset and revision
    let ready = notes
        .iter()
        .find(|(_, note)| note.msg.contains("`fresh`"))
        .expect("the ready build wasn't announced");
    assert_eq!(ready.0, "io-ts1");
    assert_eq!(ready.1.level, NotificationLevel::Info);
    assert!(ready.1.msg.contains("#r1"), "{}", ready.1.msg);
    assert!(ready.1.msg.contains("Ready"), "{}", ready.1.msg);
    // the invalid build is an Error notice listing its errors
    let invalid = notes
        .iter()
        .find(|(_, note)| note.msg.contains("`broken`"))
        .expect("the invalid build wasn't announced");
    assert_eq!(invalid.1.level, NotificationLevel::Error);
    assert!(invalid.1.msg.contains("Invalid"), "{}", invalid.1.msg);
    assert!(
        invalid.1.msg.contains("no latest instance"),
        "{}",
        invalid.1.msg
    );
    // the requester is told the newest instance is current
    let skipped = notes
        .iter()
        .find(|(user, _)| user == "europa-ts1")
        .expect("the requester wasn't told");
    assert_eq!(skipped.1.level, NotificationLevel::Info);
    assert!(
        skipped.1.msg.contains(&current_r1.to_string()),
        "{}",
        skipped.1.msg
    );
}

/// A toolset with automatic instances off is only built by a manual request, and a create that
/// loses a revision race is rebuilt; conflicts that never end leave the events in flight
#[tokio::test]
async fn manual_only_tool_sets_and_revision_races() {
    // a toolset that doesn't instance automatically
    let mut api = FakeApi::default();
    let groups = ["minerva-tr1"];
    let manual_only = tool_set(&mut api.graph, "manual-only", &groups, false, None);
    // an edit builds nothing but is finished
    let edit = event(
        "io-tr1",
        &groups,
        WorkflowGraphChange::ToolSetChanged {
            tool_set: manual_only,
        },
    );
    let outcome = instancer(&api).process(std::slice::from_ref(&edit)).await;
    assert_eq!(api.created_tool_sets(), Vec::new());
    assert_eq!(outcome.cleared, vec![edit.id]);
    // a manual request builds it, even after losing one revision race
    *api.conflicts.lock().unwrap() = 1;
    let request = event(
        "io-tr1",
        &groups,
        WorkflowGraphChange::ManualToolSetInstance {
            tool_set: manual_only,
        },
    );
    let outcome = instancer(&api)
        .process(std::slice::from_ref(&request))
        .await;
    assert_eq!(built(&api), vec![manual_only]);
    assert_eq!(
        api.created_tool_sets()[0].trigger,
        ToolSetInstanceTrigger::Manual
    );
    assert_eq!(outcome.cleared, vec![request.id]);
    // conflicts that never end leave the request in flight
    *api.conflicts.lock().unwrap() = usize::MAX;
    let outcome = instancer(&api)
        .process(std::slice::from_ref(&request))
        .await;
    assert_eq!(outcome.in_flight, vec![request.id]);
}
