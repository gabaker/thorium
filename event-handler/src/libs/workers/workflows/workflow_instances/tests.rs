//! Tests for mapping, coalescing, and building workflows from graph events over an in-memory graph

use chrono::Utc;
use std::collections::BTreeSet;
use thorium::models::entities::workflows::{
    GraphRead, INSTANCE_AGENT_TAG, MemoryGraph, WORKFLOW_ID_TAG, resolve_workflow,
};
use thorium::models::{
    AgentEntity, AssociationKind, CrawlLimits, EntityKinds, EntityMetadata, Event,
    NotificationLevel, WorkflowCatalogEntity, WorkflowGraphChange, WorkflowInstanceTrigger,
};
use thorium::test_utilities::generators;
use uuid::Uuid;

use super::WorkflowInstancer;
use crate::libs::workers::workflows::fake::{FakeApi, entity, event};
use crate::libs::workers::workflows::{WorkflowsWorkerStats, retry_after};

/// Add a workflow catalog to a graph
///
/// # Arguments
///
/// * `graph` - The graph to add to
/// * `name` - The workflow's name
/// * `groups` - The workflow's groups
/// * `auto_instance` - Whether changes build instances automatically
fn catalog(graph: &mut MemoryGraph, name: &str, groups: &[&str], auto_instance: bool) -> Uuid {
    // a workflow with no agents
    let meta = WorkflowCatalogEntity {
        auto_instance,
        ..WorkflowCatalogEntity::default()
    };
    let workflow = entity(
        EntityKinds::WorkflowCatalog,
        name,
        groups,
        EntityMetadata::WorkflowCatalog(meta),
    );
    let id = workflow.id;
    graph.insert(workflow);
    id
}

/// Add an agent to a graph
///
/// # Arguments
///
/// * `graph` - The graph to add to
/// * `name` - The agent's name
/// * `groups` - The agent's groups
fn agent(graph: &mut MemoryGraph, name: &str, groups: &[&str]) -> Uuid {
    // an agent with a generated config
    let meta = AgentEntity {
        config: generators::gen_agent_config(name),
        ..AgentEntity::default()
    };
    let agent = entity(
        EntityKinds::Agent,
        name,
        groups,
        EntityMetadata::Agent(meta),
    );
    let id = agent.id;
    graph.insert(agent);
    id
}

/// Add an instance of a workflow at a revision, tagged in its groups, and make it the workflow's
/// newest instance when asked
///
/// # Arguments
///
/// * `graph` - The graph to add to
/// * `workflow` - The workflow the instance belongs to
/// * `revision` - The instance's revision
/// * `tags` - The intrinsic tags the instance carries
/// * `newest` - Whether the workflow names it as its newest instance
fn instance(
    graph: &mut MemoryGraph,
    workflow: Uuid,
    revision: u64,
    tags: &[(&str, Uuid)],
    newest: bool,
) -> Uuid {
    // build an instance of the workflow with no agents
    let catalog = graph.entity(workflow).unwrap().clone();
    let meta = generators::gen_workflow_instance(&catalog, revision, None, &[]);
    let groups = catalog
        .groups
        .iter()
        .map(String::as_str)
        .collect::<Vec<&str>>();
    let stored = entity(
        EntityKinds::WorkflowInstance,
        &format!("{}#r{revision}", catalog.name),
        &groups,
        EntityMetadata::WorkflowInstance(meta),
    );
    let id = stored.id;
    graph.insert(stored);
    // tag it with its workflow and the given tags in every group of its workflow
    graph.tag(id, WORKFLOW_ID_TAG, &workflow.to_string());
    for (key, value) in tags {
        graph.tag(id, key, &value.to_string());
    }
    // point the workflow at it
    if newest
        && let Some(entity) = graph.entity_mut(workflow)
        && let EntityMetadata::WorkflowCatalog(meta) = &mut entity.metadata
    {
        meta.newest_instance = Some(id);
        meta.highest_revision = meta.highest_revision.max(revision);
    }
    id
}

/// Build an instancer over a fake API with the default limits
///
/// # Arguments
///
/// * `api` - The fake API
fn instancer(api: &FakeApi) -> WorkflowInstancer<'_, FakeApi> {
    // the default crawl limits
    WorkflowInstancer::new(api, CrawlLimits::default())
}

/// Get the ids of the workflows the created instances belong to
///
/// # Arguments
///
/// * `api` - The fake API
fn built(api: &FakeApi) -> Vec<Uuid> {
    // each created instance's workflow, sorted
    let mut workflows = api
        .created()
        .iter()
        .map(|instance| instance.workflow)
        .collect::<Vec<Uuid>>();
    workflows.sort();
    workflows
}

