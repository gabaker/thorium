//! Tests the Thorium MCP server's tools over its streamable HTTP transport

use rmcp::model::{CallToolRequestParams, CallToolResult, ErrorCode, ProtocolVersion};
use rmcp::service::{ClientLifecycleMode, ClientServiceExt, RunningService, ServiceError};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{ErrorData, RoleClient, ServiceExt};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};
use thorium::client::ResultsClient;
use thorium::models::{
    AgenticTool, AgenticToolBacking, ArgTarget, Entity, EntityMetadata, GenericJobArgs,
    ImageScaler, OutputDisplayType, OutputRequest, ReactionCursorOpts, ReactionSource,
    ReactionStatus, RunInputs, SampleRequest, ScopedTokenRequest, ScopedTokenRole, ToolIcon,
    ToolInputMapping, ToolMechanism, ToolRunRequest, ToolSpec, UserRole, WorkflowRunRequest,
};
use thorium::test_utilities::generators::{self, GroupRoles, TestUser};
use thorium::test_utilities::{self, runs};
use thorium::{Error, Thorium, is};
use uuid::Uuid;

/// A developer for every scaler
const DEVELOPER: UserRole = UserRole::Developer {
    k8s: true,
    bare_metal: true,
    windows: true,
    external: true,
    kvm: true,
};

/// A scoped token role that may run tools in every scaler
const SCOPED_DEVELOPER: ScopedTokenRole = ScopedTokenRole::Developer {
    k8s: true,
    bare_metal: true,
    windows: true,
    external: true,
    kvm: true,
};

/// How long a tool run tool may take to answer when it never waits for a job
const NO_WAIT: Duration = Duration::from_secs(1);

/// An MCP client session with Thorium's MCP server
type Session = RunningService<RoleClient, ()>;

/// A runner and the group they own
struct Runner {
    /// The admin client
    admin: Thorium,
    /// The runner
    user: TestUser,
    /// The group the runner owns
    group: String,
    /// The suffix of this test's readable names
    suffix: String,
}

/// Create a developer named after a moon and a group named after a god that they own
///
/// # Arguments
///
/// * `moon` - The moon the runner is named after
/// * `god` - The god the group is named after
async fn runner(moon: &str, god: &str) -> Result<Runner, Error> {
    // a developer for every scaler who owns one group
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let name = generators::readable_name(moon, &suffix);
    let user = generators::named_user_with_role(&admin, &name, DEVELOPER).await?;
    let group = generators::god_group(
        &admin,
        god,
        &suffix,
        &GroupRoles::default().owner(&user.username),
    )
    .await?;
    Ok(Runner {
        admin,
        user,
        group,
        suffix,
    })
}

/// Upload a random file to a group and get its SHA256
///
/// # Arguments
///
/// * `group` - The group to upload the file to
/// * `client` - The client to upload with
async fn sample(group: &str, client: &Thorium) -> Result<String, Error> {
    // upload a random file
    let req: SampleRequest = generators::gen_sample(group);
    Ok(client.files.create(req).await?.sha256)
}

/// Store a JSON result on a file under a tool name
///
/// # Arguments
///
/// * `client` - The client to upload with
/// * `sha256` - The file the result is for
/// * `tool` - The tool name to store the result under
async fn result(client: &Thorium, sha256: &str, tool: &str) -> Result<(), Error> {
    // store a result naming its tool
    let req = OutputRequest::new(
        sha256.to_owned(),
        tool,
        format!("{{\"answer\": \"{tool}\"}}"),
        OutputDisplayType::Json,
    );
    client.files.create_result(req).await?;
    Ok(())
}

/// Open an MCP session with Thorium's MCP server authenticated by a token
///
/// # Arguments
///
/// * `client` - A client whose host the MCP server is under
/// * `token` - The raw token to authenticate with
async fn session(client: &Thorium, token: &str) -> Result<Session, Error> {
    // connect to the server's streamable HTTP endpoint with the token as a bearer token
    let config = StreamableHttpClientTransportConfig::with_uri(format!("{}/api/mcp", client.host))
        .auth_header(token);
    let transport = StreamableHttpClientTransport::from_config(config);
    ().serve(transport)
        .await
        .map_err(|err| Error::new(format!("failed to start an MCP session: {err}")))
}

/// Open an MCP session on the stateless revision, which discovers the server instead of
/// initializing a session
///
/// # Arguments
///
/// * `client` - A client whose host the MCP server is under
/// * `token` - The raw token to authenticate with
async fn stateless_session(client: &Thorium, token: &str) -> Result<Session, Error> {
    // connect to the server's streamable HTTP endpoint with the token as a bearer token
    let config = StreamableHttpClientTransportConfig::with_uri(format!("{}/api/mcp", client.host))
        .auth_header(token);
    let transport = StreamableHttpClientTransport::from_config(config);
    // discover the server on the stateless revision only
    let lifecycle = ClientLifecycleMode::Discover {
        preferred_versions: vec![ProtocolVersion::V_2026_07_28],
    };
    ().serve_with_lifecycle(transport, lifecycle)
        .await
        .map_err(|err| Error::new(format!("failed to discover the MCP server: {err}")))
}

/// Call a tool and get its result or the error the server answered with
///
/// # Arguments
///
/// * `session` - The MCP session to call the tool in
/// * `name` - The name of the tool to call
/// * `arguments` - The tool call's arguments
async fn call(
    session: &Session,
    name: &'static str,
    arguments: Value,
) -> Result<Result<CallToolResult, ErrorData>, Error> {
    // call the tool with its arguments
    let mut params = CallToolRequestParams::new(name);
    if let Some(arguments) = arguments.as_object() {
        params = params.with_arguments(arguments.clone());
    }
    match session.call_tool(params).await {
        Ok(result) => Ok(Ok(result)),
        Err(ServiceError::McpError(err)) => Ok(Err(err)),
        Err(err) => Err(Error::new(format!("calling {name} failed: {err}"))),
    }
}

