//! Agent swarms: fan independent tasks out to several sub-agents at once.
//!
//! The per-agent goal tools (see [`crate::thorpal::agents::tool`]) delegate a
//! single task and the main agent's tool calls run one after another, which is
//! the right shape for dependent work. A swarm instead takes a batch of
//! independent tasks in one `spawn_swarm` call and runs a sub-agent for each of
//! them concurrently, bounded per agent by its `max_swarm_size` and across the
//! whole run by the shared live sub-agent cap.

use std::collections::HashMap;
use std::sync::Arc;

use rig::Agent;
use rig::agent::tool::DynamicTool;
use rig::tool::{ToolExecutionError, ToolOutput};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Semaphore;
use tracing::Instrument;

use super::budget::TurnBudget;
use super::tool::{
    DelegationRegistry, Invocation, SubAgentRun, rerun_from_record, run_sub_agent,
    take_matching_record,
};
use crate::thorpal::coverage::CoverageLedger;
use crate::thorpal::events::{ThorPalEvent, ThorPalEventSender};
use crate::thorpal::store::{AgentRecord, SessionStore};
use crate::thorpal::tools::registry::EnabledTools;
use crate::thorpal::usage::UsageLedger;

/// The name of the tool the main agent calls to spawn a swarm
pub const SWARM_TOOL_NAME: &str = "spawn_swarm";

/// The fewest tasks a swarm may contain; a single task should use the agent's own tool
const MIN_SWARM_TASKS: usize = 2;

/// A sub-agent that can be included in a swarm
pub struct SwarmMember {
    /// The tool name the main agent calls this agent by (e.g. `reverse_engineer`)
    pub tool_name: String,
    /// The human-facing role prefix for this agent's display names (e.g. `ReverseEngineer`)
    pub role: String,
    /// A description of what this agent does, shown to the main agent
    pub description: String,
    /// The max number of copies of this agent a single swarm may include
    pub max_swarm_size: usize,
    /// The built sub-agent, shared with its single sub-agent tool
    pub agent: Arc<Agent>,
    /// The resolved registry tool names of this agent's own toolset
    pub enabled: EnabledTools,
    /// The turn-budget knobs each invocation of this member runs under
    pub budget: TurnBudget,
}

/// A single task within a swarm
#[derive(Debug, Deserialize)]
struct SwarmTask {
    /// The tool name of the agent to run this task with
    agent: String,
    /// The concrete task for the sub-agent to carry out
    prompt: String,
}

/// The arguments accepted by the swarm tool
#[derive(Debug, Deserialize)]
struct SwarmArgs {
    /// The overarching, top-level goal every task in this swarm serves
    goal: String,
    /// The independent tasks to run concurrently
    tasks: Vec<SwarmTask>,
}

/// Make sure a swarm's tasks are runnable by its members and within their limits
///
/// On success this returns the index of the member each task should run on, in
/// task order. Errors name the offending agent and limit so the model can
/// correct its call.
///
/// # Arguments
///
/// * `members` - The agents that can be included in a swarm
/// * `tasks` - The tasks the main agent asked to run
fn validate_tasks(members: &[SwarmMember], tasks: &[SwarmTask]) -> Result<Vec<usize>, String> {
    // a single task gains nothing from a swarm
    if tasks.len() < MIN_SWARM_TASKS {
        return Err(format!(
            "a swarm needs at least {MIN_SWARM_TASKS} tasks; call the agent's own tool for a single task"
        ));
    }
    // track how many tasks each member has been given
    let mut counts: HashMap<usize, usize> = HashMap::with_capacity(members.len());
    // resolve each task's agent to a member index
    let mut resolved = Vec::with_capacity(tasks.len());
    for task in tasks {
        // find the member this task targets
        let Some(index) = members.iter().position(|m| m.tool_name == task.agent) else {
            // list the valid agents so the model can pick one
            let valid: Vec<&str> = members.iter().map(|m| m.tool_name.as_str()).collect();
            return Err(format!(
                "unknown swarm agent '{}'; valid agents are: {}",
                task.agent,
                valid.join(", ")
            ));
        };
        // count this task against its member's swarm size limit
        let count = counts.entry(index).or_default();
        *count += 1;
        if *count > members[index].max_swarm_size {
            return Err(format!(
                "a swarm may include at most {} '{}' agents",
                members[index].max_swarm_size, task.agent
            ));
        }
        resolved.push(index);
    }
    Ok(resolved)
}