/// Get the sorted users who were notified about a workflow
///
/// # Arguments
///
/// * `api` - The fake API
/// * `name` - The workflow's name
fn notified(api: &FakeApi, name: &str) -> Vec<String> {
    // the users of every note naming the workflow, sorted
    let named = format!("`{name}`");
    let mut users = api
        .notes()
        .into_iter()
        .filter(|(_, note)| note.msg.contains(&named))
        .map(|(user, _)| user)
        .collect::<Vec<String>>();
    users.sort();
    users
}

/// The latest qualifying event decides a coalesced build: an agent change that only matched an
/// older revision never takes the trigger or the user from an earlier edit of the workflow, and
/// with automatic instances off only a manual request decides; only the deciding user and the
/// manual requesters are notified
#[tokio::test]
async fn latest_qualifying_event_decides_the_build() {
    // a workflow whose first revision used the agent and whose newest doesn't, and a workflow
    // with automatic instances off
    let mut api = FakeApi::default();
    let groups = ["minerva-q1"];
    let agent_id = Uuid::new_v4();
    let edited = catalog(&mut api.graph, "edited", &groups, true);
    instance(
        &mut api.graph,
        edited,
        1,
        &[(INSTANCE_AGENT_TAG, agent_id)],
        false,
    );
    instance(&mut api.graph, edited, 2, &[], true);
    let manual_only = catalog(&mut api.graph, "manual-only", &groups, false);
    // io edits the first workflow and callisto asks for the second, then callisto's agent change
    // reaches the first workflow's old revision and io edits the second
    let events = vec![
        event(
            "io-q1",
            &groups,
            WorkflowGraphChange::WorkflowUpdated { workflow: edited },
        ),
        event(
            "callisto-q1",
            &groups,
            WorkflowGraphChange::ManualInstance {
                workflow: manual_only,
            },
        ),
        event(
            "callisto-q1",
            &groups,
            WorkflowGraphChange::AgentUpdated { agent: agent_id },
        ),
        event(
            "io-q1",
            &groups,
            WorkflowGraphChange::WorkflowUpdated {
                workflow: manual_only,
            },
        ),
    ];
    let outcome = instancer(&api).process(&events).await;
    // the edit decides the first build and the manual request decides the second
    let created = api.created();
    let edited_build = created.iter().find(|i| i.workflow == edited).unwrap();
    assert_eq!(edited_build.triggered_by, "io-q1");
    assert_eq!(
        edited_build.trigger,
        WorkflowInstanceTrigger::WorkflowUpdated
    );
    let manual_build = created.iter().find(|i| i.workflow == manual_only).unwrap();
    assert_eq!(manual_build.triggered_by, "callisto-q1");
    assert_eq!(manual_build.trigger, WorkflowInstanceTrigger::Manual);
    // each build tells only the user who decided it
    assert_eq!(notified(&api, "edited"), vec!["io-q1"]);
    assert_eq!(notified(&api, "manual-only"), vec!["callisto-q1"]);
    assert_eq!(outcome.cleared.len(), events.len());
}

/// A lookup that missed a workflow's newest instance, as when it's created after the lookup ran,
/// still rebuilds the workflow when that instance carries the looked-up tag, and a workflow whose
/// newest instance lacks the tag still builds nothing
#[tokio::test]
async fn newest_instance_missed_by_a_lookup_is_checked() {
    // a workflow whose newest instance carries the agent's tag only where the lookup doesn't
    // search, and one whose newest instance dropped the agent
    let mut api = FakeApi::default();
    let groups = ["minerva-q2", "mars-q2"];
    let agent_id = Uuid::new_v4();
    let raced = catalog(&mut api.graph, "raced", &groups, true);
    instance(
        &mut api.graph,
        raced,
        1,
        &[(INSTANCE_AGENT_TAG, agent_id)],
        false,
    );
    let newest = instance(&mut api.graph, raced, 2, &[], true);
    api.graph.tag_in(
        newest,
        INSTANCE_AGENT_TAG,
        &agent_id.to_string(),
        &["mars-q2"],
    );
    let dropped = catalog(&mut api.graph, "dropped", &groups, true);
    instance(
        &mut api.graph,
        dropped,
        1,
        &[(INSTANCE_AGENT_TAG, agent_id)],
        false,
    );
    instance(&mut api.graph, dropped, 2, &[], true);
    // the agent changes, looked up only in minerva
    let ev = event(
        "io-q2",
        &["minerva-q2"],
        WorkflowGraphChange::AgentUpdated { agent: agent_id },
    );
    let outcome = instancer(&api).process(std::slice::from_ref(&ev)).await;
    // only the workflow whose newest instance carries the tag is rebuilt
    assert_eq!(built(&api), vec![raced]);
    assert_eq!(
        api.created()[0].trigger,
        WorkflowInstanceTrigger::AgentUpdated { agent: agent_id }
    );
    assert_eq!(outcome.cleared, vec![ev.id]);
}