/// Call a tool that must succeed and get its structured result
///
/// # Arguments
///
/// * `session` - The MCP session to call the tool in
/// * `name` - The name of the tool to call
/// * `arguments` - The tool call's arguments
async fn ok(session: &Session, name: &'static str, arguments: Value) -> Result<Value, Error> {
    // the call must succeed without a tool error
    let result = call(session, name, arguments)
        .await?
        .map_err(|err| Error::new(format!("{name} returned an error: {err:?}")))?;
    if result.is_error == Some(true) {
        return Err(Error::new(format!(
            "{name} returned a tool error: {result:?}"
        )));
    }
    result
        .structured_content
        .ok_or_else(|| Error::new(format!("{name} returned no structured content")))
}

/// Call a tool that must be refused and get the error it was refused with
///
/// # Arguments
///
/// * `session` - The MCP session to call the tool in
/// * `name` - The name of the tool to call
/// * `arguments` - The tool call's arguments
async fn refused(
    session: &Session,
    name: &'static str,
    arguments: Value,
) -> Result<ErrorData, Error> {
    // the call must come back as an error
    match call(session, name, arguments).await? {
        Ok(result) => Err(Error::new(format!(
            "{name} succeeded when it should be refused: {result:?}"
        ))),
        Err(err) => Ok(err),
    }
}

/// Build a container tool spec whose input maps onto a positional, a kwarg, a switch, and the
/// files to run on
///
/// # Arguments
///
/// * `image` - The Image tool instance the tool runs
fn mapped_spec(image: Uuid) -> ToolSpec {
    // start from a container tool backed by the image
    let ToolSpec::Agentic(mut tool) = runs::gen_container_spec(image) else {
        unreachable!("a container spec is always an Agentic tool spec")
    };
    // take a target, a depth, a verbose flag, and files
    let schema = json!({
        "type": "object",
        "properties": {
            "target": {"type": "string"},
            "depth": {"type": "integer"},
            "verbose": {"type": "boolean"},
            "files": {"type": "array", "items": {"type": "string"}},
        },
    });
    tool.input_schema = schema.as_object().cloned().unwrap_or_default();
    // map each property onto the job
    if let AgenticToolBacking::Container(container) = &mut tool.backing {
        container.inputs = ToolInputMapping {
            args: BTreeMap::from([
                ("target".to_owned(), ArgTarget::Positional),
                ("depth".to_owned(), ArgTarget::Kwarg("--depth".to_owned())),
                (
                    "verbose".to_owned(),
                    ArgTarget::Switch("--verbose".to_owned()),
                ),
            ]),
            samples: Some("files".to_owned()),
        };
    }
    ToolSpec::Agentic(tool)
}

/// Create an Image tool and a container tool with a mapped input that runs it
///
/// # Arguments
///
/// * `groups` - The groups the tools are in
/// * `admin` - The admin client
async fn mapped_tool(groups: &[&str], admin: &Thorium) -> Result<Entity, Error> {
    // an Image tool and a container tool with a mapped input
    let (_, image) = runs::image_tool(groups, ImageScaler::K8s, admin).await?;
    let (_, boxed) = generators::tool(groups, "1.0.0", &mapped_spec(image.id), admin).await?;
    Ok(boxed)
}

/// Get the reaction id a structured result names
///
/// # Arguments
///
/// * `value` - The structured result
fn reaction_id(value: &Value) -> Result<Uuid, Error> {
    // read the reaction field as a uuid
    value
        .get("reaction")
        .and_then(Value::as_str)
        .and_then(|raw| Uuid::parse_str(raw).ok())
        .ok_or_else(|| Error::new(format!("no reaction id in {value}")))
}

/// Count the reactions a user created in a group
///
/// # Arguments
///
/// * `user` - The user who created the reactions
/// * `group` - The group to count in
async fn reaction_count(user: &TestUser, group: &str) -> Result<usize, Error> {
    // every reaction is tagged with its creator
    let opts = ReactionCursorOpts::default().group(group).limit(1000);
    let cursor = user
        .client
        .reactions
        .list_tag_new(&user.username, &opts)
        .await?;
    Ok(cursor.data.len())
}