/// Render every swarm member's result into one report for the main agent
///
/// Results are listed in task order, each under its own heading with the task
/// it was given, so the main agent can match answers back to its questions.
///
/// # Arguments
///
/// * `tasks` - The tasks the swarm ran, in order
/// * `runs` - The outcome of each task, in the same order
fn render_results(tasks: &[SwarmTask], runs: &[SubAgentRun]) -> String {
    // count the failures up front for the summary line
    let failed = runs.iter().filter(|run| run.output.is_err()).count();
    // open with a summary so the main agent sees the overall outcome first
    let mut report = format!(
        "Swarm finished: {} succeeded, {} failed.\n",
        runs.len() - failed,
        failed
    );
    // add a section for each task in order
    for (i, (task, run)) in tasks.iter().zip(runs).enumerate() {
        // show either the sub-agent's answer or why it failed; a budget death
        // carries a salvaged partial handoff as its model-visible feedback
        let body = match &run.output {
            Ok(output) => output.clone(),
            Err(error) => match error.model_feedback() {
                Some(feedback) => format!("FAILED: {feedback}"),
                None => format!("FAILED: {error}"),
            },
        };
        report.push_str(&format!(
            "\n### Task {}: {}\nTask: {}\n\n{}\n",
            i + 1,
            run.name,
            task.prompt,
            body
        ));
    }
    report
}