/// Each kind of graph change maps to the workflows it affects: the workflow it names, the
/// workflows whose newest instance is tagged with the agent, tool, or toolset it names, and none
/// for toolset changes; every tag lookup searches only the event's groups
#[tokio::test]
// one table drives every kind of change through the same checks
#[allow(clippy::too_many_lines)]
async fn every_change_maps_to_its_workflows() {
    // a workflow per lookup, each with a newest instance tagged with what it uses
    let mut api = FakeApi::default();
    let groups = ["minerva-m1", "mars-m1"];
    let named = catalog(&mut api.graph, "named", &groups, true);
    let by_agent = catalog(&mut api.graph, "by-agent", &groups, true);
    let by_tool = catalog(&mut api.graph, "by-tool", &groups, true);
    let by_set = catalog(&mut api.graph, "by-set", &groups, true);
    let (agent_id, tool, tool_set) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let agent_r1 = instance(
        &mut api.graph,
        by_agent,
        1,
        &[(INSTANCE_AGENT_TAG, agent_id)],
        true,
    );
    let tool_r1 = instance(
        &mut api.graph,
        by_tool,
        1,
        &[("InstanceFollowsTool", tool)],
        true,
    );
    let set_r1 = instance(
        &mut api.graph,
        by_set,
        1,
        &[("InstanceFollowsToolSet", tool_set)],
        true,
    );
    // one event of every kind
    let edge = |source: Uuid, kind: AssociationKind| WorkflowGraphChange::EdgeChanged {
        source,
        target: Uuid::new_v4(),
        kind,
    };
    let mut tags = std::collections::BTreeMap::new();
    tags.insert("family".to_owned(), BTreeSet::from(["parser".to_owned()]));
    let cases = vec![
        (
            WorkflowGraphChange::WorkflowUpdated { workflow: named },
            vec![named],
            WorkflowInstanceTrigger::WorkflowUpdated,
        ),
        (
            WorkflowGraphChange::ManualInstance { workflow: named },
            vec![named],
            WorkflowInstanceTrigger::Manual,
        ),
        (
            edge(named, AssociationKind::Orchestrates),
            vec![named],
            WorkflowInstanceTrigger::WorkflowUpdated,
        ),
        (
            edge(agent_id, AssociationKind::DelegatesTo),
            vec![by_agent],
            WorkflowInstanceTrigger::AgentUpdated { agent: agent_id },
        ),
        (
            edge(agent_id, AssociationKind::Calls),
            vec![by_agent],
            WorkflowInstanceTrigger::AgentUpdated { agent: agent_id },
        ),
        (
            edge(agent_id, AssociationKind::RunsIn),
            vec![by_agent],
            WorkflowInstanceTrigger::AgentUpdated { agent: agent_id },
        ),
        (
            WorkflowGraphChange::AgentUpdated { agent: agent_id },
            vec![by_agent],
            WorkflowInstanceTrigger::AgentUpdated { agent: agent_id },
        ),
        (
            WorkflowGraphChange::ToolUpdated {
                tool,
                tool_instance: Uuid::new_v4(),
            },
            vec![by_tool],
            WorkflowInstanceTrigger::ToolUpdated {
                tool,
                tool_instance: Uuid::nil(),
            },
        ),
        (
            WorkflowGraphChange::ToolSetUpdated {
                tool_set,
                tool_set_instance: Uuid::nil(),
            },
            vec![by_set],
            WorkflowInstanceTrigger::ToolSetUpdated {
                tool_set,
                tool_set_instance: Uuid::nil(),
            },
        ),
        (
            edge(tool_set, AssociationKind::Includes),
            vec![],
            WorkflowInstanceTrigger::Manual,
        ),
        (
            WorkflowGraphChange::ToolSetChanged { tool_set },
            vec![],
            WorkflowInstanceTrigger::Manual,
        ),
        (
            WorkflowGraphChange::ManualToolSetInstance { tool_set },
            vec![],
            WorkflowInstanceTrigger::Manual,
        ),
        (
            WorkflowGraphChange::ToolTagsChanged { tool, tags },
            vec![],
            WorkflowInstanceTrigger::Manual,
        ),
    ];
    for (change, expected, trigger) in cases {
        // map one event on its own
        api.graph.clear_reads();
        let ev = event("io-m1", &groups, change.clone());
        let mapping = instancer(&api).affected(std::slice::from_ref(&ev)).await;
        let found = mapping.workflows.keys().copied().collect::<Vec<Uuid>>();
        assert_eq!(found, expected, "{change:?} mapped to the wrong workflows");
        assert!(mapping.failed.is_empty(), "{change:?} failed to map");
        // an event that affects nothing is still finished
        if expected.is_empty() {
            assert_eq!(mapping.unmapped, vec![ev.id], "{change:?} wasn't finished");
        }
        // each workflow records the event, and the event decides its trigger and user
        for affected in mapping.workflows.values() {
            let cause = affected.decide(true, None, None).unwrap();
            let mut want = trigger.clone();
            if let (
                WorkflowInstanceTrigger::ToolUpdated { tool_instance, .. },
                WorkflowGraphChange::ToolUpdated {
                    tool_instance: sent,
                    ..
                },
            ) = (&mut want, &change)
            {
                *tool_instance = *sent;
            }
            assert_eq!(cause.trigger, want, "{change:?} recorded the wrong trigger");
            assert_eq!(cause.triggered_by, "io-m1");
            assert_eq!(affected.events, vec![ev.id]);
            // only an event naming the workflow is direct; a lookup records the matched instance
            let direct = matches!(
                change,
                WorkflowGraphChange::WorkflowUpdated { .. }
                    | WorkflowGraphChange::ManualInstance { .. }
                    | WorkflowGraphChange::EdgeChanged {
                        kind: AssociationKind::Orchestrates,
                        ..
                    }
            );
            assert_eq!(affected.direct(), direct, "{change:?} directness");
            if !direct {
                let matched = [agent_r1, tool_r1, set_r1]
                    .into_iter()
                    .filter(|id| affected.matched().contains(id))
                    .count();
                assert_eq!(matched, 1, "{change:?} matched {:?}", affected.matched());
            }
        }
        // every lookup searched exactly the event's groups, and only the matched instances,
        // which name their workflows, were read by id
        for read in api.graph.reads() {
            match read {
                GraphRead::Tagged {
                    kind,
                    groups: searched,
                    ..
                } => {
                    assert_eq!(kind, EntityKinds::WorkflowInstance);
                    assert_eq!(searched, groups.map(str::to_owned).to_vec());
                }
                GraphRead::Node { id, .. } => assert!(
                    [agent_r1, tool_r1, set_r1].contains(&id),
                    "{change:?} read entity {id} while mapping"
                ),
            }
        }
    }
}