/// With a run token, `run_tool` runs only the run's own tools, as sub runs of the run in its
/// group, on the run's files when the input names none; `get_sample` reads only the token's
/// groups
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn mcp_tool_runs_with_run_token() -> Result<(), Error> {
    // a runner with a second group, a workflow calling a mapped container tool over MCP, an
    // unbound tool, and a file in each group
    let runner = runner("isonoe", "fornax").await?;
    let second = generators::god_group(
        &runner.admin,
        "ceres",
        &runner.suffix,
        &GroupRoles::default().owner(&runner.user.username),
    )
    .await?;
    let groups = [runner.group.as_str(), second.as_str()];
    let boxed = mapped_tool(&groups, &runner.admin).await?;
    let unbound = mapped_tool(&groups, &runner.admin).await?;
    let triage = runs::ready_workflow(
        "Triage",
        &groups,
        &[(&boxed, ToolMechanism::Mcp)],
        &runner.admin,
    )
    .await?;
    let near = sample(&runner.group, &runner.user.client).await?;
    let far = sample(&second, &runner.user.client).await?;
    // a workflow run in the first group on the first file, and its token
    let created = runner
        .user
        .client
        .reactions
        .run_workflow(
            triage.id(),
            &WorkflowRunRequest::new(RunInputs::default().sample(&near)).group(&runner.group),
        )
        .await?;
    let token = runner
        .admin
        .reactions
        .run_token(&runner.group, created.id)
        .await?;
    is!(token.groups, vec![runner.group.clone()]);
    let mcp = session(&runner.user.client, &token.token).await?;
    // a file in the token's group is read and one only in the other group isn't found
    let found = ok(&mcp, "get_sample", json!({"sha256": near})).await?;
    is!(found["sha256"], json!(near));
    let err = refused(&mcp, "get_sample", json!({"sha256": far})).await?;
    is!(err.code, ErrorCode::RESOURCE_NOT_FOUND);
    // a tool outside the run is refused and creates nothing
    let before = (
        reaction_count(&runner.user, &runner.group).await?,
        reaction_count(&runner.user, &second).await?,
    );
    refused(
        &mcp,
        "run_tool",
        json!({"tool_instance": unbound.id, "input": {"files": [near]}}),
    )
    .await?;
    is!(
        (
            reaction_count(&runner.user, &runner.group).await?,
            reaction_count(&runner.user, &second).await?
        ),
        before
    );
    // the run's tool naming no files runs on the run's files as a sub run of the run
    let run = ok(
        &mcp,
        "run_tool",
        json!({"tool_instance": boxed.id, "input": {"target": "main"}}),
    )
    .await?;
    is!(run["group"], json!(runner.group));
    let call = runner
        .user
        .client
        .reactions
        .get(&runner.group, reaction_id(&run)?)
        .await?;
    is!(call.samples, vec![near.clone()]);
    is!(call.parent, Some(created.id));
    is!(call.run, Some(created.id));
    // naming the other group still runs in the run's group under the run
    let run = ok(
        &mcp,
        "run_tool",
        json!({"tool_instance": boxed.id, "input": {"files": [near]}, "group": second}),
    )
    .await?;
    is!(run["group"], json!(runner.group));
    let call = runner
        .user
        .client
        .reactions
        .get(&runner.group, reaction_id(&run)?)
        .await?;
    is!(call.parent, Some(created.id));
    // the run's tool calls are read back with the token
    let status = ok(&mcp, "get_tool_run", json!({"reaction": call.id})).await?;
    is!(status["group"], json!(runner.group));
    // close the session
    mcp.cancel()
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    Ok(())
}

/// The MCP revision the server speaks without sessions
const STATELESS_REVISION: &str = "2026-07-28";

/// Call a tool over plain HTTP the way a stateless client does, with no session
///
/// # Arguments
///
/// * `client` - A client whose host the MCP server is under
/// * `token` - The raw token to authenticate with
/// * `host` - The `Host` header to send, if not the client's own
/// * `name` - The name of the tool to call
/// * `arguments` - The tool call's arguments
async fn stateless_call(
    client: &Thorium,
    token: &str,
    host: Option<&str>,
    name: &str,
    arguments: Value,
) -> Result<reqwest::Response, Error> {
    // a self-contained tools/call request carrying its revision, client, and capabilities
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": name,
            "arguments": arguments,
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": STATELESS_REVISION,
                "io.modelcontextprotocol/clientInfo": {"name": "thorium-tests", "version": "1.0.0"},
                "io.modelcontextprotocol/clientCapabilities": {}
            }
        }
    });
    let mut req = reqwest::Client::new()
        .post(format!("{}/api/mcp", client.host))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", STATELESS_REVISION)
        .header("Mcp-Method", "tools/call")
        .header("Mcp-Name", name)
        .body(body.to_string());
    // send another host if asked to
    if let Some(host) = host {
        req = req.header("Host", host);
    }
    req.send()
        .await
        .map_err(|err| Error::new(format!("calling {name} over HTTP failed: {err}")))
}

/// Read the JSON-RPC response from a stateless call's body, sent as JSON or as an event stream
///
/// # Arguments
///
/// * `body` - The response body
fn rpc_response(body: &str) -> Result<Value, Error> {
    // a JSON body is the response itself
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        return Ok(value);
    }
    // in an event stream the response is the data line with the request's id
    body.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|data| serde_json::from_str::<Value>(data.trim()).ok())
        .find(|value| value.get("id").is_some())
        .ok_or_else(|| Error::new(format!("no JSON-RPC response in {body}")))
}

