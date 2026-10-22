//! The tool run tools for the Thorium MCP server
//!
//! These tools never hold a request open waiting for a job: `run_tool` returns as soon as the
//! run's reaction exists, `get_tool_run` reads its status with a hint of when to poll again,
//! and `get_tool_run_results` refuses runs that haven't finished.

use chrono::Utc;
use http::StatusCode;
use rmcp::ErrorData;
use rmcp::handler::server::tool::Extension as RmcpExtension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use tracing::instrument;
use uuid::Uuid;

use crate::Error;
use crate::client::{ResultsClient, Thorium};
use crate::models::{
    AgenticTool, AgenticToolBacking, EntityMetadata, MappedToolInput, Output, Reaction,
    ReactionStatus, ResultGetParams, RunInputs, ToolRunRequest, ToolSpec, tool_run_retry_after,
};

use super::{ThoriumMCP, json_result};

/// The params needed to start a tool run
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct RunTool {
    /// The id of the tool instance to run, as named in the tool's description
    pub tool_instance: Uuid,
    /// The tool call's input, matching the tool's input schema
    #[serde(default)]
    pub input: serde_json::Map<String, Value>,
    /// The group to run in (only needed when the tool instance is in several groups the caller
    /// can run in)
    #[serde(default)]
    pub group: Option<String>,
}

/// The params needed to read a tool run
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ToolRun {
    /// The id of the tool run's reaction, as `run_tool` returned it
    pub reaction: Uuid,
    /// The group the tool run is in, as `run_tool` returned it (searched for when unset)
    #[serde(default)]
    pub group: Option<String>,
}

/// Build the error for a tool run that can't be found
///
/// # Arguments
///
/// * `id` - The id of the tool run's reaction
fn run_not_found(id: Uuid) -> ErrorData {
    // say the run doesn't exist or isn't visible
    ErrorData::resource_not_found(
        format!("tool run {id} doesn't exist or isn't in any of your groups"),
        None,
    )
}