/// An agent change only affects the workflows whose newest instance is tagged with the agent;
/// a workflow whose older revision is tagged but whose newest isn't gets no build
#[tokio::test]
async fn agent_change_matches_only_newest_instances() {
    // two workflows that both used the agent once, one of which dropped it in its newest revision
    let mut api = FakeApi::default();
    let groups = ["minerva-n1"];
    let agent_id = Uuid::new_v4();
    let keeps = catalog(&mut api.graph, "keeps", &groups, true);
    let dropped = catalog(&mut api.graph, "dropped", &groups, true);
    instance(
        &mut api.graph,
        keeps,
        1,
        &[(INSTANCE_AGENT_TAG, agent_id)],
        false,
    );
    instance(
        &mut api.graph,
        keeps,
        2,
        &[(INSTANCE_AGENT_TAG, agent_id)],
        true,
    );
    instance(
        &mut api.graph,
        dropped,
        1,
        &[(INSTANCE_AGENT_TAG, agent_id)],
        false,
    );
    instance(&mut api.graph, dropped, 2, &[], true);
    // the agent changes
    let ev = event(
        "io-n1",
        &groups,
        WorkflowGraphChange::AgentUpdated { agent: agent_id },
    );
    let outcome = instancer(&api).process(std::slice::from_ref(&ev)).await;
    // only the workflow whose newest instance uses the agent is rebuilt
    assert_eq!(built(&api), vec![keeps]);
    assert_eq!(outcome.cleared, vec![ev.id]);
    assert_eq!(outcome.in_flight, Vec::<Uuid>::new());
}

