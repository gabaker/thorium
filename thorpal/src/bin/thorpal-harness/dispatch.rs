//! The run's tools as `ThorPal` tools: rendered for the model with Thorium's renderers and
//! carried out through Thorium's API

use rig::tool::{DynamicTool, ToolErrorKind, ToolExecutionError, ToolOutput};
use serde_json::Value;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;
use thorium::Thorium;
use thorium::client::Keys;
use thorium::client::tool_runs::{self, NoHooks, ToolRunExit, ToolRunOptions, ToolRunReport};
use thorium::models::{
    AgenticToolBacking, HarnessModelEntry, HarnessToolEntry, ModelApi, RunInputs, ToolChoice,
    ToolInvocation, ToolResultFormat, ToolRunRequest,
};
use thorium::tools::render::{
    RenderError, RenderOptions, ToolCallOutput, ToolCallResult, decode_arguments, encode_result,
    render_tool_choice, render_tools,
};
use tokio::sync::OnceCell;
use uuid::Uuid;

/// The definition fields rig sends for every tool; the renderers' other fields are dropped
const SENT_FIELDS: [&str; 3] = ["name", "description", "input_schema"];

/// Carries out the run's tool calls through Thorium
pub struct Dispatcher {
    /// The Thorium API url and the run's token
    keys: Keys,
    /// The run's reaction, the parent of every tool run it starts
    run: Uuid,
    /// The client for tool runs, built on the first one
    client: OnceCell<Thorium>,
}

impl Dispatcher {
    /// Build a dispatcher for a run
    ///
    /// # Arguments
    ///
    /// * `keys` - The Thorium API url and the run's token
    /// * `run` - The run's reaction id
    #[must_use]
    pub fn new(keys: Keys, run: Uuid) -> Self {
        Dispatcher {
            keys,
            run,
            client: OnceCell::new(),
        }
    }

    /// Get the Thorium client, building it on first use
    async fn client(&self) -> Result<&Thorium, String> {
        // build the client from the run's keys once
        self.client
            .get_or_try_init(|| Thorium::from_keys(self.keys.clone()))
            .await
            .map_err(|error| format!("unable to connect to Thorium: {error}"))
    }

    /// Carry out one tool call
    ///
    /// # Arguments
    ///
    /// * `tool_instance` - The tool instance being called
    /// * `entry` - The tool's `tools.json` entry
    /// * `input` - The decoded tool call input
    pub async fn call(
        &self,
        tool_instance: Uuid,
        entry: &HarnessToolEntry,
        input: &serde_json::Map<String, Value>,
    ) -> ToolRunReport {
        // only tools reached through Thorium's API get here
        let ToolInvocation::ThoriumApi { backing, .. } = &entry.invocation else {
            return ToolRunReport::failed(format!(
                "{} isn't reached through Thorium's API",
                entry.function_name
            ));
        };
        // wait at most the tool's timeout and return at most its result size
        let mut opts = ToolRunOptions::new(thorium::models::DEFAULT_TOOL_RESULTS_DIR);
        opts.timeout = entry.timeout.map(Duration::from_secs);
        opts.max_result_bytes = entry.max_result_bytes;
        match backing {
            // a Thorium MCP tool is called through Thorium's MCP server with the run's token
            AgenticToolBacking::ThoriumMcp { tool } => {
                // the MCP server authenticates the run's token
                let Some(token) = &self.keys.token else {
                    return ToolRunReport::failed("calling a Thorium MCP tool needs a token");
                };
                // Thorium serves its MCP server under the API
                let url = format!("{}/api/mcp", self.keys.api.trim_end_matches('/'));
                tool_runs::call_mcp(&url, token, tool, input, &opts).await
            }
            // a container tool runs as a sub run of this run
            AgenticToolBacking::Container(container) => {
                // map the call's input onto the tool's job
                let mapped = match container.inputs.map_input(input) {
                    Ok(mapped) => mapped,
                    Err(error) => return ToolRunReport::usage(error),
                };
                // run the job on the files the input names, under this run
                let inputs = mapped
                    .samples
                    .unwrap_or_default()
                    .into_iter()
                    .fold(RunInputs::default(), RunInputs::sample);
                let req = ToolRunRequest::new(inputs)
                    .args(mapped.args)
                    .parent(self.run);
                opts.output = container.outputs.resolved_path(&entry.function_name).into();
                // the run's reaction is found in the tool instance's groups
                let thorium = match self.client().await {
                    Ok(thorium) => thorium,
                    Err(error) => return ToolRunReport::failed(error),
                };
                let groups = match thorium.entities.get(tool_instance).await {
                    Ok(entity) => entity.groups,
                    Err(error) => return ToolRunReport::failed(error.to_string()),
                };
                // start the run, wait for it, and download its results
                tool_runs::run(thorium, tool_instance, &groups, &req, &opts, &mut NoHooks).await
            }
            // a native tool only runs inside the harness that provides it
            AgenticToolBacking::HarnessNative { .. } => ToolRunReport::failed(format!(
                "{} is built into another harness",
                entry.function_name
            )),
        }
    }
}