/// Whether an error means a group doesn't hold a reaction the caller can read
///
/// # Arguments
///
/// * `error` - The error from reading a reaction
fn is_missing(error: &Error) -> bool {
    // a missing reaction and a group the caller can't read both mean "not here"
    matches!(
        error.status(),
        Some(StatusCode::NOT_FOUND | StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
    )
}

/// Find a tool run's reaction in the first of some groups that holds it
///
/// # Arguments
///
/// * `thorium` - The client of the caller
/// * `id` - The id of the tool run's reaction
/// * `groups` - The groups to look in
async fn find_run(thorium: &Thorium, id: Uuid, groups: &[String]) -> Result<Reaction, ErrorData> {
    // read the reaction from every group at once
    let reads =
        futures::future::join_all(groups.iter().map(|group| thorium.reactions.get(group, id)))
            .await;
    // keep the reaction a group holds, passing on any error that isn't a miss
    let mut failure = None;
    for read in reads {
        match read {
            Ok(reaction) => return Ok(reaction),
            Err(error) if is_missing(&error) => (),
            Err(error) => failure = failure.or(Some(error)),
        }
    }
    // no group holds it, so report the first real failure or that it wasn't found
    match failure {
        Some(error) => Err(ErrorData::from(error)),
        None => Err(run_not_found(id)),
    }
}

/// Read a tool run's reaction from its group, or from every group the caller is in
///
/// # Arguments
///
/// * `thorium` - The client of the caller
/// * `params` - The tool run to read
async fn read_run(thorium: &Thorium, params: &ToolRun) -> Result<Reaction, ErrorData> {
    // look in the named group, or in every group the caller is in
    let groups = match &params.group {
        Some(group) => vec![group.clone()],
        None => thorium.users.info().await?.groups,
    };
    find_run(thorium, params.reaction, &groups).await
}

/// Map a tool call input with the input mapping of the tool instance it runs
///
/// Only container tools have an input mapping; any other tool instance takes no input, and the
/// tool run route decides whether it can run.
///
/// # Arguments
///
/// * `metadata` - The metadata of the entity to run
/// * `id` - The id of the entity to run
/// * `input` - The tool call's input
fn map_input(
    metadata: &EntityMetadata,
    id: Uuid,
    input: &serde_json::Map<String, Value>,
) -> Result<MappedToolInput, ErrorData> {
    // only tool instances can be run
    let EntityMetadata::ToolInstance(instance) = metadata else {
        return Err(ErrorData::resource_not_found(
            format!("tool instance {id} not found"),
            None,
        ));
    };
    match &instance.spec {
        // a container tool maps its input onto its job
        ToolSpec::Agentic(AgenticTool {
            backing: AgenticToolBacking::Container(container),
            ..
        }) => container
            .inputs
            .map_input(input)
            .map_err(|error| ErrorData::invalid_params(error, None)),
        // any other tool instance has nowhere to put an input
        _ if input.is_empty() => Ok(MappedToolInput::default()),
        _ => Err(ErrorData::invalid_params(
            format!("tool instance {id} isn't a container tool, so it takes no input"),
            None,
        )),
    }
}

/// Describe one result a tool run stored on an input
///
/// # Arguments
///
/// * `outputs` - The results stored on the input under the run's tool, newest first
fn result_entry(outputs: Option<&Vec<Output>>) -> Value {
    // show the newest result and its files, or nothing if there isn't one
    match outputs.and_then(|outputs| outputs.first()) {
        Some(output) => json!({"result": output.result, "files": output.files}),
        None => Value::Null,
    }
}

#[tool_router(router = tool_runs_router, vis = "pub")]
impl ThoriumMCP {
    /// Start a tool run without waiting for it
    ///
    /// # Arguments
    ///
    /// * `parameters` - The parameters required for this tool
    /// * `parts` - The request parts required to get a token for this tool
    #[tool(
        name = "run_tool",
        description = "Start a run of a tool behind Thorium without waiting for it. Pass the tool \
        instance id named in the tool's description and the tool's input. Returns the run's \
        reaction id and group; read the run with get_tool_run and its results with \
        get_tool_run_results."
    )]
    #[instrument(name = "ThoriumMCP::run_tool", skip(self, parts), err(Debug))]
    pub async fn run_tool(
        &self,
        Parameters(params): Parameters<RunTool>,
        RmcpExtension(parts): RmcpExtension<axum::http::request::Parts>,
    ) -> Result<CallToolResult, ErrorData> {
        // get a thorium client
        let thorium = self.conf.client(&parts).await?;
        // read the tool instance to run
        let entity = thorium.entities.get(params.tool_instance).await?;
        // map the input onto the tool's job args and files
        let mapped = map_input(&entity.metadata, params.tool_instance, &params.input)?;
        // run on the files the input names; a run token's tool call naming none runs on its run's
        // inputs
        let inputs = mapped
            .samples
            .unwrap_or_default()
            .into_iter()
            .fold(RunInputs::default(), RunInputs::sample);
        let mut req = ToolRunRequest::new(inputs).args(mapped.args);
        // run in the requested group, if one was given
        if let Some(group) = &params.group {
            req = req.group(group);
        }
        // start the run without waiting for its job
        let created = thorium
            .reactions
            .run_tool(params.tool_instance, &req)
            .await?;
        // find the group the run was placed in so it can be read back with one request
        let groups = match &params.group {
            Some(group) => vec![group.clone()],
            None => entity.groups.clone(),
        };
        let reaction = match find_run(&thorium, created.id, &groups).await {
            Ok(reaction) => reaction,
            // a run token's tool call runs in its run's group, which may be none of these
            Err(_) => find_run(&thorium, created.id, &thorium.users.info().await?.groups).await?,
        };
        // return the run's reaction id and group
        json_result(&json!({"reaction": reaction.id, "group": reaction.group}))
    }

    /// Get a tool run's status without waiting
    ///
    /// # Arguments
    ///
    /// * `parameters` - The parameters required for this tool
    /// * `parts` - The request parts required to get a token for this tool
    #[tool(
        name = "get_tool_run",
        description = "Get the status (Created, Started, Completed, or Failed) of a tool run \
        started with run_tool, without waiting. While it runs, retry_after_secs says how many \
        seconds to wait before checking it again."
    )]
    #[instrument(name = "ThoriumMCP::get_tool_run", skip(self, parts), err(Debug))]
    pub async fn get_tool_run(
        &self,
        Parameters(params): Parameters<ToolRun>,
        RmcpExtension(parts): RmcpExtension<axum::http::request::Parts>,
    ) -> Result<CallToolResult, ErrorData> {
        // get a thorium client
        let thorium = self.conf.client(&parts).await?;
        // read the run's reaction
        let reaction = read_run(&thorium, &params).await?;
        // hint when to poll again from the run's start while it hasn't finished
        let retry_after = match reaction.status {
            ReactionStatus::Completed | ReactionStatus::Failed => None,
            ReactionStatus::Created | ReactionStatus::Started => {
                // the run started when its reaction was created, its first status update
                let logs = thorium
                    .reactions
                    .status_logs(&reaction.group, &reaction.id)
                    .await?;
                // the polling hint counts from the earliest update until now
                let now = Utc::now();
                let start = logs.iter().map(|log| log.timestamp).min().unwrap_or(now);
                let elapsed = u64::try_from((now - start).num_seconds()).unwrap_or(0);
                tool_run_retry_after(&reaction.status, elapsed)
            }
        };
        // return the run's status and polling hint
        json_result(&json!({
            "reaction": reaction.id,
            "group": reaction.group,
            "status": reaction.status,
            "retry_after_secs": retry_after,
        }))
    }

    /// Get a finished tool run's results
    ///
    /// # Arguments
    ///
    /// * `parameters` - The parameters required for this tool
    /// * `parts` - The request parts required to get a token for this tool
    #[tool(
        name = "get_tool_run_results",
        description = "Get the results of a finished tool run started with run_tool, for each \
        file or repo it ran on. A run that hasn't finished is refused right away; check it with \
        get_tool_run first."
    )]
    #[instrument(
        name = "ThoriumMCP::get_tool_run_results",
        skip(self, parts),
        err(Debug)
    )]
    pub async fn get_tool_run_results(
        &self,
        Parameters(params): Parameters<ToolRun>,
        RmcpExtension(parts): RmcpExtension<axum::http::request::Parts>,
    ) -> Result<CallToolResult, ErrorData> {
        // get a thorium client
        let thorium = self.conf.client(&parts).await?;
        // read the run's reaction
        let reaction = read_run(&thorium, &params).await?;
        // only a completed run has results
        match reaction.status {
            ReactionStatus::Completed => (),
            ReactionStatus::Failed => {
                return Err(ErrorData::invalid_request(
                    format!("tool run {} failed, so it has no results", reaction.id),
                    None,
                ));
            }
            ReactionStatus::Created | ReactionStatus::Started => {
                return Err(ErrorData::invalid_request(
                    format!(
                        "tool run {} hasn't finished yet ({}); check it with get_tool_run",
                        reaction.id, reaction.status
                    ),
                    None,
                ));
            }
        }
        // results are stored on the run's inputs under its tool's name
        let Some(tool) = reaction.result_key() else {
            return Err(ErrorData::invalid_params(
                format!(
                    "reaction {} is a pipeline reaction, not a tool run",
                    reaction.id
                ),
                None,
            ));
        };
        let query = ResultGetParams::default().tool(tool);
        // read the results on each input file and repo
        let mut results = BTreeMap::new();
        for sha256 in &reaction.samples {
            let found = thorium.files.get_results(sha256, &query).await?;
            results.insert(sha256.clone(), result_entry(found.results.get(tool)));
        }
        for repo in &reaction.repos {
            let found = thorium.repos.get_results(&repo.url, &query).await?;
            results.insert(repo.url.clone(), result_entry(found.results.get(tool)));
        }
        // return each input's result under the run's tool name
        json_result(&json!({"reaction": reaction.id, "tool": tool, "results": results}))
    }
}