/// Many events for one workflow make one build whose user is the latest event's, every manual
/// requester is notified once, and events for two workflows make two builds
#[tokio::test]
async fn events_coalesce_per_workflow() {
    // two workflows
    let mut api = FakeApi::default();
    let groups = ["minerva-c1"];
    let first = catalog(&mut api.graph, "first", &groups, true);
    let second = catalog(&mut api.graph, "second", &groups, true);
    // edits by io then europa, manual requests by callisto twice and ganymede, all for one
    // workflow, then one edit of the other workflow
    let updated = |workflow| WorkflowGraphChange::WorkflowUpdated { workflow };
    let manual = |workflow| WorkflowGraphChange::ManualInstance { workflow };
    let events = vec![
        event("io-c1", &groups, updated(first)),
        event("callisto-c1", &groups, manual(first)),
        event("io-c1", &groups, updated(first)),
        event("ganymede-c1", &groups, manual(first)),
        event("callisto-c1", &groups, manual(first)),
        event("europa-c1", &groups, updated(first)),
        event("io-c1", &groups, updated(second)),
    ];
    let outcome = instancer(&api).process(&events).await;
    // one build per workflow, each recording its latest event's user and trigger
    let mut expected = vec![first, second];
    expected.sort();
    assert_eq!(built(&api), expected);
    let created = api.created();
    let first_build = created.iter().find(|i| i.workflow == first).unwrap();
    assert_eq!(first_build.triggered_by, "europa-c1");
    assert_eq!(
        first_build.trigger,
        WorkflowInstanceTrigger::WorkflowUpdated
    );
    // the first workflow's build notifies its user and each manual requester once
    let mut first_notes = api
        .notes()
        .into_iter()
        .filter(|(_, note)| note.msg.contains("`first`"))
        .map(|(user, _)| user)
        .collect::<Vec<String>>();
    first_notes.sort();
    assert_eq!(first_notes, vec!["callisto-c1", "europa-c1", "ganymede-c1"]);
    // every event is finished and counted
    assert_eq!(outcome.cleared.len(), events.len());
    assert_eq!(outcome.stats.events_processed, events.len());
    assert_eq!(outcome.stats.instances_created, 2);
}

/// A manual request coalesced after an edit makes the requester the build's user
#[tokio::test]
async fn latest_manual_request_sets_the_user() {
    // an edit by io, then a manual request by europa
    let mut api = FakeApi::default();
    let groups = ["minerva-c2"];
    let workflow = catalog(&mut api.graph, "flow", &groups, true);
    let events = vec![
        event(
            "io-c2",
            &groups,
            WorkflowGraphChange::WorkflowUpdated { workflow },
        ),
        event(
            "europa-c2",
            &groups,
            WorkflowGraphChange::ManualInstance { workflow },
        ),
    ];
    instancer(&api).process(&events).await;
    // the build is manual and europa's
    let created = api.created();
    assert_eq!(created.len(), 1);
    assert_eq!(created[0].triggered_by, "europa-c2");
    assert_eq!(created[0].trigger, WorkflowInstanceTrigger::Manual);
}