/// With a user token the server lists its 10 live tools, each with its title and annotations,
/// and speaks the stateless revision to a current client
#[tokio::test]
async fn mcp_tools_list_titles_and_annotations() -> Result<(), Error> {
    // a runner with a file and a client on the stateless revision
    let runner = runner("thebe", "vulcan").await?;
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let mcp = stateless_session(&runner.user.client, &runner.user.token).await?;
    // the client and server agree on the stateless revision
    let revision = mcp
        .peer_info()
        .map(|info| info.protocol_version.to_string());
    is!(revision.as_deref(), Some(STATELESS_REVISION));
    // the client gets a sample without a session
    let found = ok(&mcp, "get_sample", json!({"sha256": sha256})).await?;
    is!(found["sha256"], json!(sha256));
    // the title, read-only, and idempotent hints of each live tool
    let expected = [
        ("get_sample", "Get sample", true, true),
        ("get_sample_results", "Get sample results", true, true),
        (
            "list_sample_result_file_paths",
            "List result files",
            true,
            true,
        ),
        ("get_sample_result_file", "Get result file", true, true),
        ("list_images", "List images", true, true),
        ("list_pipelines", "List pipelines", true, true),
        ("start_tree", "Crawl related data", true, false),
        ("run_tool", "Run tool", false, false),
        ("get_tool_run", "Get tool run", true, true),
        ("get_tool_run_results", "Get tool run results", true, true),
    ];
    // the server lists exactly those tools
    let listed = mcp
        .list_all_tools()
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    let mut names = listed
        .iter()
        .map(|tool| tool.name.to_string())
        .collect::<Vec<String>>();
    names.sort();
    let mut wanted = expected
        .iter()
        .map(|(name, ..)| (*name).to_owned())
        .collect::<Vec<String>>();
    wanted.sort();
    is!(names, wanted);
    // each has its title and annotations, none destructive and none reaching outside Thorium
    for (name, title, read_only, idempotent) in expected {
        let tool = listed
            .iter()
            .find(|tool| tool.name == name)
            .ok_or_else(|| Error::new(format!("{name} isn't listed")))?;
        is!(tool.title.as_deref(), Some(title));
        let annotations = tool
            .annotations
            .as_ref()
            .ok_or_else(|| Error::new(format!("{name} has no annotations")))?;
        is!(annotations.title.as_deref(), Some(title));
        is!(annotations.read_only_hint, Some(read_only));
        is!(annotations.destructive_hint, Some(false));
        is!(annotations.idempotent_hint, Some(idempotent));
        is!(annotations.open_world_hint, Some(false));
    }
    // close the session
    mcp.cancel()
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    Ok(())
}