/// Build the description shown to the main agent for the swarm tool
///
/// # Arguments
///
/// * `members` - The agents that can be included in a swarm
/// * `max_alive` - The max number of sub-agents alive at once across the run
fn describe(members: &[SwarmMember], max_alive: usize) -> String {
    // list every swarmable agent along with its swarm size limit
    let agents = members
        .iter()
        .map(|m| {
            format!(
                "- {} (at most {} per swarm): {}",
                m.tool_name, m.max_swarm_size, m.description
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "Spawn a swarm of sub-agents that work on independent tasks at the same time.

Provide `goal` (the overarching objective every task serves) and `tasks`, a list
of at least {MIN_SWARM_TASKS} tasks, each naming the `agent` to run it and the
concrete `prompt` for that agent. Every task runs concurrently and you get back
one report with each task's answer (or its failure) in task order. At most
{max_alive} sub-agents run at once across the whole run; extra tasks wait for a
free slot.

Agents available to a swarm:
{agents}

A member that dies early (turn or output budget) reports a partial handoff
marked SUB-AGENT TERMINATED EARLY in its task section -- verify its findings on
the forum and re-delegate what remains as a narrower task.

Use a swarm only when the tasks are independent: none needs another's answer,
and each targets a different function, address range, or behavior (e.g. \"what
does each of these 5 unexplored functions do\", or examining separate subsystems
at once). Give each task exactly one specific forum question so members do not
overlap or duplicate each other's work.

Do not use a swarm when a task depends on an earlier finding (e.g. recover a
key, then decrypt strings with it), when it is one deep investigation of a
single function or call chain, or when it follows up on a previous answer. Call
the agent's own tool for those, one task at a time."
    )
}

/// Build the argument schema for the swarm tool
///
/// Each task's `agent` is restricted to the swarmable tool names, and the task
/// list is capped at every member running at its own swarm size limit.
///
/// # Arguments
///
/// * `members` - The agents that can be included in a swarm
fn parameters(members: &[SwarmMember]) -> serde_json::Value {
    // restrict each task's agent to the swarmable tool names
    let agent_names: Vec<&str> = members.iter().map(|m| m.tool_name.as_str()).collect();
    // the largest swarm possible is every member at its own limit
    let max_tasks: usize = members.iter().map(|m| m.max_swarm_size).sum();
    // hand-write the argument schema (avoids pulling in schemars directly)
    json!({
        "type": "object",
        "properties": {
            "goal": {
                "type": "string",
                "description": "The overarching, top-level goal every task in this swarm serves."
            },
            "tasks": {
                "type": "array",
                "description": "The independent tasks to run concurrently, one sub-agent each.",
                "minItems": MIN_SWARM_TASKS,
                "maxItems": max_tasks,
                "items": {
                    "type": "object",
                    "properties": {
                        "agent": {
                            "type": "string",
                            "enum": agent_names,
                            "description": "The agent to run this task with."
                        },
                        "prompt": {
                            "type": "string",
                            "description": "The concrete task for this sub-agent to carry out."
                        }
                    },
                    "required": ["agent", "prompt"]
                }
            }
        },
        "required": ["goal", "tasks"]
    })
}

/// Wrap a set of sub-agents as a swarm tool for the main agent.
///
/// The returned `spawn_swarm` tool takes a `goal` and a list of `tasks`, runs
/// a sub-agent for every task concurrently, and returns one report with every
/// task's result in order. A member that fails is reported inline rather than
/// failing the whole swarm. Each member runs exactly like a single delegation:
/// it gets its own display name, span, hook, and usage-ledger entry, and it
/// counts against the run's live sub-agent cap.
///
/// # Arguments
///
/// * `members` - The agents that can be included in a swarm
/// * `usage` - The shared usage ledger each member's token usage folds into
/// * `coverage` - The shared coverage ledger each member's tool calls fold into
/// * `events` - An optional event sink to mirror the swarm's activity onto
/// * `store` - An optional session store each member's record is saved into
/// * `alive` - The semaphore capping how many sub-agents are alive at once
/// * `max_alive` - The number of permits `alive` was created with
/// * `post_mortem` - The shared salvage summarizer run over dead members, if any
///
/// # Examples
///
/// ```text
/// let tool = into_swarm_tool(
///     members,
///     usage.clone(),
///     coverage.clone(),
///     None,
///     None,
///     Arc::new(Semaphore::new(8)),
///     8,
///     None,
/// );
/// agent_builder.dynamic_tool(tool);
/// ```
#[allow(clippy::too_many_arguments)]
pub fn into_swarm_tool(
    members: Vec<SwarmMember>,
    usage: Arc<UsageLedger>,
    coverage: Arc<CoverageLedger>,
    events: Option<ThorPalEventSender>,
    store: Option<Arc<SessionStore>>,
    alive: Arc<Semaphore>,
    max_alive: usize,
    post_mortem: Option<Arc<Agent>>,
) -> DynamicTool {
    // describe the tool, its members, and when to use it
    let description = describe(&members, max_alive);
    // build the argument schema from the members and their limits
    let parameters = parameters(&members);
    // share the members across every invocation of this tool
    let members = Arc::new(members);

    DynamicTool::new(
        SWARM_TOOL_NAME,
        description,
        parameters,
        move |context, args| {
            // clone the shared state into this invocation's future
            let members = Arc::clone(&members);
            let usage = Arc::clone(&usage);
            let coverage = Arc::clone(&coverage);
            let events = events.clone();
            let store = store.clone();
            let alive = Arc::clone(&alive);
            // clone the shared salvage summarizer for each member's death path
            let post_mortem = post_mortem.clone();
            // propagate the caller's tool context so each member's own tools work
            let inherited_context = context.clone();
            Box::pin(async move {
                // parse the goal/tasks arguments, mirroring rig's error handling
                let args: SwarmArgs = serde_json::from_value(args).map_err(|error| {
                    ToolExecutionError::invalid_args(format!(
                        "failed to parse swarm tool arguments: {error}"
                    ))
                    .with_source(error)
                })?;
                // make sure every task targets a member and stays within its limit
                let resolved = validate_tasks(&members, &args.tasks)
                    .map_err(ToolExecutionError::invalid_args)?;
                // mint a unique display name for this swarm
                let name = crate::thorpal::agents::naming::next_display_name("Swarm");
                // prefix the swarm's name with the caller's side-conversation
                // scope, when one is set, so its spans and events attribute to
                // that scope (each member prefixes itself the same way)
                let name = match super::tool::ScopeMarker::from_context(&inherited_context) {
                    Some(scope) => format!("{}/{name}", scope.prefix),
                    None => name,
                };
                // build the span every member's own span nests under
                let span = tracing::info_span!("swarm", swarm = %name);
                // log the swarm's goal and size once as it starts
                tracing::info!(parent: &span, goal = %args.goal, tasks = args.tasks.len(), "swarm started");
                // mirror the swarm onto the event sink if a consumer is listening
                if let Some(tx) = &events {
                    let _ = tx.send(ThorPalEvent::SwarmStarted {
                        name: name.clone(),
                        goal: args.goal.clone(),
                        members: resolved.iter().map(|&i| members[i].role.clone()).collect(),
                        resumed: false,
                    });
                }
                // build a future for each task; the shared semaphore inside the
                // runner caps how many of them are actually alive at once
                let runs = resolved.iter().zip(&args.tasks).map(|(&index, task)| {
                    let member = &members[index];
                    run_sub_agent(
                        &member.agent,
                        Invocation::Fresh {
                            role: member.role.clone(),
                            goal: args.goal.clone(),
                            task: task.prompt.clone(),
                            swarm: Some(name.clone()),
                        },
                        inherited_context.clone(),
                        &member.enabled,
                        member.budget,
                        &usage,
                        &coverage,
                        &events,
                        &store,
                        &alive,
                        post_mortem.as_deref(),
                    )
                });
                // run every task concurrently under the swarm span, keeping task order
                let runs = futures::future::join_all(runs)
                    .instrument(span.clone())
                    .await;
                // a shutdown-time interruption freezes the swarm for resume:
                // the members persisted their own running-shaped records, so
                // skip the finished event and hand the marker up so the
                // parent's turn freezes too instead of reading teardown noise
                let interrupted = runs.iter().any(|run| {
                    run.output.as_ref().is_err_and(|error| {
                        error
                            .to_string()
                            .starts_with(crate::thorpal::retry::SHUTDOWN_INTERRUPT_MARKER)
                    })
                });
                if interrupted {
                    tracing::info!(parent: &span, "swarm interrupted by shutdown; freezing for resume");
                    return Err(ToolExecutionError::other(format!(
                        "{} swarm interrupted by shutdown",
                        crate::thorpal::retry::SHUTDOWN_INTERRUPT_MARKER
                    )));
                }
                // tally the outcomes for the log line and event
                let failed = runs.iter().filter(|run| run.output.is_err()).count();
                let succeeded = runs.len() - failed;
                tracing::info!(parent: &span, succeeded, failed, "swarm finished");
                // mirror the swarm's completion onto the event sink if a consumer is listening
                if let Some(tx) = &events {
                    let _ = tx.send(ThorPalEvent::SwarmFinished {
                        name,
                        succeeded,
                        failed,
                    });
                }
                // hand every member's result back to the calling agent in one report
                Ok(ToolOutput::text(render_results(&args.tasks, &runs)))
            })
        },
    )
}

/// Re-applies the live swarm validation against the retained delegation specs
///
/// Mirrors [`validate_tasks`] for a resumed call: the same task-count floor,
/// unknown/non-swarmable agent rejection, and per-member swarm-size limits,
/// phrased identically so the parent reads the refusal the live run would
/// have produced.
///
/// # Arguments
///
/// * `registry` - The retained delegation specs the limits come from
/// * `tasks` - The dead call's tasks, reparsed
fn validate_resumed_tasks(
    registry: &DelegationRegistry,
    tasks: &[SwarmTask],
) -> Result<(), String> {
    // a single task gains nothing from a swarm
    if tasks.len() < MIN_SWARM_TASKS {
        return Err(format!(
            "a swarm needs at least {MIN_SWARM_TASKS} tasks; call the agent's own tool for a single task"
        ));
    }
    // track how many tasks each member has been given
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for task in tasks {
        // the task must target a swarmable member of this run's workflow
        let Some(spec) = registry
            .specs
            .get(&task.agent)
            .filter(|spec| spec.swarmable)
        else {
            let valid: Vec<&str> = registry
                .specs
                .iter()
                .filter(|(_, spec)| spec.swarmable)
                .map(|(name, _)| name.as_str())
                .collect();
            return Err(format!(
                "unknown swarm agent '{}'; valid agents are: {}",
                task.agent,
                valid.join(", ")
            ));
        };
        // count this task against its member's swarm size limit
        let count = counts.entry(task.agent.as_str()).or_default();
        *count += 1;
        if *count > spec.max_swarm_size {
            return Err(format!(
                "a swarm may include at most {} '{}' agents",
                spec.max_swarm_size, task.agent
            ));
        }
    }
    Ok(())
}

/// Resume an interrupted `spawn_swarm` call across a process death
///
/// Reconstructs the dead run's swarm from the dangling call's arguments and
/// the saved agent records: a finished member's answer is reused verbatim
/// (never re-run), a running-shaped member resumes from its persisted partial
/// transcript under its own name, and a task with no record (it was still
/// queued behind the live cap when the process died) starts fresh under the
/// recorded swarm's name. The result is the same in-task-order report a live
/// swarm returns, so the parent conversation cannot tell the difference.
///
/// # Arguments
///
/// * `registry` - The retained delegation specs, live cap, and post-mortem
/// * `args` - The dangling `spawn_swarm` call's arguments, verbatim
/// * `parent_call` - The dangling call's `ToolCallId` string
/// * `records` - The unmatched saved records, consumed as members match
/// * `context` - The run's shared tool context for the members' own tools
/// * `usage` - The shared usage ledger member usage folds into
/// * `coverage` - The shared coverage ledger member tool calls fold into
/// * `events` - An optional event sink to mirror the swarm's activity onto
/// * `store` - An optional session store member records save into
#[allow(clippy::too_many_arguments)]
pub(crate) async fn resume_swarm(
    registry: &DelegationRegistry,
    args: &serde_json::Value,
    parent_call: &str,
    records: &mut Vec<AgentRecord>,
    context: rig::tool::ToolContext,
    usage: &Arc<UsageLedger>,
    coverage: &Arc<CoverageLedger>,
    events: &Option<ThorPalEventSender>,
    store: &Option<Arc<SessionStore>>,
) -> String {
    // reparse the arguments the model issued the dead call with; a parse
    // failure means the stored history is damaged
    let args: SwarmArgs = match serde_json::from_value(args.clone()) {
        Ok(args) => args,
        Err(error) => {
            tracing::warn!(?error, "failed to reparse an interrupted swarm's arguments");
            return format!(
                "[interrupted] this swarm never completed and its arguments could not \
                 be reconstructed ({error}); re-issue the spawn_swarm call if the \
                 results are still needed."
            );
        }
    };
    // re-apply the live tool's validation: a call the live run would have
    // refused (too few tasks, an unknown or non-swarmable agent, a member
    // over its swarm-size limit) must read as the SAME refusal on resume --
    // the dead process may have been killed before the refusal persisted
    if let Err(error) = validate_resumed_tasks(registry, &args.tasks) {
        tracing::warn!(%error, "an interrupted swarm fails validation; answering with the refusal");
        return error;
    }
    // match each task to its saved record (consumed so duplicate tasks pair
    // one-to-one), splitting what each member needs into resume/reuse/fresh
    let matched: Vec<(/* task index */ usize, Option<AgentRecord>)> = args
        .tasks
        .iter()
        .enumerate()
        .map(|(index, task)| {
            // an unknown agent (a workflow change between runs) matches nothing
            let role = registry
                .specs
                .get(&task.agent)
                .map(|spec| spec.role.as_str())
                .unwrap_or(task.agent.as_str());
            let record = take_matching_record(records, parent_call, role, &args.goal, &task.prompt);
            (index, record)
        })
        .collect();
    // reuse the recorded swarm's name so member identities stay grouped under
    // it; only a swarm with no surviving member record mints a fresh name
    let name = matched
        .iter()
        .find_map(|(_, record)| record.as_ref().and_then(|record| record.swarm.clone()))
        .unwrap_or_else(|| crate::thorpal::agents::naming::next_display_name("Swarm"));
    // build the span every member's own span nests under
    let span = tracing::info_span!("swarm", swarm = %name);
    tracing::info!(parent: &span, goal = %args.goal, tasks = args.tasks.len(), "swarm resumed");
    // mirror the resumed swarm onto the event sink if a consumer is
    // listening, naming members by ROLE in task order exactly like a live one
    if let Some(tx) = events {
        let _ = tx.send(ThorPalEvent::SwarmStarted {
            name: name.clone(),
            goal: args.goal.clone(),
            members: args
                .tasks
                .iter()
                .map(|task| {
                    registry
                        .specs
                        .get(&task.agent)
                        .map(|spec| spec.role.clone())
                        .unwrap_or_else(|| task.agent.clone())
                })
                .collect(),
            resumed: true,
        });
    }
    // run every member concurrently under the swarm span, keeping task order;
    // reused finished records resolve immediately while resumed and fresh
    // members draw on the shared live cap exactly like a live swarm
    let runs = matched.into_iter().map(|(index, record)| {
        let task = &args.tasks[index];
        let goal = &args.goal;
        let name = &name;
        let context = context.clone();
        async move {
            // a finished record's outcome is reused verbatim, never re-run
            if let Some(record) = &record
                && record.finished_at.is_some()
            {
                return rerun_from_record(record.clone());
            }
            // an unknown agent cannot be re-run; report it inline like a death
            let Some(spec) = registry.specs.get(&task.agent) else {
                return SubAgentRun {
                    name: task.agent.clone(),
                    output: Err(rig::tool::ToolExecutionError::other(format!(
                        "the '{}' agent is not part of this session's workflow",
                        task.agent
                    ))),
                };
            };
            // a running-shaped record resumes; a task with no record starts fresh
            let invocation = match record {
                Some(record) => Invocation::Resumed { record },
                None => Invocation::Fresh {
                    role: spec.role.clone(),
                    goal: goal.clone(),
                    task: task.prompt.clone(),
                    swarm: Some(name.clone()),
                },
            };
            run_sub_agent(
                &spec.agent,
                invocation,
                context,
                &spec.enabled,
                spec.budget,
                usage,
                coverage,
                events,
                store,
                &registry.alive,
                registry.post_mortem.as_deref(),
            )
            .await
        }
    });
    let runs = futures::future::join_all(runs)
        .instrument(span.clone())
        .await;
    // a shutdown mid-resume freezes the swarm again, exactly like a live one:
    // the members persisted running-shaped records, so skip the finished
    // event and hand the marker up (the caller's own turn then dies on the
    // latched gate before this text could persist anywhere)
    let interrupted = runs.iter().any(|run| {
        run.output.as_ref().is_err_and(|error| {
            error
                .to_string()
                .starts_with(crate::thorpal::retry::SHUTDOWN_INTERRUPT_MARKER)
        })
    });
    if interrupted {
        tracing::info!(parent: &span, "resumed swarm interrupted by shutdown; freezing again");
        return format!(
            "{} swarm interrupted by shutdown",
            crate::thorpal::retry::SHUTDOWN_INTERRUPT_MARKER
        );
    }
    // tally the outcomes for the log line and event
    let failed = runs.iter().filter(|run| run.output.is_err()).count();
    let succeeded = runs.len() - failed;
    tracing::info!(parent: &span, succeeded, failed, "swarm finished");
    // mirror the swarm's completion onto the event sink if a consumer is listening
    if let Some(tx) = events {
        let _ = tx.send(ThorPalEvent::SwarmFinished {
            name,
            succeeded,
            failed,
        });
    }
    // hand every member's result back in the same report shape a live swarm uses
    render_results(&args.tasks, &runs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig::client::AgentClientExt;
    use rig::providers::openai;

    /// Build a swarm member backed by an offline agent
    ///
    /// The agent is never prompted, so the client only needs to be buildable.
    ///
    /// # Arguments
    ///
    /// * `tool_name` - The tool name to give this member
    /// * `max_swarm_size` - The max copies of this member a swarm may include
    fn member(tool_name: &str, max_swarm_size: usize) -> SwarmMember {
        // build a client that is never actually used to talk to a model
        let client = openai::Client::builder()
            .base_url("http://localhost:1")
            .api_key("test")
            .build()
            .expect("offline client should build");
        SwarmMember {
            tool_name: tool_name.to_owned(),
            role: tool_name.to_owned(),
            description: "desc".to_owned(),
            max_swarm_size,
            agent: Arc::new(client.agent("test-model").build()),
            enabled: EnabledTools::default(),
            budget: TurnBudget {
                max_turns: 10,
                max_extension_turns: 2,
                warn_turns: 2,
                context_window: 0,
            },
        }
    }

    /// Build a swarm task for the given agent
    ///
    /// # Arguments
    ///
    /// * `agent` - The tool name of the agent to run this task with
    /// * `prompt` - The task for the agent to carry out
    fn task(agent: &str, prompt: &str) -> SwarmTask {
        SwarmTask {
            agent: agent.to_owned(),
            prompt: prompt.to_owned(),
        }
    }

    /// Valid tasks resolve to their members in task order.
    #[test]
    fn valid_tasks_resolve_in_order() {
        let members = vec![member("researcher", 2), member("reverse_engineer", 3)];
        let tasks = vec![
            task("reverse_engineer", "a"),
            task("researcher", "b"),
            task("reverse_engineer", "c"),
        ];
        assert_eq!(validate_tasks(&members, &tasks), Ok(vec![1, 0, 1]));
    }

    /// Invalid swarms are rejected with an explanation.
    #[test]
    fn invalid_tasks_are_rejected() {
        let members = vec![member("researcher", 1), member("reverse_engineer", 2)];
        // a single task should use the agent's own tool
        assert!(validate_tasks(&members, &[task("researcher", "a")]).is_err());
        // unknown agents list the valid ones
        let err = validate_tasks(&members, &[task("nope", "a"), task("researcher", "b")])
            .expect_err("unknown agent should be rejected");
        assert!(err.contains("reverse_engineer"));
        // each agent is held to its own swarm size limit
        let err = validate_tasks(
            &members,
            &[task("researcher", "a"), task("researcher", "b")],
        )
        .expect_err("over the limit should be rejected");
        assert!(err.contains("at most 1 'researcher'"));
    }

    /// The schema restricts agents to members and caps the task count.
    #[test]
    fn schema_reflects_members() {
        let members = vec![member("researcher", 2), member("reverse_engineer", 6)];
        let schema = parameters(&members);
        let tasks = &schema["properties"]["tasks"];
        assert_eq!(tasks["minItems"], 2);
        assert_eq!(tasks["maxItems"], 8);
        assert_eq!(
            tasks["items"]["properties"]["agent"]["enum"],
            json!(["researcher", "reverse_engineer"])
        );
    }

    /// The report lists every task in order, including failures.
    #[test]
    fn results_render_successes_and_failures() {
        let tasks = vec![
            task("reverse_engineer", "first"),
            task("reverse_engineer", "second"),
        ];
        let runs = vec![
            SubAgentRun {
                name: "ReverseEngineer(Io)".to_owned(),
                output: Ok("found it".to_owned()),
            },
            SubAgentRun {
                name: "ReverseEngineer(Europa)".to_owned(),
                output: Err(ToolExecutionError::other("sub-agent terminated early")
                    .with_model_feedback("SUB-AGENT TERMINATED EARLY -- partial handoff")),
            },
        ];
        let report = render_results(&tasks, &runs);
        assert!(report.starts_with("Swarm finished: 1 succeeded, 1 failed."));
        // the first task's answer comes before the second task's failure
        let first = report
            .find("### Task 1: ReverseEngineer(Io)")
            .expect("task 1 section");
        let second = report
            .find("### Task 2: ReverseEngineer(Europa)")
            .expect("task 2 section");
        assert!(first < second);
        assert!(report.contains("found it"));
        // a death with a salvaged handoff renders the handoff, not the error
        assert!(report.contains("FAILED: SUB-AGENT TERMINATED EARLY -- partial handoff"));
        // a failure without model feedback still renders its operator message
        let bare = vec![
            SubAgentRun {
                name: "ReverseEngineer(Io)".to_owned(),
                output: Ok("found it".to_owned()),
            },
            SubAgentRun {
                name: "ReverseEngineer(Europa)".to_owned(),
                output: Err(ToolExecutionError::invalid_args("boom")),
            },
        ];
        let report = render_results(&tasks, &bare);
        assert!(report.contains("FAILED: "));
        assert!(report.contains("boom"));
    }

    /// The live sub-agent cap is shared by every member of a swarm.
    #[tokio::test]
    async fn alive_cap_limits_concurrency() {
        // a single slot shared by three concurrent waiters
        let alive = Semaphore::new(1);
        let peak = std::sync::atomic::AtomicUsize::new(0);
        let current = std::sync::atomic::AtomicUsize::new(0);
        // mirror how run_sub_agent holds a permit for its whole run
        let work = (0..3).map(|_| async {
            let _permit = alive.acquire().await.expect("semaphore open");
            let now = current.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            peak.fetch_max(now, std::sync::atomic::Ordering::SeqCst);
            tokio::task::yield_now().await;
            current.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        });
        futures::future::join_all(work).await;
        assert_eq!(peak.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