/// Turn a tool call's report into the result encoded for the model
///
/// Returns the text the model gets and whether the call failed.
///
/// # Arguments
///
/// * `report` - The tool call's report
/// * `entry` - The tool's `tools.json` entry
/// * `api` - The protocol the model is called with
#[must_use]
pub fn encode_report(
    report: &ToolRunReport,
    entry: &HarnessToolEntry,
    api: ModelApi,
) -> (String, bool) {
    // a call that didn't complete returns its error
    let failed = report.exit != ToolRunExit::Completed;
    let output = if failed {
        ToolCallOutput::Text(
            report
                .output
                .error
                .clone()
                .unwrap_or_else(|| format!("{} failed", entry.function_name)),
        )
    } else {
        // a completed call returns its result in the tool's format
        let result = report.output.result.clone().unwrap_or(Value::Null);
        match entry.result_format {
            ToolResultFormat::Json => ToolCallOutput::Json(result),
            ToolResultFormat::Text => ToolCallOutput::Text(match result {
                Value::String(text) => text,
                other => other.to_string(),
            }),
            ToolResultFormat::Files => ToolCallOutput::Files(report.output.files.clone()),
        }
    };
    // encode it for the model's API
    let result = ToolCallResult {
        call_id: String::new(),
        function_name: entry.function_name.clone(),
        output,
        is_error: failed,
    };
    let encoded = encode_result(&result, entry, api);
    // rig builds the tool result envelope, so only its content and error flag are kept
    let is_error = encoded.get("is_error").and_then(Value::as_bool) == Some(true);
    let text = match encoded.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
        None => encoded.to_string(),
    };
    (text, is_error)
}

/// The run's tools rendered as `ThorPal` tools
pub struct RenderedTools {
    /// The tools to give the entry agent
    pub tools: Vec<DynamicTool>,
    /// Messages to log once for the run: renderer warnings and the fields rig doesn't send
    pub warnings: Vec<String>,
}

