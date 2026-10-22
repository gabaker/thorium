//! MCP support for Thorium

use axum::Router;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use axum::response::IntoResponse;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::model::{
    CacheScope, CallToolRequestParams, CallToolResponse, CallToolResult, ListToolsResult,
    PaginatedRequestParams, ProtocolVersion, ResultType, ServerCapabilities, ServerConfig,
    ToolAnnotations,
};
use rmcp::service::RequestContext;
use rmcp::transport::StreamableHttpServerConfig;
use rmcp::transport::streamable_http_server::StreamableHttpService;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::{ErrorData, RoleServer, tool_handler};
use serde::Serialize;
use std::net::IpAddr;
use std::str::FromStr;
use tower::ServiceExt;
use tracing::{Level, event};

mod files;
mod images;
mod pipelines;
mod run_scope;
mod runs;
mod trees;

use crate::Conf;
use crate::client::Thorium;
use crate::models::{AuthedUser, RunTokenScope};
use crate::utils::AppState;
use run_scope::RunTools;

/// The config info needed for mcp clients
#[derive(Clone, Copy)]
pub struct McpConfig {
    // The ip address to talk to the API at
    pub ip: IpAddr,
    /// The port to ues to talk to our api
    pub port: u16,
}

impl McpConfig {
    /// Get the url to talk to the Thorium API at
    pub fn get_url(&self) -> String {
        // build the url to talk to Thorium at
        format!("http://{}:{}", self.ip, self.port)
    }

    /// Grab our token from this requests parts
    ///
    /// # Arguments
    ///
    /// * `parts` - The parts to grab our token from
    fn grab_token(parts: &Parts) -> Result<&str, ErrorData> {
        // get our authorizaton header if it exists
        match parts.headers.get("Authorization") {
            Some(value) => {
                // decode the header as a str without panicking so a non-UTF8 Authorization
                // header returns a clean INVALID_PARAMS error instead of crashing the request task
                let value_str = value.to_str().map_err(|_| ErrorData {
                    code: rmcp::model::ErrorCode::INVALID_PARAMS,
                    message: "Authorization header is not valid UTF-8".into(),
                    data: None,
                })?;
                // split this on spaces and get our token
                // if there isn't a space then assume they passed just a token
                match value_str.split_once(' ') {
                    Some((_, token)) => Ok(token),
                    None => Ok(value_str),
                }
            }
            // we are missing an authorization header to reject this request
            None => Err(ErrorData {
                code: rmcp::model::ErrorCode::INVALID_PARAMS,
                message: "Missing authorization header".into(),
                data: None,
            }),
        }
    }

    /// Get a Thorium client to use for this MCP session
    ///
    /// # Arguments
    ///
    /// * `parts` - The request parts to get token info from
    pub async fn client(&self, parts: &Parts) -> Result<Thorium, ErrorData> {
        // build the url to talk to Thorium at
        let url = self.get_url();
        // get our authorization token
        let token = Self::grab_token(parts)?;
        // get a thorim client
        let thorium = Thorium::build(&url).token(token).build().await?;
        Ok(thorium)
    }
}

impl From<&Conf> for McpConfig {
    fn from(conf: &Conf) -> McpConfig {
        // get our interface as an ip address
        let ip = IpAddr::from_str(&conf.thorium.interface).unwrap();
        // build our mcp config
        McpConfig {
            ip,
            port: conf.thorium.port,
        }
    }
}

#[derive(Clone)]
pub struct ThoriumMCP {
    conf: McpConfig,
    tool_router: ToolRouter<Self>,
}

#[tool_handler(router = self.tool_router)]
impl rmcp::ServerHandler for ThoriumMCP {
    /// Describe this server and the capabilities it offers
    fn get_info(&self) -> ServerConfig {
        // offer tools and say what Thorium is
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("A file analysis and data generation platform.")
    }