/// A client on the stateless revision calls `get_sample` with no session, and gets the sample
/// as structured content and as a text block with the same JSON
#[tokio::test]
async fn mcp_stateless_call_without_session() -> Result<(), Error> {
    // a runner with a file
    let runner = runner("amalthea", "minerva").await?;
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    // call get_sample with no session
    let response = stateless_call(
        &runner.user.client,
        &runner.user.token,
        None,
        "get_sample",
        json!({"sha256": sha256}),
    )
    .await?;
    is!(response.status().as_u16(), 200);
    // the server opens no session
    is!(response.headers().get("Mcp-Session-Id").is_none(), true);
    // the result holds the sample as structured content
    let body = response
        .text()
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    let rpc = rpc_response(&body)?;
    let answer = &rpc["result"];
    is!(answer["isError"], json!(false));
    is!(answer["structuredContent"]["sha256"], json!(sha256));
    // and one text block holds the same JSON
    let content = answer["content"].as_array().cloned().unwrap_or_default();
    is!(content.len(), 1);
    let text = content[0]["text"].as_str().unwrap_or_default();
    let parsed: Value = serde_json::from_str(text).map_err(|err| Error::new(err.to_string()))?;
    is!(parsed, answer["structuredContent"].clone());
    // a list result is one text block holding the whole list too
    let tool = format!("sundial-{}", runner.suffix);
    result(&runner.user.client, &sha256, &tool).await?;
    let response = stateless_call(
        &runner.user.client,
        &runner.user.token,
        None,
        "list_sample_result_file_paths",
        json!({"sha256": sha256, "tool": tool}),
    )
    .await?;
    let body = response
        .text()
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    let rpc = rpc_response(&body)?;
    let content = rpc["result"]["content"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    is!(content.len(), 1);
    is!(content[0]["text"], json!("[]"));
    Ok(())
}

/// The MCP server accepts requests addressed to the in-cluster service names and refuses
/// other hosts, so a page on another site can't reach it through DNS rebinding
#[tokio::test]
async fn mcp_allowed_hosts() -> Result<(), Error> {
    // a runner with a file
    let runner = runner("leda", "vesta").await?;
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    // a request addressed to the in-cluster API service is served
    let namespace = &test_utilities::CONF.thorium.namespace;
    let service = format!("thorium-api.{namespace}.svc.cluster.local");
    let response = stateless_call(
        &runner.user.client,
        &runner.user.token,
        Some(&service),
        "get_sample",
        json!({"sha256": sha256}),
    )
    .await?;
    is!(response.status().as_u16(), 200);
    // a request addressed to another host is refused before any tool runs
    let response = stateless_call(
        &runner.user.client,
        &runner.user.token,
        Some("rebind.example.com"),
        "get_sample",
        json!({"sha256": sha256}),
    )
    .await?;
    is!(response.status().as_u16(), 403);
    Ok(())
}

/// Build a run tool spec backed by one of the server's MCP tools
///
/// # Arguments
///
/// * `backing` - The MCP tool backing the run tool
/// * `output_schema` - The schema the run tool's results must match, if any
fn mcp_run_tool(backing: &str, output_schema: Option<Value>) -> ToolSpec {
    // start from a random agentic tool and back it with the MCP tool
    let ToolSpec::Agentic(mut tool) = generators::gen_agentic_spec() else {
        unreachable!("an agentic spec is always an Agentic tool spec")
    };
    tool.backing = AgenticToolBacking::ThoriumMcp {
        tool: backing.to_owned(),
    };
    tool.output_schema = output_schema.and_then(|schema| schema.as_object().cloned());
    ToolSpec::Agentic(tool)
}

/// Get the agentic tool a tool instance runs
///
/// # Arguments
///
/// * `instance` - The tool instance
fn agentic(instance: &Entity) -> Result<AgenticTool, Error> {
    // only an agentic tool instance has one
    match &instance.metadata {
        EntityMetadata::ToolInstance(tool) => match &tool.spec {
            ToolSpec::Agentic(agentic) => Ok(agentic.clone()),
            spec => Err(Error::new(format!("{spec:?} isn't an agentic tool"))),
        },
        metadata => Err(Error::new(format!("{metadata:?} isn't a tool instance"))),
    }
}

/// Run a workflow whose agent calls some tools over MCP on a file and get its run token
///
/// # Arguments
///
/// * `runner` - The runner
/// * `tools` - The tool instances the agent calls
/// * `sha256` - The file to run on, in the runner's group
async fn run_token_for(runner: &Runner, tools: &[&Entity], sha256: &str) -> Result<String, Error> {
    // a workflow calling each tool over MCP
    let bound = tools
        .iter()
        .map(|tool| (*tool, ToolMechanism::Mcp))
        .collect::<Vec<_>>();
    let workflow =
        runs::ready_workflow("Survey", &[runner.group.as_str()], &bound, &runner.admin).await?;
    // a run of it in the runner's group and its token
    let created = runner
        .user
        .client
        .reactions
        .run_workflow(
            workflow.id(),
            &WorkflowRunRequest::new(RunInputs::default().sample(sha256)).group(&runner.group),
        )
        .await?;
    let token = runner
        .admin
        .reactions
        .run_token(&runner.group, created.id)
        .await?;
    Ok(token.token)
}

/// With a run token the server lists only the MCP tools the run's tools use, each described
/// from its run tool, plus the tool run tools for its container tool; the full list is
/// unchanged for a user token, a hidden tool still answers with the token's normal limits, and
/// a JSON result carries the same JSON as structured content and text
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn mcp_run_scoped_tools_list() -> Result<(), Error> {
    // a runner with a second group and a file
    let runner = runner("sinope", "bellona").await?;
    let second = generators::god_group(
        &runner.admin,
        "faunus",
        &runner.suffix,
        &GroupRoles::default().owner(&runner.user.username),
    )
    .await?;
    let groups = [runner.group.as_str(), second.as_str()];
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    // two run tools backed by get_sample, one titled with an output schema and an icon
    let ToolSpec::Agentic(mut described) = mcp_run_tool(
        "get_sample",
        Some(
            json!({"type": "object", "properties": {"sha256": {"type": "string"}}, "required": ["sha256"]}),
        ),
    ) else {
        unreachable!("a run tool is an Agentic tool spec")
    };
    described.title = Some(format!("Read the sample {}", runner.suffix));
    described.icons = vec![ToolIcon {
        src: "https://example.com/sample.png".to_owned(),
        mime_type: Some("image/png".to_owned()),
        sizes: vec!["48x48".to_owned()],
        theme: None,
    }];
    let (_, described) = generators::tool(
        &groups,
        "1.0.0",
        &ToolSpec::Agentic(described),
        &runner.admin,
    )
    .await?;
    let (_, shared) = generators::tool(
        &groups,
        "1.0.0",
        &mcp_run_tool("get_sample", None),
        &runner.admin,
    )
    .await?;
    // a run tool backed by get_sample_results and a container tool
    let (_, results) = generators::tool(
        &groups,
        "1.0.0",
        &mcp_run_tool("get_sample_results", None),
        &runner.admin,
    )
    .await?;
    let boxed = mapped_tool(&groups, &runner.admin).await?;
    let token = Box::pin(run_token_for(
        &runner,
        &[&described, &shared, &results, &boxed],
        &sha256,
    ))
    .await?;
    let mcp = session(&runner.user.client, &token).await?;
    let user = session(&runner.user.client, &runner.user.token).await?;
    // the run's listing holds the two MCP tools and the tool run tools
    let listed = mcp
        .list_all_tools()
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    let mut names = listed
        .iter()
        .map(|tool| tool.name.to_string())
        .collect::<Vec<String>>();
    names.sort();
    is!(
        names,
        vec![
            "get_sample".to_owned(),
            "get_sample_results".to_owned(),
            "get_tool_run".to_owned(),
            "get_tool_run_results".to_owned(),
            "run_tool".to_owned(),
        ]
    );
    // the user's listing is the full list
    let full = user
        .list_all_tools()
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    is!(full.len(), 10);
    // get_sample is described from the run tool with the lower instance id
    let lowest = if described.id < shared.id {
        agentic(&described)?
    } else {
        agentic(&shared)?
    };
    let expected = lowest.to_mcp_tool();
    let find = |tools: &[rmcp::model::Tool], name: &str| {
        tools.iter().find(|tool| tool.name == name).cloned()
    };
    let sample_tool = find(&listed, "get_sample")
        .ok_or_else(|| Error::new("get_sample isn't listed".to_owned()))?;
    is!(sample_tool.title, expected.title);
    is!(sample_tool.description, expected.description);
    is!(sample_tool.annotations, expected.annotations);
    is!(sample_tool.output_schema, expected.output_schema);
    is!(sample_tool.icons, expected.icons);
    // with the server's own input schema
    let server_tool = find(&full, "get_sample")
        .ok_or_else(|| Error::new("get_sample isn't served".to_owned()))?;
    is!(sample_tool.input_schema, server_tool.input_schema);
    // a JSON result carries the sample as structured content and as text
    let result = call(&mcp, "get_sample", json!({"sha256": sha256}))
        .await?
        .map_err(|err| Error::new(format!("get_sample failed: {err:?}")))?;
    let structured = result
        .structured_content
        .clone()
        .ok_or_else(|| Error::new("no structured content".to_owned()))?;
    is!(structured["sha256"], json!(sha256));
    is!(result.content.len(), 1);
    let text = result.content[0]
        .as_text()
        .map_or_else(String::new, |text| text.text.clone());
    let parsed: Value = serde_json::from_str(&text).map_err(|err| Error::new(err.to_string()))?;
    is!(parsed, structured);
    // a hidden tool answers with the token's normal limits: its group but not another
    let images = ok(&mcp, "list_images", json!({"group": runner.group})).await?;
    is!(images["data"].is_array(), true);
    refused(&mcp, "list_images", json!({"group": second})).await?;
    ok(&user, "list_images", json!({"group": second})).await?;
    // close the sessions
    mcp.cancel()
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    user.cancel()
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    Ok(())
}

/// With a run token, a result matching its run tool's output schema comes back as structured
/// content and one missing a required property is an error naming it
#[tokio::test]
async fn mcp_run_tool_output_schema() -> Result<(), Error> {
    // a runner with a file that has a result
    let runner = runner("kale", "pomona").await?;
    let groups = [runner.group.as_str()];
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let tool = format!("almanac-{}", runner.suffix);
    result(&runner.user.client, &sha256, &tool).await?;
    // a run tool whose results match its schema and one whose schema needs a missing property
    let (_, matching) = generators::tool(
        &groups,
        "1.0.0",
        &mcp_run_tool(
            "get_sample",
            Some(json!({"type": "object", "properties": {"sha256": {"type": "string"}}, "required": ["sha256"]})),
        ),
        &runner.admin,
    )
    .await?;
    let (_, strict) = generators::tool(
        &groups,
        "1.0.0",
        &mcp_run_tool(
            "get_sample_results",
            Some(json!({"type": "object", "required": ["verdict"]})),
        ),
        &runner.admin,
    )
    .await?;
    let token = Box::pin(run_token_for(&runner, &[&matching, &strict], &sha256)).await?;
    let mcp = session(&runner.user.client, &token).await?;
    // the matching result comes back as structured content
    let found = ok(&mcp, "get_sample", json!({"sha256": sha256})).await?;
    is!(found["sha256"], json!(sha256));
    // the result missing the required property is an error naming it
    let checked = call(&mcp, "get_sample_results", json!({"sha256": sha256}))
        .await?
        .map_err(|err| Error::new(format!("get_sample_results failed: {err:?}")))?;
    is!(checked.is_error, Some(true));
    is!(checked.structured_content.is_none(), true);
    let text = checked.content[0]
        .as_text()
        .map_or_else(String::new, |text| text.text.clone());
    is!(text.contains("verdict"), true);
    // the same tool with the user's token isn't checked
    let user = session(&runner.user.client, &runner.user.token).await?;
    let results = ok(&user, "get_sample_results", json!({"sha256": sha256})).await?;
    is!(results[&tool], json!({"answer": tool.clone()}));
    // close the sessions
    mcp.cancel()
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    user.cancel()
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    Ok(())
}

/// `run_tool` starts a tool run without waiting, mapping its input onto the job, and
/// `get_tool_run` answers with the run's status and a polling hint without waiting; results of
/// an unfinished run and unknown runs are refused at once
#[tokio::test]
async fn mcp_run_tool_maps_input_without_waiting() -> Result<(), Error> {
    // a runner, a container tool with a mapped input, and a file
    let runner = runner("adrastea", "janus").await?;
    let groups = [runner.group.as_str()];
    let boxed = mapped_tool(&groups, &runner.admin).await?;
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let mcp = session(&runner.user.client, &runner.user.token).await?;
    // start a run of the tool while no worker can claim its job
    let input = json!({"target": "main", "depth": 2, "verbose": true, "files": [sha256]});
    let started = Instant::now();
    let run = ok(
        &mcp,
        "run_tool",
        json!({"tool_instance": boxed.id, "input": input}),
    )
    .await?;
    is!((started.elapsed() < NO_WAIT), true);
    let id = reaction_id(&run)?;
    is!(run["group"], json!(runner.group));
    // the run is a tool run of the container tool on the mapped files with the mapped args
    let reaction = runner.user.client.reactions.get(&runner.group, id).await?;
    is!(
        matches!(
            reaction.source,
            ReactionSource::Tool { tool_instance, .. } if tool_instance == boxed.id
        ),
        true
    );
    is!(reaction.samples, vec![sha256.clone()]);
    is!(reaction.parent, None::<Uuid>);
    let stage = reaction.plan.stages[0][0].name.clone();
    let expected = GenericJobArgs::default()
        .positionals(vec!["main"])
        .kwarg("--depth", vec!["2"])
        .switches(vec!["--verbose"]);
    is!(reaction.args.get(&stage), Some(&expected));
    // its status comes back without waiting, with a polling hint
    let started = Instant::now();
    let status = ok(&mcp, "get_tool_run", json!({"reaction": id})).await?;
    is!((started.elapsed() < NO_WAIT), true);
    is!(status["status"], json!("Created"));
    is!(status["group"], json!(runner.group));
    let hint = status["retry_after_secs"].as_u64();
    is!(hint.is_some_and(|secs| (1..=60).contains(&secs)), true);
    // the group returned by run_tool reads the same run
    let grouped = ok(
        &mcp,
        "get_tool_run",
        json!({"reaction": id, "group": runner.group}),
    )
    .await?;
    is!(grouped["status"], json!("Created"));
    // the results of the unfinished run are refused at once, saying it hasn't finished
    let started = Instant::now();
    let err = refused(&mcp, "get_tool_run_results", json!({"reaction": id})).await?;
    is!((started.elapsed() < NO_WAIT), true);
    is!(err.message.contains("hasn't finished"), true);
    // an unknown run isn't found
    let err = refused(&mcp, "get_tool_run", json!({"reaction": Uuid::new_v4()})).await?;
    is!(err.code, ErrorCode::RESOURCE_NOT_FOUND);
    // input the mapping can't place is refused and starts nothing
    let err = refused(
        &mcp,
        "run_tool",
        json!({"tool_instance": boxed.id, "input": {"verbose": "yes", "files": [sha256]}}),
    )
    .await?;
    is!(err.code, ErrorCode::INVALID_PARAMS);
    is!(err.message.contains("verbose"), true);
    // close the session
    mcp.cancel()
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    Ok(())
}

/// `get_tool_run_results` returns a finished tool run's results by file and tool name, and a
/// finished run has no polling hint
#[tokio::test]
async fn mcp_tool_run_results() -> Result<(), Error> {
    // a runner, a container tool with a mapped input, and two files
    let runner = runner("amalthea", "minerva").await?;
    let groups = [runner.group.as_str()];
    let boxed = mapped_tool(&groups, &runner.admin).await?;
    let first = sample(&runner.group, &runner.user.client).await?;
    let second = sample(&runner.group, &runner.user.client).await?;
    let mcp = session(&runner.user.client, &runner.user.token).await?;
    // run the tool on both files
    let run = ok(
        &mcp,
        "run_tool",
        json!({"tool_instance": boxed.id, "input": {"files": [first, second]}}),
    )
    .await?;
    let id = reaction_id(&run)?;
    let reaction = runner.user.client.reactions.get(&runner.group, id).await?;
    let tool = reaction.plan.stages[0][0].name.clone();
    // the tool stores a result on each file under its name and the run completes
    result(&runner.user.client, &first, &tool).await?;
    result(&runner.user.client, &second, &tool).await?;
    runs::handle_reaction(
        &runner.user.client,
        &runner.user.auth_header(),
        &runner.group,
        id,
        "proceed",
    )
    .await?;
    let reaction = runner.user.client.reactions.get(&runner.group, id).await?;
    is!(reaction.status, ReactionStatus::Completed);
    // a finished run has no polling hint
    let status = ok(&mcp, "get_tool_run", json!({"reaction": id})).await?;
    is!(status["status"], json!("Completed"));
    is!(status["retry_after_secs"], Value::Null);
    // its results come back per file under the tool's name
    let results = ok(&mcp, "get_tool_run_results", json!({"reaction": id})).await?;
    is!(results["tool"], json!(tool));
    for sha256 in [&first, &second] {
        is!(
            results["results"][sha256]["result"],
            json!({"answer": tool.clone()})
        );
    }
    // a failed run has no polling hint and no results
    let failed = runner
        .user
        .client
        .reactions
        .run_tool(
            boxed.id,
            &ToolRunRequest::new(RunInputs::default().sample(&first)),
        )
        .await?;
    runs::handle_reaction(
        &runner.user.client,
        &runner.user.auth_header(),
        &runner.group,
        failed.id,
        "fail",
    )
    .await?;
    let status = ok(&mcp, "get_tool_run", json!({"reaction": failed.id})).await?;
    is!(status["status"], json!("Failed"));
    is!(status["retry_after_secs"], Value::Null);
    let err = refused(&mcp, "get_tool_run_results", json!({"reaction": failed.id})).await?;
    is!(err.message.contains("failed"), true);
    // close the session
    mcp.cancel()
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    Ok(())
}

/// A completed tool run's results come back over MCP whatever their display type, so a tool
/// whose results are hidden from the UI still answers its callers
#[tokio::test]
async fn mcp_tool_run_hidden_results() -> Result<(), Error> {
    // a runner, a container tool with a mapped input, and a file
    let runner = runner("eurydome", "pales").await?;
    let groups = [runner.group.as_str()];
    let boxed = mapped_tool(&groups, &runner.admin).await?;
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let mcp = session(&runner.user.client, &runner.user.token).await?;
    // run the tool on the file
    let run = ok(
        &mcp,
        "run_tool",
        json!({"tool_instance": boxed.id, "input": {"files": [sha256]}}),
    )
    .await?;
    let id = reaction_id(&run)?;
    let reaction = runner.user.client.reactions.get(&runner.group, id).await?;
    let tool = reaction.plan.stages[0][0].name.clone();
    // the tool stores a hidden result on the file and the run completes
    let req = OutputRequest::new(
        sha256.clone(),
        &tool,
        "{\"answer\": \"hidden\"}".to_owned(),
        OutputDisplayType::Hidden,
    );
    runner.user.client.files.create_result(req).await?;
    runs::handle_reaction(
        &runner.user.client,
        &runner.user.auth_header(),
        &runner.group,
        id,
        "proceed",
    )
    .await?;
    // the hidden result comes back under the tool's name
    let results = ok(&mcp, "get_tool_run_results", json!({"reaction": id})).await?;
    is!(
        results["results"][&sha256]["result"],
        json!({"answer": "hidden"})
    );
    // close the session
    mcp.cancel()
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    Ok(())
}

/// A token limited to some of the runner's groups reads samples and tool runs only in those
/// groups, and can't start runs of tool instances outside them
#[tokio::test]
async fn mcp_tool_runs_outside_token_groups() -> Result<(), Error> {
    // a runner with a second group, a tool and a file in each, and a run in the second
    let runner = runner("thebe", "vesta").await?;
    let second = generators::god_group(
        &runner.admin,
        "ceres",
        &runner.suffix,
        &GroupRoles::default().owner(&runner.user.username),
    )
    .await?;
    let inside = mapped_tool(&[runner.group.as_str()], &runner.admin).await?;
    let outside = mapped_tool(&[second.as_str()], &runner.admin).await?;
    let near = sample(&runner.group, &runner.user.client).await?;
    let far = sample(&second, &runner.user.client).await?;
    let hidden = runner
        .user
        .client
        .reactions
        .run_tool(
            outside.id,
            &ToolRunRequest::new(RunInputs::default().sample(&far)),
        )
        .await?;
    // a token limited to the first group
    let name = format!("mcp-{}", runner.suffix);
    let scoped = runner
        .user
        .client
        .users
        .create_scoped_token(
            &ScopedTokenRequest::new(&name)
                .role(SCOPED_DEVELOPER)
                .group(&runner.group),
        )
        .await?;
    let mcp = session(&runner.user.client, &scoped.token).await?;
    // a file in the token's group is read and one only in the other group isn't found
    let found = ok(&mcp, "get_sample", json!({"sha256": near})).await?;
    is!(found["sha256"], json!(near));
    let err = refused(&mcp, "get_sample", json!({"sha256": far})).await?;
    is!(err.code, ErrorCode::RESOURCE_NOT_FOUND);
    // a run in the other group isn't found, even when its group is named
    for args in [
        json!({"reaction": hidden.id}),
        json!({"reaction": hidden.id, "group": second}),
    ] {
        let err = refused(&mcp, "get_tool_run", args.clone()).await?;
        is!(err.code, ErrorCode::RESOURCE_NOT_FOUND);
        let err = refused(&mcp, "get_tool_run_results", args).await?;
        is!(err.code, ErrorCode::RESOURCE_NOT_FOUND);
    }
    // a tool instance outside the token's groups can't be run
    let err = refused(
        &mcp,
        "run_tool",
        json!({"tool_instance": outside.id, "input": {"files": [near]}}),
    )
    .await?;
    is!(err.code, ErrorCode::RESOURCE_NOT_FOUND);
    // the user's own token still reads the other group's run
    let user = session(&runner.user.client, &runner.user.token).await?;
    let status = ok(&user, "get_tool_run", json!({"reaction": hidden.id})).await?;
    is!(status["group"], json!(second));
    // and the tool inside the token's groups still runs
    let run = ok(
        &mcp,
        "run_tool",
        json!({"tool_instance": inside.id, "input": {"files": [near]}}),
    )
    .await?;
    is!(run["group"], json!(runner.group));
    // close both sessions
    mcp.cancel()
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    user.cancel()
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    Ok(())
}

/// The tools the MCP server already had keep their behavior with a user token
#[tokio::test]
async fn mcp_existing_tools_with_user_token() -> Result<(), Error> {
    // a runner with a file that has a result, some images, and a pipeline
    let runner = runner("metis", "pluto").await?;
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let tool = format!("sundial-{}", runner.suffix);
    result(&runner.user.client, &sha256, &tool).await?;
    let images = generators::images(&runner.group, 2, false, &runner.admin).await?;
    let pipelines = generators::pipelines(&runner.group, 1, false, &runner.admin).await?;
    let mcp = session(&runner.user.client, &runner.user.token).await?;
    // the file and its results are read
    let found = ok(&mcp, "get_sample", json!({"sha256": sha256})).await?;
    is!(found["sha256"], json!(sha256));
    let results = ok(&mcp, "get_sample_results", json!({"sha256": sha256})).await?;
    is!(results[&tool], json!({"answer": tool.clone()}));
    let paths = ok(
        &mcp,
        "list_sample_result_file_paths",
        json!({"sha256": sha256, "tool": tool}),
    )
    .await?;
    is!(paths, json!([]));
    // the stored images and pipelines are listed
    let listed = ok(&mcp, "list_images", json!({"group": runner.group})).await?;
    let names = listed["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|image| image["name"].as_str().map(str::to_owned))
        .collect::<Vec<String>>();
    for image in &images {
        is!(names.contains(&image.name), true);
    }
    let listed = ok(&mcp, "list_pipelines", json!({"group": runner.group})).await?;
    is!(listed.to_string().contains(&pipelines[0].name), true);
    // a tree grows from the file
    let tree = ok(&mcp, "start_tree", json!({"samples": [sha256]})).await?;
    is!(tree.to_string().contains(&sha256), true);
    // close the session
    mcp.cancel()
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    Ok(())
}

/// The server's tool list includes the tool run tools, so an Agentic tool may be backed by them
#[tokio::test]
async fn mcp_tool_run_tools_back_agentic_tools() -> Result<(), Error> {
    // a runner and an MCP session
    let runner = runner("callirrhoe", "neptune").await?;
    let mcp = session(&runner.user.client, &runner.user.token).await?;
    // the server lists the three tool run tools
    let listed = mcp
        .list_all_tools()
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    let names = listed
        .iter()
        .map(|tool| tool.name.to_string())
        .collect::<Vec<String>>();
    for name in ["run_tool", "get_tool_run", "get_tool_run_results"] {
        is!(names.contains(&name.to_owned()), true);
    }
    // an Agentic tool backed by each of them is accepted
    for name in ["run_tool", "get_tool_run", "get_tool_run_results"] {
        let ToolSpec::Agentic(mut tool) = generators::gen_agentic_spec() else {
            unreachable!("an agentic spec is always an Agentic tool spec")
        };
        tool.backing = AgenticToolBacking::ThoriumMcp {
            tool: name.to_owned(),
        };
        generators::tool(
            &[runner.group.as_str()],
            "1.0.0",
            &ToolSpec::Agentic(tool),
            &runner.admin,
        )
        .await?;
    }
    // close the session
    mcp.cancel()
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    Ok(())
}