/// Render the entry agent's tools for the run's model and bind each to the dispatcher
///
/// # Arguments
///
/// * `entries` - The entry agent's tools, by tool instance id
/// * `model` - The entry agent's model
/// * `choice` - The entry agent's tool choice
/// * `parallel` - The entry agent's parallel tool call setting
/// * `dispatcher` - Carries out the tool calls
pub fn render(
    entries: &[(Uuid, HarnessToolEntry)],
    model: &HarnessModelEntry,
    choice: &ToolChoice,
    parallel: Option<bool>,
    dispatcher: &Arc<Dispatcher>,
) -> Result<RenderedTools, RenderError> {
    // rig never sends a tool's strict flag, and a call's arguments are checked against the tool's
    // own schema, so render no strict variant: one would drop bounds the checks still apply
    let mut capabilities = model.capabilities.clone();
    capabilities.strict_tools = false;
    // render every tool and the tool choice for the model's API
    let opts = RenderOptions {
        capabilities,
        model_id: model.model_id.clone(),
        ..RenderOptions::default()
    };
    let refs: Vec<&HarnessToolEntry> = entries.iter().map(|(_, entry)| entry).collect();
    let rendered = render_tools(&refs, model.api, &opts)?;
    let tool_choice = render_tool_choice(choice, parallel, model.api, &opts);
    // gather what to log once for the run
    let mut warnings: Vec<String> = rendered
        .iter()
        .flat_map(|tool| tool.warnings.iter().cloned())
        .chain(tool_choice.warnings.iter().cloned())
        .collect();
    // rig sends only a tool's name, description, and input schema
    let dropped: BTreeSet<String> = rendered
        .iter()
        .filter_map(|tool| tool.definition.as_object())
        .flat_map(|definition| definition.keys())
        .filter(|key| !SENT_FIELDS.contains(&key.as_str()))
        .cloned()
        .collect();
    if !dropped.is_empty() {
        warnings.push(format!(
            "ThorPal sends only the name, description, and input schema of each tool, so these rendered fields are dropped: {}",
            dropped.into_iter().collect::<Vec<String>>().join(", ")
        ));
    }
    if *choice != ToolChoice::Auto || parallel.is_some() {
        warnings.push(format!(
            "ThorPal sets no tool choice or parallel call setting, so the agent's ({choice:?}, parallel {parallel:?}) are dropped"
        ));
    }
    // bind each rendered tool to the dispatcher
    let api = model.api;
    let tools = entries
        .iter()
        .zip(rendered)
        .map(|((tool_instance, entry), rendered)| {
            // the model sees the rendered schema and description
            let schema = rendered
                .definition
                .get("input_schema")
                .cloned()
                .unwrap_or_else(|| Value::Object(entry.input_schema.clone()));
            let description = rendered
                .definition
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or(&entry.model_description)
                .to_owned();
            // each call is carried out through the dispatcher
            let tool_instance = *tool_instance;
            let entry = Arc::new(entry.clone());
            let dispatcher = Arc::clone(dispatcher);
            DynamicTool::new(
                entry.function_name.clone(),
                description,
                schema,
                move |_context, raw| {
                    // each call gets its own handles to the entry and dispatcher
                    let entry = Arc::clone(&entry);
                    let dispatcher = Arc::clone(&dispatcher);
                    Box::pin(async move {
                        call_tool(&dispatcher, tool_instance, &entry, api, &raw).await
                    })
                },
            )
        })
        .collect();
    Ok(RenderedTools { tools, warnings })
}

/// Decode, carry out, and encode one tool call
///
/// # Arguments
///
/// * `dispatcher` - Carries out the call
/// * `tool_instance` - The tool instance being called
/// * `entry` - The tool's `tools.json` entry
/// * `api` - The protocol the model is called with
/// * `raw` - The arguments the model sent
async fn call_tool(
    dispatcher: &Dispatcher,
    tool_instance: Uuid,
    entry: &HarnessToolEntry,
    api: ModelApi,
    raw: &Value,
) -> Result<ToolOutput, ToolExecutionError> {
    // rig doesn't send strict schemas, so every call's arguments are checked
    let input = match decode_arguments(raw, entry, false) {
        Ok(Value::Object(input)) => input,
        Ok(_) => serde_json::Map::new(),
        Err(error) => {
            return Err(ToolExecutionError::invalid_args(error.message.clone())
                .with_model_feedback(error.message));
        }
    };
    // carry out the call and encode its result
    let report = dispatcher.call(tool_instance, entry, &input).await;
    let (text, is_error) = encode_report(&report, entry, api);
    // a failed call reaches the model as an error result
    if is_error {
        return Err(
            ToolExecutionError::new(ToolErrorKind::Other, text.clone()).with_model_feedback(text)
        );
    }
    Ok(ToolOutput::text(text))
}