    /// List the tools this server serves, or only the run's tools for a run-scoped token
    ///
    /// # Arguments
    ///
    /// * `_request` - The listing request, which has nothing to page through
    /// * `context` - The request's context, carrying its HTTP request parts
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        // list every tool, or only the run's tools for a run-scoped token
        let all = self.tool_router.list_all();
        let (tools, scope) = match self.run_tools(&context).await? {
            Some(run_tools) => (run_tools.list(all), CacheScope::Private),
            None => (all, CacheScope::Public),
        };
        // only the stateless revision carries cache hints
        let hints = context
            .protocol_version()
            .is_some_and(|version| version >= ProtocolVersion::V_2026_07_28);
        // a run's listing is its token's own, so only that client may cache it
        Ok(ListToolsResult {
            result_type: Some(ResultType::COMPLETE),
            tools,
            meta: None,
            next_cursor: None,
            ttl_ms: hints.then_some(0),
            cache_scope: hints.then_some(scope),
        })
    }

    /// Call a tool, checking a run-scoped token's results against its run tool's output schema
    ///
    /// # Arguments
    ///
    /// * `request` - The tool call
    /// * `context` - The request's context, carrying its HTTP request parts
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        // read the run's tools before the call takes the context
        let run_tools = self.run_tools(&context).await?;
        let name = request.name.to_string();
        // call the tool with the token's normal limits
        let called = ToolCallContext::new(self, request, context);
        let response = self.tool_router.call(called).await?;
        // a run tool's finished result must match its output schema
        match (run_tools, response) {
            (Some(run_tools), CallToolResponse::Complete(result)) => {
                Ok(CallToolResponse::Complete(run_tools.check(&name, result)))
            }
            (_, response) => Ok(response),
        }
    }
}

impl ThoriumMCP {
    /// Read the run tools of the run a request's token is limited to, if it is run-scoped
    ///
    /// # Arguments
    ///
    /// * `context` - The request's context, carrying its HTTP request parts
    async fn run_tools(
        &self,
        context: &RequestContext<RoleServer>,
    ) -> Result<Option<RunTools>, ErrorData> {
        // the HTTP layer marks requests made with a run-scoped token with the token's run
        let Some(parts) = context.extensions.get::<Parts>() else {
            return Ok(None);
        };
        let Some(scope) = parts.extensions.get::<RunTokenScope>() else {
            return Ok(None);
        };
        // read the run's tools with the token itself
        let thorium = self.conf.client(parts).await?;
        Ok(Some(RunTools::load(&thorium, scope).await?))
    }

    /// Build a new Thorium MCP instance
    ///
    /// # Arguments
    ///
    /// * `mcp_conf` - The mcp config to use
    pub fn new(mcp_conf: McpConfig) -> Self {
        Self {
            conf: mcp_conf,
            tool_router: Self::router(),
        }
    }

    /// Build the router holding every tool Thorium's MCP server exposes
    fn router() -> ToolRouter<Self> {
        // combine the sample, image, pipeline, tree, and tool run tools
        let router = Self::sample_router()
            + Self::images_router()
            + Self::pipelines_router()
            + Self::tree_router()
            + Self::tool_runs_router();
        // give each tool its title and annotations
        describe_live_tools(router)
    }
}

/// Build a tool result holding one JSON value
///
/// # Arguments
///
/// * `value` - The value to return
pub(crate) fn json_result<T: Serialize>(value: &T) -> Result<CallToolResult, ErrorData> {
    // serialize the value
    let value = serde_json::to_value(value)
        .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
    // return the value as structured content and as a text block with the same JSON
    Ok(CallToolResult::structured(value))
}