/// A failed read leaves only its workflow's events in flight: a 500 or a connection error on an
/// agent read, and a missing agent the workflow's edge leads to, keep the events; a deleted
/// workflow clears its events with no instance and no notification; a failed tag lookup keeps
/// its event; a healthy workflow in the same batch is built and cleared
#[tokio::test]
async fn failed_reads_keep_events_in_flight() {
    // workflows whose agents fail to read in different ways, a deleted one, and a healthy one
    let mut api = FakeApi::default();
    let groups = ["minerva-f1"];
    let errored = catalog(&mut api.graph, "errored", &groups, true);
    let unreachable = catalog(&mut api.graph, "unreachable", &groups, true);
    let raced = catalog(&mut api.graph, "raced", &groups, true);
    let healthy = catalog(&mut api.graph, "healthy", &groups, true);
    let deleted = Uuid::new_v4();
    let errored_agent = agent(&mut api.graph, "errored_agent", &groups);
    let unreachable_agent = agent(&mut api.graph, "unreachable_agent", &groups);
    let gone_agent = Uuid::new_v4();
    api.graph
        .edge(AssociationKind::Orchestrates, errored, errored_agent);
    api.graph.edge(
        AssociationKind::Orchestrates,
        unreachable,
        unreachable_agent,
    );
    api.graph
        .edge(AssociationKind::Orchestrates, raced, gone_agent);
    api.graph.fail_reads(errored_agent, Some(500));
    api.graph.fail_reads(unreachable_agent, None);
    // one edit of each
    let updated = |workflow| WorkflowGraphChange::WorkflowUpdated { workflow };
    let events = [errored, unreachable, raced, deleted, healthy]
        .into_iter()
        .map(|workflow| event("io-f1", &groups, updated(workflow)))
        .collect::<Vec<Event>>();
    let outcome = instancer(&api).process(&events).await;
    // the failed reads keep their events, and the deleted and healthy workflows are cleared
    let ids = |range: std::ops::Range<usize>| {
        let mut ids = events[range].iter().map(|e| e.id).collect::<Vec<Uuid>>();
        ids.sort();
        ids
    };
    let mut in_flight = outcome.in_flight.clone();
    in_flight.sort();
    let mut cleared = outcome.cleared.clone();
    cleared.sort();
    assert_eq!(in_flight, ids(0..3));
    assert_eq!(cleared, ids(3..5));
    // only the healthy workflow was built and only its user was told
    assert_eq!(built(&api), vec![healthy]);
    assert_eq!(api.notes().len(), 1);
    // a failed lookup keeps its event and finishes nothing
    let mut lookups = FakeApi::default();
    lookups.graph.fail_tag_lookups(Some(500));
    let ev = event(
        "io-f1",
        &groups,
        WorkflowGraphChange::AgentUpdated {
            agent: Uuid::new_v4(),
        },
    );
    let outcome = instancer(&lookups).process(std::slice::from_ref(&ev)).await;
    assert_eq!(outcome.in_flight, vec![ev.id]);
    assert_eq!(outcome.cleared, Vec::<Uuid>::new());
    // an event shared by a failed and a healthy workflow stays in flight
    let mut shared = FakeApi::default();
    let agent_id = agent(&mut shared.graph, "shared_agent", &groups);
    let ok = catalog(&mut shared.graph, "ok", &groups, true);
    let bad = catalog(&mut shared.graph, "bad", &groups, true);
    let bad_agent = agent(&mut shared.graph, "bad_agent", &groups);
    shared
        .graph
        .edge(AssociationKind::Orchestrates, bad, bad_agent);
    shared.graph.fail_reads(bad_agent, Some(503));
    instance(
        &mut shared.graph,
        ok,
        1,
        &[(INSTANCE_AGENT_TAG, agent_id)],
        true,
    );
    instance(
        &mut shared.graph,
        bad,
        1,
        &[(INSTANCE_AGENT_TAG, agent_id)],
        true,
    );
    let ev = event(
        "io-f1",
        &groups,
        WorkflowGraphChange::AgentUpdated { agent: agent_id },
    );
    let outcome = instancer(&shared).process(std::slice::from_ref(&ev)).await;
    assert_eq!(outcome.in_flight, vec![ev.id]);
    assert_eq!(built(&shared), vec![ok]);
}

/// A worker that left events in flight sets one retry time and keeps the earliest one
#[test]
fn retry_time_follows_in_flight_events() {
    // nothing in flight sets no retry time
    assert!(retry_after(false, None).is_none());
    // events in flight set a retry time a few minutes out
    let now = Utc::now();
    let first = retry_after(true, None).expect("no retry time for events in flight");
    assert!(first > now && first <= now + chrono::Duration::minutes(5));
    // a later failure keeps the earlier retry time
    assert_eq!(retry_after(true, Some(first)), Some(first));
    assert_eq!(retry_after(false, Some(first)), Some(first));
}

/// A workflow with automatic instances turned off is only built by a manual request
#[tokio::test]
async fn auto_instance_off_needs_a_manual_request() {
    // a workflow that doesn't instance automatically
    let mut api = FakeApi::default();
    let groups = ["minerva-a1"];
    let workflow = catalog(&mut api.graph, "manual-only", &groups, false);
    // an edit builds nothing but is finished
    let edit = event(
        "io-a1",
        &groups,
        WorkflowGraphChange::WorkflowUpdated { workflow },
    );
    let outcome = instancer(&api).process(std::slice::from_ref(&edit)).await;
    assert_eq!(api.created(), Vec::new());
    assert_eq!(outcome.cleared, vec![edit.id]);
    // a manual request builds it
    let request = event(
        "io-a1",
        &groups,
        WorkflowGraphChange::ManualInstance { workflow },
    );
    instancer(&api)
        .process(std::slice::from_ref(&request))
        .await;
    assert_eq!(built(&api), vec![workflow]);
}

/// Every counter goes up once per occurrence: processed events, created and invalid instances,
/// dedupe skips, sent notifications, and failed notifications
#[tokio::test]
async fn stats_count_each_occurrence() {
    // a workflow built for the first time, and one whose newest instance is already current
    let mut api = FakeApi {
        failing_notes: BTreeSet::from(["callisto-s1".to_owned()]),
        ..FakeApi::default()
    };
    let groups = ["minerva-s1"];
    let fresh = catalog(&mut api.graph, "fresh", &groups, true);
    let current = catalog(&mut api.graph, "current", &groups, true);
    let resolved = resolve_workflow(&api.graph, current, &CrawlLimits::default())
        .await
        .unwrap();
    let mut stored = resolved.instance.clone();
    stored.revision = 1;
    let catalog_entity = api.graph.entity(current).unwrap().clone();
    let entity = entity(
        EntityKinds::WorkflowInstance,
        "current#r1",
        &groups,
        EntityMetadata::WorkflowInstance(stored),
    );
    let stored_id = entity.id;
    api.graph.insert(entity);
    if let Some(found) = api.graph.entity_mut(current)
        && let EntityMetadata::WorkflowCatalog(meta) = &mut found.metadata
    {
        meta.newest_instance = Some(stored_id);
        meta.highest_revision = 1;
    }
    assert_eq!(catalog_entity.id, current);
    // edits of both, and a manual request of the current one by a user whose notification fails
    let events = vec![
        event(
            "io-s1",
            &groups,
            WorkflowGraphChange::WorkflowUpdated { workflow: fresh },
        ),
        event(
            "io-s1",
            &groups,
            WorkflowGraphChange::WorkflowUpdated { workflow: current },
        ),
        event(
            "europa-s1",
            &groups,
            WorkflowGraphChange::ManualInstance { workflow: current },
        ),
        event(
            "callisto-s1",
            &groups,
            WorkflowGraphChange::ManualInstance { workflow: current },
        ),
    ];
    let outcome = instancer(&api).process(&events).await;
    // one invalid instance created, one dedupe skip, a sent and a failed manual notice, and an
    // invalid notice for the fresh build
    let expected = WorkflowsWorkerStats {
        events_processed: 4,
        instances_created: 1,
        skipped: 1,
        invalid: 1,
        notifications_sent: 2,
        notification_failures: 1,
        ..outcome.stats.clone()
    };
    assert_eq!(outcome.stats, expected);
    // a failed notification still finishes every event
    assert_eq!(outcome.cleared.len(), 4);
    // the dedupe notice tells the requester the newest instance is current
    let notes = api.notes();
    let skipped = notes
        .iter()
        .find(|(user, _)| user == "europa-s1")
        .expect("the manual requester wasn't told");
    assert!(
        skipped.1.msg.contains(&stored_id.to_string()),
        "{}",
        skipped.1.msg
    );
    assert_eq!(skipped.1.level, NotificationLevel::Info);
    // the invalid build is an error naming its catalog, revision, id, and errors
    let invalid = notes
        .iter()
        .find(|(user, note)| user == "io-s1" && note.msg.contains("`fresh`"))
        .expect("the invalid build wasn't announced");
    assert_eq!(invalid.1.level, NotificationLevel::Error);
    assert!(invalid.1.msg.contains("#r1"), "{}", invalid.1.msg);
    assert!(invalid.1.msg.contains("has no agents"), "{}", invalid.1.msg);
    // stats add up across batches
    let mut total = WorkflowsWorkerStats::default();
    total += outcome.stats.clone();
    total += outcome.stats;
    assert_eq!(total.events_processed, 8);
}

/// A create refused with a revision conflict is rebuilt from a fresh read, so a lost race
/// creates nothing twice and still finishes its events
#[tokio::test]
async fn revision_conflict_rebuilds() {
    // a workflow whose first create loses a race
    let mut api = FakeApi::default();
    let groups = ["minerva-r1"];
    let workflow = catalog(&mut api.graph, "raced", &groups, true);
    *api.conflicts.lock().unwrap() = 1;
    let ev = event(
        "io-r1",
        &groups,
        WorkflowGraphChange::WorkflowUpdated { workflow },
    );
    let outcome = instancer(&api).process(std::slice::from_ref(&ev)).await;
    // the retried create went through and the event is finished
    assert_eq!(built(&api), vec![workflow]);
    assert_eq!(outcome.cleared, vec![ev.id]);
    // conflicts that never end leave the event in flight
    *api.conflicts.lock().unwrap() = usize::MAX;
    let ev = event(
        "io-r1",
        &groups,
        WorkflowGraphChange::WorkflowUpdated { workflow },
    );
    let outcome = instancer(&api).process(std::slice::from_ref(&ev)).await;
    assert_eq!(outcome.in_flight, vec![ev.id]);
}