/// The title, read-only hint, and idempotent hint of each tool the server serves
///
/// No tool is destructive or reaches outside Thorium.
const LIVE_TOOLS: [(&str, &str, bool, bool); 10] = [
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

/// Give each tool in a router its title and annotations
///
/// # Arguments
///
/// * `router` - The router whose tools to describe
fn describe_live_tools<S>(mut router: ToolRouter<S>) -> ToolRouter<S> {
    for route in router.map.values_mut() {
        // find this tool's title and hints
        let Some((_, title, read_only, idempotent)) = LIVE_TOOLS
            .iter()
            .find(|(name, ..)| route.attr.name == *name)
        else {
            continue;
        };
        // title the tool and annotate how it may be used
        route.attr.title = Some((*title).to_owned());
        route.attr.annotations = Some(
            ToolAnnotations::with_title(*title)
                .read_only(*read_only)
                .destructive(false)
                .idempotent(*idempotent)
                .open_world(false),
        );
    }
    router
}

impl ThoriumMCP {
    /// Get the names of every tool Thorium's MCP server exposes
    #[must_use]
    pub fn tool_names() -> Vec<String> {
        // list every tool in the same router the server is built from
        Self::router()
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect()
    }
}

/// Get the run a request's token is limited to, if it is a run-scoped token
///
/// # Arguments
///
/// * `state` - Shared Thorium objects
/// * `headers` - The request's headers
async fn run_scope(state: &AppState, headers: &HeaderMap) -> Option<RunTokenScope> {
    // MCP clients send the raw token, alone or after its scheme
    let header = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let token = header.split_once(' ').map_or(header, |(_, token)| token);
    // authenticate it and keep its run scope
    let user = AuthedUser::from_raw_token(token, &state.shared)
        .await
        .ok()?;
    user.run().cloned()
}

/// Mount the mcp service to our api
///
/// # Arguments
///
/// * `router` - The router to add routes too
/// * `conf` - The Thorium config
pub fn mount(router: Router<AppState>, conf: &Conf) -> Router<AppState> {
    // create a new mcp config
    let mcp_conf = McpConfig::from(conf);
    // accept only requests addressed to Thorium's own hosts, so a page on another site can't
    // reach the server through DNS rebinding
    let allowed_hosts = conf.thorium.mcp_allowed_hosts();
    event!(
        Level::INFO,
        msg = "MCP server allowed hosts",
        hosts = allowed_hosts.join(",")
    );
    let server_conf = StreamableHttpServerConfig::default()
        .with_allowed_hosts(allowed_hosts)
        .with_allowed_origins(conf.thorium.mcp.allowed_origins.clone());
    // create a new service
    let service = StreamableHttpService::new(
        move || Ok(ThoriumMCP::new(mcp_conf)),
        LocalSessionManager::default().into(),
        server_conf,
    );
    // mark requests made with a run-scoped token with the token's run before serving them
    let mcp_router = Router::<AppState>::new().fallback(
        move |State(state): State<AppState>, mut req: axum::extract::Request| {
            let service = service.clone();
            async move {
                // a request whose token doesn't authenticate is left to each tool to refuse
                if let Some(scope) = run_scope(&state, req.headers()).await {
                    req.extensions_mut().insert(scope);
                }
                // serve the request with the MCP service
                match service.oneshot(req).await {
                    Ok(response) => response.into_response(),
                    Err(never) => match never {},
                }
            }
        },
    );
    // nest our router
    router.nest("/mcp", mcp_router)
}

#[cfg(test)]
mod tests {
    use super::{McpConfig, ThoriumMCP};
    use axum::http::request::Parts;
    use axum::http::{HeaderValue, Request};
    use serde_json::{Value, json};

    /// Every tool the server serves has the title and annotations of its kind of use
    #[test]
    fn live_tool_titles_and_annotations() {
        // the title, read-only, destructive, idempotent, and open world hints of each tool
        let expected: [(&str, &str, bool, bool, bool); 10] = [
            ("get_sample", "Get sample", true, false, true),
            (
                "get_sample_results",
                "Get sample results",
                true,
                false,
                true,
            ),
            (
                "list_sample_result_file_paths",
                "List result files",
                true,
                false,
                true,
            ),
            (
                "get_sample_result_file",
                "Get result file",
                true,
                false,
                true,
            ),
            ("list_images", "List images", true, false, true),
            ("list_pipelines", "List pipelines", true, false, true),
            ("start_tree", "Crawl related data", true, false, false),
            ("run_tool", "Run tool", false, false, false),
            ("get_tool_run", "Get tool run", true, false, true),
            (
                "get_tool_run_results",
                "Get tool run results",
                true,
                false,
                true,
            ),
        ];
        // list the tools the server serves
        let tools = ThoriumMCP::router().list_all();
        assert_eq!(tools.len(), expected.len());
        for (name, title, read_only, destructive, idempotent) in expected {
            // find the tool
            let tool = tools
                .iter()
                .find(|tool| tool.name == name)
                .unwrap_or_else(|| panic!("{name} isn't served"));
            // its title is set on the tool and its annotations
            assert_eq!(tool.title.as_deref(), Some(title), "{name}");
            let annotations = tool
                .annotations
                .as_ref()
                .unwrap_or_else(|| panic!("{name} has no annotations"));
            assert_eq!(annotations.title.as_deref(), Some(title), "{name}");
            // its hints follow its kind of use, and no tool reaches outside Thorium
            assert_eq!(annotations.read_only_hint, Some(read_only), "{name}");
            assert_eq!(annotations.destructive_hint, Some(destructive), "{name}");
            assert_eq!(annotations.idempotent_hint, Some(idempotent), "{name}");
            assert_eq!(annotations.open_world_hint, Some(false), "{name}");
        }
    }

    /// A JSON tool result carries the value as structured content and as one text block
    #[test]
    fn json_results_carry_matching_text() {
        // build a result from an object
        let value = json!({"sha256": "abc", "tags": {"Format": ["PE"]}});
        let result = super::json_result(&value).expect("value serializes");
        // the structured content is the value
        assert_eq!(result.structured_content.as_ref(), Some(&value));
        assert_eq!(result.is_error, Some(false));
        // one text block holds the same JSON
        assert_eq!(result.content.len(), 1);
        let text = result.content[0].as_text().expect("a text block");
        let parsed: Value = serde_json::from_str(&text.text).expect("the text is JSON");
        assert_eq!(parsed, value);
    }

    /// The workspace locks one rmcp release, the current one, without the SSE server transport
    #[test]
    fn single_current_rmcp() {
        // read the workspace lock file and the api manifest
        let api = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let lock = std::fs::read_to_string(api.join("../Cargo.lock")).expect("lock file reads");
        let manifest = std::fs::read_to_string(api.join("Cargo.toml")).expect("manifest reads");
        // collect the version of every locked rmcp package
        let lines = lock.lines().collect::<Vec<&str>>();
        let versions = lines
            .windows(2)
            .filter(|pair| pair[0] == "name = \"rmcp\"")
            .map(|pair| pair[1].to_owned())
            .collect::<Vec<String>>();
        // there is exactly one, on the 3.6 line or later
        assert_eq!(versions.len(), 1, "{versions:?}");
        let version = versions[0]
            .trim_start_matches("version = \"")
            .trim_end_matches('"');
        let mut parts = version
            .split('.')
            .map(|part| part.parse::<u64>().unwrap_or(0));
        let major = parts.next().unwrap_or(0);
        let minor = parts.next().unwrap_or(0);
        assert!((major, minor) >= (3, 6), "rmcp {version} is older than 3.6");
        // the api never enables the SSE server transport
        let rmcp_line = manifest
            .lines()
            .find(|line| line.starts_with("rmcp = "))
            .expect("the api depends on rmcp");
        assert!(!rmcp_line.contains("transport-sse-server"), "{rmcp_line}");
    }

    /// Build request parts carrying the given raw Authorization header bytes
    ///
    /// # Arguments
    ///
    /// * `value` - The raw bytes to set as the Authorization header value
    fn parts_with_auth(value: &[u8]) -> Parts {
        // craft a header value straight from bytes so we can inject non-UTF8 input
        let header = HeaderValue::from_bytes(value).expect("failed to build header value");
        // build a request carrying that header and tear off just its parts
        Request::builder()
            .header("Authorization", header)
            .body(())
            .expect("failed to build request")
            .into_parts()
            .0
    }

    #[test]
    fn grab_token_rejects_non_utf8_authorization() {
        // a non-UTF8 Authorization header is attacker controlled and must not crash the API
        let parts = parts_with_auth(&[0xFF, 0xFE]);
        // grabbing the token must return an error instead of panicking on `to_str().unwrap()`
        assert!(
            McpConfig::grab_token(&parts).is_err(),
            "grab_token accepted a non-UTF8 Authorization header instead of erroring"
        );
    }

    #[test]
    fn grab_token_extracts_bearer_token() {
        // a well formed bearer header must still yield the token after the space
        let parts = parts_with_auth(b"Bearer deadbeefcafe");
        let token = McpConfig::grab_token(&parts).expect("failed to grab a valid token");
        assert_eq!(token, "deadbeefcafe");
    }

    /// The MCP server's tool names are the tools its routers expose
    #[test]
    fn tool_names() {
        // list every tool the server exposes
        let mut names = ThoriumMCP::tool_names();
        names.sort();
        // the server exposes the sample, image, pipeline, tree, and tool run tools
        assert_eq!(
            names,
            vec![
                "get_sample",
                "get_sample_result_file",
                "get_sample_results",
                "get_tool_run",
                "get_tool_run_results",
                "list_images",
                "list_pipelines",
                "list_sample_result_file_paths",
                "run_tool",
                "start_tree",
            ]
        );
    }
}