/// Mapping an agent change that matched one instance of each of many workflows lists the tagged
/// instances once and reads each matched instance once, with no listing per workflow
#[tokio::test]
async fn mapping_many_single_revision_workflows() {
    // fifty workflows whose only instance follows one agent
    let mut api = FakeApi::default();
    let groups = ["minerva-w1"];
    let shared = agent(&mut api.graph, "shared", &groups);
    let mut workflows = BTreeSet::new();
    for index in 0..50 {
        let workflow = catalog(&mut api.graph, &format!("wide-{index}"), &groups, true);
        api.graph
            .edge(AssociationKind::Orchestrates, workflow, shared);
        instance(
            &mut api.graph,
            workflow,
            1,
            &[(INSTANCE_AGENT_TAG, shared)],
            true,
        );
        workflows.insert(workflow);
    }
    // the agent's change maps to every workflow with one lookup and one read per workflow
    let ev = event(
        "io-w1",
        &groups,
        WorkflowGraphChange::AgentUpdated { agent: shared },
    );
    let mapping = instancer(&api).affected(std::slice::from_ref(&ev)).await;
    assert_eq!(
        mapping
            .workflows
            .keys()
            .copied()
            .collect::<BTreeSet<Uuid>>(),
        workflows
    );
    let reads = api.graph.reads();
    let lookups = reads
        .iter()
        .filter(|read| matches!(read, GraphRead::Tagged { .. }))
        .count();
    let by_id = reads
        .iter()
        .filter(|read| matches!(read, GraphRead::Node { .. }))
        .count();
    assert_eq!((lookups, by_id), (1, 50), "{reads:?}");
}

/// Mapping an agent change lists the tagged instance ids in one lookup carrying both of the
/// agent's groups, reads one matched instance by id to find its workflow, and lists that
/// workflow's instance ids once, however many of its revisions matched; crawling a workflow where
/// three agents delegate to one sub-agent reads the sub-agent once
#[tokio::test]
async fn mapping_and_crawl_cost() {
    // a workflow whose three agents delegate to one sub-agent, tagged with all four
    let mut api = FakeApi::default();
    let groups = ["minerva-p1", "mars-p1"];
    let workflow = catalog(&mut api.graph, "fan-in", &groups, true);
    let sub = agent(&mut api.graph, "sub", &groups);
    let mut tags = vec![(INSTANCE_AGENT_TAG, sub)];
    for name in ["left", "middle", "right"] {
        let id = agent(&mut api.graph, name, &groups);
        api.graph.edge(AssociationKind::Orchestrates, workflow, id);
        api.graph.edge(AssociationKind::DelegatesTo, id, sub);
        tags.push((INSTANCE_AGENT_TAG, id));
    }
    let revisions = [
        instance(&mut api.graph, workflow, 1, &tags, false),
        instance(&mut api.graph, workflow, 2, &tags, false),
        instance(&mut api.graph, workflow, 3, &tags, true),
    ];
    // mapping the sub-agent's change is one lookup in both groups, one instance read, and one
    // listing of the workflow's instances
    let ev = event(
        "io-p1",
        &groups,
        WorkflowGraphChange::AgentUpdated { agent: sub },
    );
    let mapping = instancer(&api).affected(std::slice::from_ref(&ev)).await;
    assert_eq!(
        mapping.workflows.keys().copied().collect::<Vec<Uuid>>(),
        vec![workflow]
    );
    let reads = api.graph.reads();
    assert_eq!(reads.len(), 3, "{reads:?}");
    assert_eq!(
        reads[0],
        GraphRead::Tagged {
            kind: EntityKinds::WorkflowInstance,
            key: INSTANCE_AGENT_TAG.to_owned(),
            value: sub.to_string(),
            groups: groups.map(str::to_owned).to_vec(),
        }
    );
    assert!(
        matches!(reads[1], GraphRead::Node { id, .. } if revisions.contains(&id)),
        "{reads:?}"
    );
    assert_eq!(
        reads[2],
        GraphRead::Tagged {
            kind: EntityKinds::WorkflowInstance,
            key: WORKFLOW_ID_TAG.to_owned(),
            value: workflow.to_string(),
            groups: groups.map(str::to_owned).to_vec(),
        }
    );
    assert_eq!(
        mapping.workflows[&workflow].matched(),
        BTreeSet::from(revisions)
    );
    // the same events for one agent share one lookup
    api.graph.clear_reads();
    let repeats = (0..50)
        .map(|_| {
            event(
                "io-p1",
                &groups,
                WorkflowGraphChange::AgentUpdated { agent: sub },
            )
        })
        .collect::<Vec<Event>>();
    instancer(&api).affected(&repeats).await;
    assert_eq!(api.graph.reads().len(), 3);
    // building the workflow reads the shared sub-agent once
    api.graph.clear_reads();
    let outcome = instancer(&api).process(std::slice::from_ref(&ev)).await;
    assert_eq!(outcome.cleared, vec![ev.id]);
    assert_eq!(api.graph.node_reads(sub), 1);
    assert_eq!(outcome.stats.tag_lookups, 2);
    assert!(outcome.stats.graph_reads >= 5, "{:?}", outcome.stats);
}
