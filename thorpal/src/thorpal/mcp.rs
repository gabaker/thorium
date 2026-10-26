//! MCP (Model Context Protocol) servers a workflow can source tools from.
//!
//! A workflow's `mcp_servers` map (plus the built-in Thorium server toggled by
//! the workflow's `thorium_mcp` flag) is connected once at build time; each
//! referenced server's tool list is fetched and frozen, and the tools are
//! attached to the agents whose `tools` lists reference them. Tools are
//! advertised to the model as `<server>__<tool>` so same-named tools from
//! different servers can never collide with each other or with compiled in
//! tools, while the MCP wire call keeps each tool's own name.

use std::collections::{BTreeMap, BTreeSet};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use futures::future::BoxFuture;
use rmcp::ServiceExt;
use rmcp::model::{ClientCapabilities, ClientInfo, Implementation};
use rmcp::service::{RoleClient, RunningService, ServerSink};
use rmcp::transport::TokioChildProcess;
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};
use serde::{Deserialize, Serialize};
use tracing::instrument;

use super::agents::workflow::AgentConfig;

/// The prefix marking an MCP entry in a workflow's tool lists
pub const MCP_PREFIX: &str = "mcp:";

/// The reserved name of the built-in Thorium MCP server
///
/// A workflow may not configure a server under this name; the built-in server
/// is turned on with the workflow's top-level `thorium_mcp` flag instead and
/// referenced like any other server (`mcp:thorium`, `mcp:thorium:<tool>`).
pub const THORIUM_MCP_SERVER: &str = "thorium";

/// How long a server gets to connect and list its tools at build time
const MCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long each MCP connection gets to close cleanly at shutdown
const MCP_CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

/// The default per-tool-call timeout when a server sets no `timeout_secs`
///
/// Stated explicitly (matching rig's own MCP default) because every batch is
/// registered through the with-timeout rig surface, where passing `None`
/// would mean unbounded calls rather than the documented 300s default.
const DEFAULT_MCP_CALL_TIMEOUT: Duration = Duration::from_secs(300);

/// The max length of a model-visible tool name (Bedrock's tool name cap)
const MAX_TOOL_NAME_LEN: usize = 64;

/// One MCP server a workflow sources tools from
///
/// Exactly one transport must be set: `command` (a stdio child process) or
/// `url` (a streamable-HTTP endpoint). String fields may embed `${VAR}`
/// environment references, which are expanded at connect time so secrets
/// never reach the session snapshot on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// The command to spawn for a stdio server, or `None` for HTTP
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// The arguments passed to `command`
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Extra environment variables set on the spawned `command`
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// The streamable-HTTP endpoint url, or `None` for stdio
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Extra HTTP headers sent on every request (e.g. Authorization)
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// The per-tool-call timeout in seconds, or `None` for the 300s default
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// Whether a connect failure skips this server instead of failing the run
    #[serde(default)]
    pub optional: bool,
    /// Whether every tool on this server is treated as read-only
    ///
    /// When false (the default) a `read_only` agent keeps only the tools whose
    /// MCP `readOnlyHint` annotation is true.
    #[serde(default)]
    pub read_only: bool,
}

impl McpServerConfig {
    /// Make sure this server config is valid
    ///
    /// # Arguments
    ///
    /// * `name` - The server's configured name, for error context
    pub(crate) fn validate(&self, name: &str) -> Result<(), anyhow::Error> {
        // server names prefix every tool name the model sees, so they must be
        // lowercase snake case to stay provider-safe
        let mut chars = name.chars();
        let valid_start = chars.next().is_some_and(|c| c.is_ascii_lowercase());
        let valid_rest = chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        if !valid_start || !valid_rest {
            anyhow::bail!(
                "MCP server name '{name}' must start with a lowercase ascii letter and \
                contain only lowercase ascii letters, digits, and underscores"
            );
        }
        // a double underscore would make the server__tool prefix scheme ambiguous
        if name.contains("__") {
            anyhow::bail!("MCP server name '{name}' must not contain a double underscore");
        }
        // exactly one transport must be configured
        match (&self.command, &self.url) {
            (Some(_), Some(_)) => anyhow::bail!(
                "MCP server '{name}' sets both `command` and `url`; pick one transport"
            ),
            (None, None) => anyhow::bail!(
                "MCP server '{name}' sets neither `command` nor `url`; pick one transport"
            ),
            _ => (),
        }
        // reject fields that belong to the other transport as likely typos
        if self.command.is_none() && (!self.args.is_empty() || !self.env.is_empty()) {
            anyhow::bail!("MCP server '{name}' sets `args`/`env` but has no `command`");
        }
        if self.url.is_none() && !self.headers.is_empty() {
            anyhow::bail!("MCP server '{name}' sets `headers` but has no `url`");
        }
        // a zero timeout could never complete a call; omit it for the default
        if self.timeout_secs == Some(0) {
            anyhow::bail!(
                "MCP server '{name}' sets `timeout_secs: 0`; omit it for the default instead"
            );
        }
        Ok(())
    }

    /// The per-call timeout this server's tools run under
    fn timeout(&self) -> Duration {
        // the yaml states seconds; an omitted value gets the documented default
        self.timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_MCP_CALL_TIMEOUT)
    }
}

/// The credentials the built-in Thorium MCP server connects with
///
/// Resolved at connect time and held only in memory: the token is never
/// persisted into the session store, so a resume must re-resolve it.
#[derive(Clone)]
pub struct ThoriumMcpAuth {
    /// The base url of the Thorium API (the MCP endpoint lives at `/api/mcp`)
    pub url: String,
    /// The token sent in the `Authorization` header
    pub token: String,
    /// Whether to accept invalid TLS certificates, mirroring the consumer's
    /// own client settings for self-signed Thorium instances
    pub accept_invalid_certs: bool,
}

/// A callback resolving credentials for the built-in Thorium MCP server
///
/// Invoked lazily, only when a built workflow actually references the
/// built-in server, so runs that never touch it stay fully offline.
pub type ThoriumMcpResolver =
    Arc<dyn Fn() -> BoxFuture<'static, Result<ThoriumMcpAuth, anyhow::Error>> + Send + Sync>;

/// A parsed `mcp:` reference from an agent's tool lists
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpToolRef {
    /// The configured server name the reference targets
    pub server: String,
    /// The single tool referenced, or `None` for every tool the server lists
    pub tool: Option<String>,
}

/// Parse an `mcp:` tool-list entry into its server and tool parts
///
/// # Arguments
///
/// * `entry` - The raw tools/disabled_tools entry (must start with [`MCP_PREFIX`])
pub(crate) fn parse_ref(entry: &str) -> Result<McpToolRef, anyhow::Error> {
    // strip the prefix the caller matched on
    let rest = entry
        .strip_prefix(MCP_PREFIX)
        .with_context(|| format!("'{entry}' is not an mcp: reference"))?;
    // split the remainder into the server and optional tool parts
    let (server, tool) = match rest.split_once(':') {
        Some((server, tool)) => (server, Some(tool)),
        None => (rest, None),
    };
    // every part present must be non-empty, and a tool may not nest further
    if server.is_empty() || tool.is_some_and(|tool| tool.is_empty() || tool.contains(':')) {
        anyhow::bail!(
            "'{entry}' is not a valid MCP reference; use mcp:<server> or mcp:<server>:<tool>"
        );
    }
    Ok(McpToolRef {
        server: server.to_owned(),
        tool: tool.map(ToOwned::to_owned),
    })
}

/// Build the model-visible name for an MCP tool
///
/// # Arguments
///
/// * `server` - The configured server name the tool comes from
/// * `tool` - The tool's own name as the server lists it
pub(crate) fn prefixed_name(server: &str, tool: &str) -> String {
    format!("{server}__{tool}")
}

/// Expand `${VAR}` environment references in a config string
///
/// # Arguments
///
/// * `input` - The raw config string to expand
/// * `context` - What the string configures, for error context
fn interpolate_env(input: &str, context: &str) -> Result<String, anyhow::Error> {
    // delegate to the injectable scanner with the live process environment
    interpolate_with(input, context, |var| std::env::var(var).ok())
}

/// Expands `${VAR}` references while remembering every expanded secret
///
/// Transport errors can echo the strings they were handed (e.g. an HTTP
/// client error repeats the request url), so anything built from expanded
/// config must be scrubbed before it reaches an error chain that tracing
/// persists to the on-disk log. The expander remembers each expansion so
/// [`EnvExpander::sanitize`] can swap the values back to their `${VAR}` form.
#[derive(Default)]
struct EnvExpander {
    /// The (expanded value, `${VAR}` placeholder) pairs seen so far
    replacements: Vec<(String, String)>,
}

impl EnvExpander {
    /// Expand `${VAR}` references, recording each expansion for sanitizing
    ///
    /// # Arguments
    ///
    /// * `input` - The raw config string to expand
    /// * `context` - What the string configures, for error context
    fn expand(&mut self, input: &str, context: &str) -> Result<String, anyhow::Error> {
        // expand from the live environment, remembering what each var produced
        interpolate_with(input, context, |var| match std::env::var(var) {
            Ok(value) => {
                // tiny values are skipped: scrubbing them would mangle
                // unrelated text, and they cannot be meaningful secrets
                if value.len() >= 4 {
                    self.replacements
                        .push((value.clone(), format!("${{{var}}}")));
                }
                Some(value)
            }
            Err(_) => None,
        })
    }

    /// Swap every remembered expansion in a message back to its `${VAR}` form
    ///
    /// # Arguments
    ///
    /// * `text` - The error text to scrub before it can be logged
    fn sanitize(&self, text: String) -> String {
        // replace longer values first so overlapping secrets scrub fully
        let mut ordered: Vec<&(String, String)> = self.replacements.iter().collect();
        ordered.sort_by_key(|(value, _)| std::cmp::Reverse(value.len()));
        // scrub each expanded value out of the message
        let mut scrubbed = text;
        for (value, placeholder) in ordered {
            scrubbed = scrubbed.replace(value, placeholder);
        }
        scrubbed
    }
}

/// Expand `${VAR}` references in a config string with an injected lookup
///
/// # Arguments
///
/// * `input` - The raw config string to expand
/// * `context` - What the string configures, for error context
/// * `lookup` - Resolves a variable name to its value if it is set
fn interpolate_with(
    input: &str,
    context: &str,
    mut lookup: impl FnMut(&str) -> Option<String>,
) -> Result<String, anyhow::Error> {
    // build the expanded string as we scan
    let mut expanded = String::with_capacity(input.len());
    // the unscanned remainder of the input
    let mut rest = input;
    // expand each ${VAR} reference in order
    while let Some(start) = rest.find("${") {
        // keep everything before the reference verbatim
        expanded.push_str(&rest[..start]);
        // the reference body starts after the `${`
        let body = &rest[start + 2..];
        // an unterminated reference is a typo worth rejecting
        let Some(end) = body.find('}') else {
            anyhow::bail!("unterminated ${{...}} reference in {context}");
        };
        // pull out the referenced variable name
        let var = &body[..end];
        if var.is_empty() {
            anyhow::bail!("empty ${{}} reference in {context}");
        }
        // a missing variable means the operator's environment is incomplete
        match lookup(var) {
            Some(value) => expanded.push_str(&value),
            None => {
                anyhow::bail!("environment variable '{var}' referenced by {context} is not set")
            }
        }
        // continue scanning after the closing brace
        rest = &body[end + 1..];
    }
    // keep whatever trails the last reference
    expanded.push_str(rest);
    Ok(expanded)
}

/// The MCP tools one agent attaches from one server
pub struct McpAgentTools {
    /// The model-visible name and definition of each tool, sorted by name
    pub tools: Vec<(String, rmcp::model::Tool)>,
    /// The sink the tools' calls are dispatched through
    pub sink: ServerSink,
    /// The per-call timeout every tool in this batch runs under
    pub timeout: Duration,
}

impl std::fmt::Debug for McpAgentTools {
    /// Summarize the batch by tool names instead of dumping schemas
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpAgentTools")
            .field(
                "tools",
                &self.tools.iter().map(|(name, _)| name).collect::<Vec<_>>(),
            )
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

/// One connected MCP server with its frozen tool list
struct McpConnection {
    /// The per-call timeout this server's tools run under
    timeout: Duration,
    /// Whether every tool on this server counts as read-only
    read_only: bool,
    /// The sink tool calls are dispatched through
    sink: ServerSink,
    /// The tools the server listed at connect time, sorted by name
    tools: Vec<rmcp::model::Tool>,
}

/// Every MCP connection one run holds, keyed by server name
#[derive(Default)]
pub(crate) struct McpConnections {
    /// The connected servers' sinks and frozen tool lists
    servers: BTreeMap<String, McpConnection>,
    /// The running client services kept alive for the session
    services: Vec<RunningService<RoleClient, ClientInfo>>,
}

impl std::fmt::Debug for McpConnections {
    /// Summarize the connection set by server name and tool count
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpConnections")
            .field(
                "servers",
                &self
                    .servers
                    .iter()
                    .map(|(name, connection)| (name, connection.tools.len()))
                    .collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

impl McpConnections {
    /// Connect every MCP server the workflow's agents actually reference
    ///
    /// Unreferenced servers are never connected. A server that fails to
    /// connect, list its tools, or expand its `${VAR}` references within the
    /// deadline fails the build unless it is `optional`, in which case it is
    /// skipped with a warning and its tools silently resolve to nothing. The
    /// built-in Thorium server is connected through the resolver and is never
    /// optional.
    ///
    /// # Arguments
    ///
    /// * `thorium_mcp` - Whether the built-in Thorium MCP server is enabled
    /// * `configs` - The workflow's configured servers
    /// * `agents` - The agents whose tool lists reference servers
    /// * `thorium` - The resolver minting the built-in server's credentials
    #[instrument(name = "McpConnections::connect", skip_all, err(Debug))]
    pub(crate) async fn connect(
        thorium_mcp: bool,
        configs: &BTreeMap<String, McpServerConfig>,
        agents: &[AgentConfig],
        thorium: Option<&ThoriumMcpResolver>,
    ) -> Result<Self, anyhow::Error> {
        // start with no connections at all
        let mut connections = McpConnections::default();
        // collect every server name the agents reference; disables count too
        // so their tool names can be validated against a live list
        let mut referenced = BTreeSet::new();
        for config in agents {
            // parse this agent's references, rejecting malformed entries
            let (enables, disables) = config.mcp_refs()?;
            referenced.extend(enables.into_iter().map(|r| r.server));
            referenced.extend(disables.into_iter().map(|r| r.server));
        }
        // connect each referenced server in a stable order
        for name in referenced {
            // resolve the connection recipe for this server
            let (config, auth) = if name == THORIUM_MCP_SERVER {
                // the built-in server must be turned on to be referenced; this
                // backstops builder paths that never ran Workflow::validate
                if !thorium_mcp {
                    anyhow::bail!(
                        "a workflow agent references the built-in Thorium MCP server \
                        'thorium' but `thorium_mcp` is not enabled"
                    );
                }
                // without a resolver there is no way to mint credentials
                let Some(resolver) = thorium else {
                    anyhow::bail!(
                        "the built-in Thorium MCP server is referenced but this consumer \
                        installed no Thorium credentials resolver"
                    );
                };
                (None, Some(Arc::clone(resolver)))
            } else {
                // every other server must be configured in the workflow; this
                // also backstops builder paths that never ran Workflow::validate
                match configs.get(&name) {
                    Some(config) => (Some(config), None),
                    None => anyhow::bail!(
                        "a workflow agent references unconfigured MCP server '{name}'"
                    ),
                }
            };
            // whether a failure skips this server instead of failing the build
            let optional = config.is_some_and(|config| config.optional);
            // bound the whole per-server setup (credential resolution,
            // connect, and tool listing) by one deadline
            let setup = tokio::time::timeout(
                MCP_CONNECT_TIMEOUT,
                Self::connect_server(&name, config, auth),
            )
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "MCP server '{name}' did not connect and list its tools within {}s",
                    MCP_CONNECT_TIMEOUT.as_secs()
                )
            })
            .and_then(|result| result);
            // keep the connection, or apply this server's failure policy
            match setup {
                Ok((service, tools)) => {
                    // log what the server offers so stalls are diagnosable
                    tracing::info!(
                        server = %name,
                        tools = tools.len(),
                        "connected to MCP server"
                    );
                    // record the frozen tool list and dispatch sink; the
                    // built-in server (no config) runs under the default cap
                    connections.servers.insert(
                        name,
                        McpConnection {
                            timeout: config
                                .map(McpServerConfig::timeout)
                                .unwrap_or(DEFAULT_MCP_CALL_TIMEOUT),
                            read_only: config.is_some_and(|config| config.read_only),
                            sink: service.peer().clone(),
                            tools,
                        },
                    );
                    // keep the service alive for the whole session
                    connections.services.push(service);
                }
                Err(err) if optional => {
                    // an optional server degrades to no tools with a warning
                    tracing::warn!(
                        server = %name,
                        ?err,
                        "skipping optional MCP server that failed to connect"
                    );
                }
                Err(err) => {
                    // close whatever did connect before failing the build
                    connections.shutdown().await;
                    return Err(err.context(format!(
                        "MCP server '{name}' failed; mark it `optional: true` to tolerate this"
                    )));
                }
            }
        }
        Ok(connections)
    }

    /// Connect one MCP server and fetch its tool list
    ///
    /// # Arguments
    ///
    /// * `name` - The server's configured name, for error context
    /// * `config` - The server's workflow config, or `None` for the built-in
    /// * `auth` - The built-in server's credential resolver when `config` is `None`
    async fn connect_server(
        name: &str,
        config: Option<&McpServerConfig>,
        auth: Option<ThoriumMcpResolver>,
    ) -> Result<
        (
            RunningService<RoleClient, ClientInfo>,
            Vec<rmcp::model::Tool>,
        ),
        anyhow::Error,
    > {
        // connect over the transport this server's recipe describes
        let service = match (config, auth) {
            // the built-in Thorium server: resolve credentials, then talk
            // streamable HTTP to the API's /api/mcp endpoint
            (None, Some(resolver)) => {
                // mint the url and token this run reaches Thorium with
                let auth = (resolver)()
                    .await
                    .context("failed to resolve Thorium MCP credentials")?;
                // the MCP route is nested under the API's /api prefix
                let url = format!("{}/api/mcp", auth.url.trim_end_matches('/'));
                // Thorium's auth convention is `Authorization: token <TOKEN>`
                let headers =
                    BTreeMap::from([("Authorization".to_owned(), format!("token {}", auth.token))]);
                Self::connect_http(name, &url, &headers, auth.accept_invalid_certs).await?
            }
            // a stdio server: spawn the command and talk over its pipes
            (Some(config), _) if config.command.is_some() => {
                Self::connect_stdio(name, config).await?
            }
            // an HTTP server: expand the url and headers, then connect
            (Some(config), _) => {
                // validate() guarantees a url when there is no command
                let raw = config.url.as_deref().unwrap_or_default();
                Self::connect_http(name, raw, &config.headers, false).await?
            }
            // connect() always supplies exactly one of config/credentials
            (None, None) => {
                anyhow::bail!("MCP server '{name}' has neither a config nor built-in credentials")
            }
        };
        // fetch the server's full tool list (paginated under the hood)
        let mut tools = service
            .peer()
            .list_all_tools()
            .await
            .with_context(|| format!("failed to list MCP server '{name}' tools"))?;
        // freeze the list in a stable order so the toolset is deterministic
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        Ok((service, tools))
    }

    /// Spawn and connect a stdio MCP server
    ///
    /// # Arguments
    ///
    /// * `name` - The server's configured name, for error context
    /// * `config` - The server's workflow config (must set `command`)
    async fn connect_stdio(
        name: &str,
        config: &McpServerConfig,
    ) -> Result<RunningService<RoleClient, ClientInfo>, anyhow::Error> {
        // expand the command and each argument and env value
        let raw = config.command.as_deref().unwrap_or_default();
        let command = interpolate_env(raw, &format!("MCP server '{name}' command"))?;
        let mut cmd = tokio::process::Command::new(command);
        for arg in &config.args {
            cmd.arg(interpolate_env(arg, &format!("MCP server '{name}' args"))?);
        }
        for (key, value) in &config.env {
            cmd.env(
                key,
                interpolate_env(value, &format!("MCP server '{name}' env '{key}'"))?,
            );
        }
        // spawn with stderr dropped: the default inherits our stderr, which
        // would let the child scribble over the chat TUI
        let (process, _stderr) = TokioChildProcess::builder(cmd)
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("failed to spawn MCP server '{name}'"))?;
        // run the MCP handshake over the child's pipes
        let service = Self::client_info()
            .serve(process)
            .await
            .with_context(|| format!("failed to connect to MCP server '{name}'"))?;
        Ok(service)
    }

    /// Connect a streamable-HTTP MCP server
    ///
    /// Connect errors are scrubbed of every `${VAR}` expansion before they
    /// can propagate: transport errors echo the url they were handed, and the
    /// url is a documented secret carrier, so the raw error must never reach
    /// the on-disk log.
    ///
    /// # Arguments
    ///
    /// * `name` - The server's configured name, for error context
    /// * `url` - The raw endpoint url (may embed `${VAR}` references)
    /// * `headers` - The raw header map (values may embed `${VAR}` references)
    /// * `accept_invalid_certs` - Whether to accept invalid TLS certificates
    async fn connect_http(
        name: &str,
        url: &str,
        headers: &BTreeMap<String, String>,
        accept_invalid_certs: bool,
    ) -> Result<RunningService<RoleClient, ClientInfo>, anyhow::Error> {
        // expand the url and headers through one expander so the error
        // scrubbing below knows every secret this connection was built from
        let mut expander = EnvExpander::default();
        let url = expander.expand(url, &format!("MCP server '{name}' url"))?;
        // expand and parse each configured header
        let mut custom = std::collections::HashMap::with_capacity(headers.len());
        for (key, value) in headers {
            // header names come from yaml keys; values may hold secrets
            let header = http::HeaderName::try_from(key.as_str())
                .with_context(|| format!("MCP server '{name}' header '{key}' is invalid"))?;
            let value = expander.expand(value, &format!("MCP server '{name}' header '{key}'"))?;
            let mut value = http::HeaderValue::from_str(&value).map_err(|_| {
                anyhow::anyhow!("MCP server '{name}' header '{key}' has an invalid value")
            })?;
            // keep header values (often credentials) out of debug output
            value.set_sensitive(true);
            custom.insert(header, value);
        }
        // build the streamable-HTTP transport; a cert-tolerant connection gets
        // its own client mirroring rmcp's bundled one (no idle pooling, no
        // redirects so custom headers are never replayed to another host)
        let config = StreamableHttpClientTransportConfig::with_uri(url).custom_headers(custom);
        let transport = if accept_invalid_certs {
            let client = reqwest::Client::builder()
                .pool_max_idle_per_host(0)
                .redirect(reqwest::redirect::Policy::none())
                .danger_accept_invalid_certs(true)
                .build()
                .context("failed to build a cert-tolerant http client")?;
            StreamableHttpClientTransport::with_client(client, config)
        } else {
            StreamableHttpClientTransport::from_config(config)
        };
        // run the MCP handshake over the transport, scrubbing the expanded
        // secrets back out of any failure before it can be logged
        let service = Self::client_info().serve(transport).await.map_err(|err| {
            let scrubbed = expander.sanitize(format!("{:#}", anyhow::Error::new(err)));
            anyhow::anyhow!("failed to connect to MCP server '{name}': {scrubbed}")
        })?;
        Ok(service)
    }

    /// The client identity thorpal introduces itself to MCP servers with
    fn client_info() -> ClientInfo {
        ClientInfo::new(
            ClientCapabilities::default(),
            Implementation::new("thorpal", env!("CARGO_PKG_VERSION")),
        )
    }

    /// Resolve one agent's MCP toolset, validating names as it goes
    ///
    /// Applies the agent's `mcp:` enables and disables against the frozen
    /// per-server tool lists, strips non-read-only tools from a read-only
    /// agent, enforces the provider name charset and length on the prefixed
    /// names, and rejects any collision with the agent's compiled in tools.
    /// Servers skipped as `optional` resolve to no tools.
    ///
    /// # Arguments
    ///
    /// * `config` - The agent whose MCP references to resolve
    pub(crate) fn agent_tools(
        &self,
        config: &AgentConfig,
    ) -> Result<Vec<McpAgentTools>, anyhow::Error> {
        // parse this agent's references again (cheap, already validated)
        let (enables, disables) = config.mcp_refs()?;
        // collapse the enables per server: `None` enables the whole list
        let mut wanted: BTreeMap<&str, Option<BTreeSet<&str>>> = BTreeMap::new();
        for reference in &enables {
            match &reference.tool {
                // a whole-server enable supersedes any single-tool enables
                None => {
                    wanted.insert(&reference.server, None);
                }
                // collect single-tool enables unless the whole server is on
                Some(tool) => match wanted.entry(&reference.server) {
                    std::collections::btree_map::Entry::Occupied(mut entry) => {
                        if let Some(set) = entry.get_mut() {
                            set.insert(tool);
                        }
                    }
                    std::collections::btree_map::Entry::Vacant(entry) => {
                        entry.insert(Some(BTreeSet::from([tool.as_str()])));
                    }
                },
            }
        }
        // group the disables per server too
        let mut dropped: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for reference in &disables {
            // mcp_refs guarantees disables carry a tool part
            if let Some(tool) = &reference.tool {
                dropped.entry(&reference.server).or_default().insert(tool);
            }
        }
        // a disable on a server this agent never enables would silently no-op
        // forever; reject it as the config debt it is (this backstops builder
        // paths that never ran Workflow::validate)
        for server in dropped.keys() {
            if !wanted.contains_key(server) {
                anyhow::bail!(
                    "agent '{}' disables MCP tools on server '{server}' but never enables it",
                    config.name
                );
            }
        }
        // the compiled in names this agent resolves, for collision defense
        let registry: BTreeSet<&str> = config
            .resolve_tools()?
            .iter()
            .map(|entry| entry.name)
            .collect();
        // every model-visible name this agent attaches, across all servers
        let mut taken = BTreeSet::new();
        // build one batch per enabled server
        let mut batches = Vec::with_capacity(wanted.len());
        for (server, subset) in wanted {
            // a server skipped as optional resolves to no tools at all
            let Some(connection) = self.servers.get(server) else {
                tracing::debug!(server, "agent references a skipped MCP server");
                continue;
            };
            // index the frozen list by raw tool name
            let listed: BTreeMap<&str, &rmcp::model::Tool> = connection
                .tools
                .iter()
                .map(|tool| (tool.name.as_ref(), tool))
                .collect();
            // a single-tool enable must name a tool the server actually lists
            if let Some(subset) = &subset {
                for tool in subset {
                    if !listed.contains_key(tool) {
                        anyhow::bail!(
                            "agent '{}' enables unknown MCP tool '{tool}' on server '{server}'",
                            config.name
                        );
                    }
                }
            }
            // disables must name listed tools too, so typos never pass silently
            let dropped = dropped.get(server).cloned().unwrap_or_default();
            for tool in &dropped {
                if !listed.contains_key(tool) {
                    anyhow::bail!(
                        "agent '{}' disables unknown MCP tool '{tool}' on server '{server}'",
                        config.name
                    );
                }
            }
            // walk the frozen list applying the enables, disables, and the
            // read-only policy in order
            let mut tools = Vec::new();
            for tool in &connection.tools {
                let raw = tool.name.as_ref();
                // keep only the enabled subset when one was given
                if subset.as_ref().is_some_and(|set| !set.contains(raw)) {
                    continue;
                }
                // drop explicitly disabled tools
                if dropped.contains(raw) {
                    continue;
                }
                // a read-only agent keeps a tool only when the server is
                // declared read-only or the tool's own annotation says so
                if config.read_only && !connection.read_only && !read_only_hint(tool) {
                    continue;
                }
                // the prefixed name is what the provider sees, so it must
                // satisfy the strictest provider's charset and length rules
                let name = prefixed_name(server, raw);
                let valid = raw
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
                if !valid || name.len() > MAX_TOOL_NAME_LEN {
                    anyhow::bail!(
                        "MCP tool '{raw}' on server '{server}' maps to the model-visible \
                        name '{name}', which exceeds {MAX_TOOL_NAME_LEN} chars or uses \
                        chars outside [a-zA-Z0-9_-]"
                    );
                }
                // defend against collisions the prefix scheme should prevent
                if registry.contains(name.as_str()) || !taken.insert(name.clone()) {
                    anyhow::bail!(
                        "agent '{}' MCP tool '{name}' from server '{server}' collides \
                        with an existing tool name",
                        config.name
                    );
                }
                tools.push((name, tool.clone()));
            }
            // skip servers everything was filtered away from
            if !tools.is_empty() {
                batches.push(McpAgentTools {
                    tools,
                    sink: connection.sink.clone(),
                    timeout: connection.timeout,
                });
            }
        }
        Ok(batches)
    }

    /// Cleanly close every held connection, bounding each close
    pub(crate) async fn shutdown(&mut self) {
        // drop the sinks first so nothing dispatches into closing services
        self.servers.clear();
        // close each running service, bounding how long a close may hang
        for mut service in self.services.drain(..) {
            if let Err(err) = service.close_with_timeout(MCP_CLOSE_TIMEOUT).await {
                tracing::warn!(?err, "an MCP connection failed to close cleanly");
            }
        }
    }
}

/// Whether an MCP tool's annotations declare it read-only
///
/// # Arguments
///
/// * `tool` - The tool definition to inspect
fn read_only_hint(tool: &rmcp::model::Tool) -> bool {
    // absent annotations conservatively count as mutating
    tool.annotations
        .as_ref()
        .and_then(|annotations| annotations.read_only_hint)
        == Some(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{
        CallToolRequestParams, CallToolResult, ContentBlock, ErrorData, ListToolsResult,
        PaginatedRequestParams, ProtocolVersion, ServerCapabilities, ServerInfo,
    };
    use rmcp::service::RequestContext;
    use rmcp::{RoleServer, ServerHandler};

    /// Build a minimal agent config with the given tool lists
    ///
    /// # Arguments
    ///
    /// * `tools` - The tools and tool groups this agent enables
    /// * `disabled` - The tools this agent disables
    /// * `read_only` - Whether this agent drops every mutating tool
    fn agent(tools: Vec<&str>, disabled: Vec<&str>, read_only: bool) -> AgentConfig {
        AgentConfig {
            name: "Tester".to_owned(),
            description: "desc".to_owned(),
            preamble: "preamble".to_owned(),
            tools: tools.into_iter().map(String::from).collect(),
            disabled_tools: disabled.into_iter().map(String::from).collect(),
            read_only,
            max_turns: 1,
            max_extension_turns: None,
            warn_turns: None,
            max_swarm_size: 1,
            agents: Vec::new(),
            max_nested_alive: 6,
        }
    }

    /// Build an MCP tool definition with an optional read-only hint
    ///
    /// # Arguments
    ///
    /// * `name` - The tool's name as the server lists it
    /// * `read_only` - The readOnlyHint annotation to set, if any
    fn tool(name: &str, read_only: Option<bool>) -> rmcp::model::Tool {
        // start from a bare definition with an empty schema
        let mut tool = rmcp::model::Tool::new(
            name.to_owned(),
            "a fixture tool".to_owned(),
            Arc::new(serde_json::Map::new()),
        );
        // annotate the read-only hint when the case calls for one
        if let Some(hint) = read_only {
            let mut annotations = rmcp::model::ToolAnnotations::new();
            annotations.read_only_hint = Some(hint);
            tool.annotations = Some(annotations);
        }
        tool
    }

    /// A fixture MCP server listing a fixed toolset over duplex pipes
    #[derive(Clone)]
    struct FixtureServer {
        /// The tools this server lists
        tools: Vec<rmcp::model::Tool>,
    }

    impl ServerHandler for FixtureServer {
        /// Advertise the tools capability like a real server
        fn get_info(&self) -> ServerInfo {
            ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
                .with_protocol_version(ProtocolVersion::LATEST)
                .with_server_info(Implementation::new("fixture", "0.0.1"))
        }

        /// List the fixed toolset in one page
        async fn list_tools(
            &self,
            _request: Option<PaginatedRequestParams>,
            _context: RequestContext<RoleServer>,
        ) -> Result<ListToolsResult, ErrorData> {
            Ok(ListToolsResult::with_all_items(self.tools.clone()))
        }

        /// Echo the called tool name so dispatch can be asserted on
        async fn call_tool(
            &self,
            request: CallToolRequestParams,
            _context: RequestContext<RoleServer>,
        ) -> Result<CallToolResult, ErrorData> {
            Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                "called {}",
                request.name
            ))]))
        }
    }

    /// Connect a fixture server and wrap it as a one-server connection set
    ///
    /// # Arguments
    ///
    /// * `name` - The server name to register the connection under
    /// * `tools` - The tools the fixture server lists
    /// * `read_only` - Whether the whole server counts as read-only
    async fn fixture(
        name: &str,
        tools: Vec<rmcp::model::Tool>,
        read_only: bool,
    ) -> (McpConnections, tokio::task::JoinHandle<()>) {
        // wire the client and server together over in-process pipes
        let (c2s, sfc) = tokio::io::duplex(8192);
        let (s2c, cfs) = tokio::io::duplex(8192);
        let server = FixtureServer { tools };
        // run the server until its transport closes
        let task = tokio::spawn(async move {
            let running = server.serve((sfc, s2c)).await.expect("server start");
            let _ = running.waiting().await;
        });
        // connect exactly like McpConnections::connect_server does
        let service = McpConnections::client_info()
            .serve((cfs, c2s))
            .await
            .expect("client connect");
        let mut listed = service.peer().list_all_tools().await.expect("list tools");
        listed.sort_by(|a, b| a.name.cmp(&b.name));
        // assemble the one-server connection set by hand
        let mut connections = McpConnections::default();
        connections.servers.insert(
            name.to_owned(),
            McpConnection {
                timeout: Duration::from_secs(1),
                read_only,
                sink: service.peer().clone(),
                tools: listed,
            },
        );
        connections.services.push(service);
        (connections, task)
    }

    /// References parse into their server and tool parts, rejecting typos.
    #[test]
    fn refs_parse_and_reject_typos() {
        // a bare server reference enables the whole server
        let parsed = parse_ref("mcp:intel").expect("server ref");
        assert_eq!(parsed.server, "intel");
        assert_eq!(parsed.tool, None);
        // a three-part reference names a single tool
        let parsed = parse_ref("mcp:intel:lookup_hash").expect("tool ref");
        assert_eq!(parsed.server, "intel");
        assert_eq!(parsed.tool.as_deref(), Some("lookup_hash"));
        // malformed references are rejected
        for bad in ["mcp:", "mcp::x", "mcp:a:", "mcp:a:b:c"] {
            assert!(parse_ref(bad).is_err(), "'{bad}' should be rejected");
        }
    }

    /// An omitted timeout_secs yields the documented 300s cap, never unbounded.
    #[test]
    fn omitted_timeout_defaults_to_the_cap() {
        // a config without a timeout gets the documented default
        let mut server = McpServerConfig {
            command: None,
            args: Vec::new(),
            env: BTreeMap::new(),
            url: Some("https://x.example/mcp".to_owned()),
            headers: BTreeMap::new(),
            timeout_secs: None,
            optional: false,
            read_only: false,
        };
        assert_eq!(server.timeout(), DEFAULT_MCP_CALL_TIMEOUT);
        assert_eq!(DEFAULT_MCP_CALL_TIMEOUT, Duration::from_secs(300));
        // an explicit timeout wins
        server.timeout_secs = Some(120);
        assert_eq!(server.timeout(), Duration::from_secs(120));
    }

    /// The expander scrubs every expanded secret back out of error text.
    #[test]
    fn expander_scrubs_expanded_secrets() {
        // expand a url embedding two secrets through the real environment;
        // the var names are unique to this test so parallel tests never race
        unsafe {
            std::env::set_var("THORPAL_MCP_TEST_KEY", "s3cretkey");
            std::env::set_var("THORPAL_MCP_TEST_HOST", "hidden.example");
        }
        let mut expander = EnvExpander::default();
        let url = expander
            .expand(
                "https://${THORPAL_MCP_TEST_HOST}/${THORPAL_MCP_TEST_KEY}/mcp",
                "test url",
            )
            .expect("expand");
        assert_eq!(url, "https://hidden.example/s3cretkey/mcp");
        // a transport-style error echoing the url comes back scrubbed
        let scrubbed = expander.sanitize(format!("error sending request for url ({url})"));
        assert!(!scrubbed.contains("s3cretkey"));
        assert!(!scrubbed.contains("hidden.example"));
        assert!(scrubbed.contains("${THORPAL_MCP_TEST_KEY}"));
        assert!(scrubbed.contains("${THORPAL_MCP_TEST_HOST}"));
    }

    /// Every config field that documents `${VAR}` support expands on the real
    /// connect path (regression pin: an unset var fails naming its call site).
    #[tokio::test]
    async fn connect_interpolates_every_documented_field() {
        /// Connect one server config and return the failure text
        ///
        /// # Arguments
        ///
        /// * `server` - The server config to connect
        async fn connect_err(server: McpServerConfig) -> String {
            let configs = BTreeMap::from([("srv".to_owned(), server)]);
            let agents = vec![agent(vec!["mcp:srv"], vec![], false)];
            format!(
                "{:#}",
                McpConnections::connect(false, &configs, &agents, None)
                    .await
                    .unwrap_err()
            )
        }
        /// A base config every case mutates
        fn base() -> McpServerConfig {
            McpServerConfig {
                command: None,
                args: Vec::new(),
                env: BTreeMap::new(),
                url: None,
                headers: BTreeMap::new(),
                timeout_secs: None,
                optional: false,
                read_only: false,
            }
        }
        // the url call site
        let mut server = base();
        server.url = Some("https://x/${THORPAL_MCP_UNSET}".to_owned());
        let error = connect_err(server).await;
        assert!(error.contains("THORPAL_MCP_UNSET") && error.contains("url"));
        // the header-value call site
        let mut server = base();
        server.url = Some("https://x/mcp".to_owned());
        server.headers = BTreeMap::from([(
            "Authorization".to_owned(),
            "Bearer ${THORPAL_MCP_UNSET}".to_owned(),
        )]);
        let error = connect_err(server).await;
        assert!(error.contains("THORPAL_MCP_UNSET") && error.contains("header"));
        // the command call site
        let mut server = base();
        server.command = Some("${THORPAL_MCP_UNSET}/mcp".to_owned());
        let error = connect_err(server).await;
        assert!(error.contains("THORPAL_MCP_UNSET") && error.contains("command"));
        // the args call site
        let mut server = base();
        server.command = Some("/nonexistent/mcp".to_owned());
        server.args = vec!["${THORPAL_MCP_UNSET}".to_owned()];
        let error = connect_err(server).await;
        assert!(error.contains("THORPAL_MCP_UNSET") && error.contains("args"));
        // the env-value call site
        let mut server = base();
        server.command = Some("/nonexistent/mcp".to_owned());
        server.env = BTreeMap::from([("KEY".to_owned(), "${THORPAL_MCP_UNSET}".to_owned())]);
        let error = connect_err(server).await;
        assert!(error.contains("THORPAL_MCP_UNSET") && error.contains("env 'KEY'"));
    }

    /// A disable on a never-enabled server is rejected, not silently ignored.
    #[tokio::test]
    async fn disables_require_a_matching_enable() {
        // a connection set with one live server
        let (connections, task) = fixture("srv", vec![tool("alpha", None)], false).await;
        // the agent disables on a server it never enables
        let error = connections
            .agent_tools(&agent(vec!["mcp:srv"], vec!["mcp:other:alpha"], false))
            .unwrap_err()
            .to_string();
        assert!(error.contains("never enables"));
        task.abort();
    }

    /// `${VAR}` references expand from the lookup and reject bad shapes.
    #[test]
    fn env_references_expand_and_reject() {
        // a simple lookup with two variables set
        let lookup = |var: &str| match var {
            "TOKEN" => Some("s3cret".to_owned()),
            "HOST" => Some("x.example".to_owned()),
            _ => None,
        };
        // plain strings pass through untouched
        let plain = interpolate_with("no refs here", "test", lookup).expect("plain");
        assert_eq!(plain, "no refs here");
        // single and multiple references expand in place
        let one = interpolate_with("Bearer ${TOKEN}", "test", lookup).expect("one");
        assert_eq!(one, "Bearer s3cret");
        let two = interpolate_with("https://${HOST}/${TOKEN}", "test", lookup).expect("two");
        assert_eq!(two, "https://x.example/s3cret");
        // a missing variable names itself and the config it came from
        let missing = interpolate_with("${NOPE}", "server 'x' url", lookup).unwrap_err();
        assert!(missing.to_string().contains("'NOPE'"));
        assert!(missing.to_string().contains("server 'x' url"));
        // unterminated and empty references are typos worth rejecting
        assert!(interpolate_with("${TOKEN", "test", lookup).is_err());
        assert!(interpolate_with("${}", "test", lookup).is_err());
    }

    /// Enables, disables, and the read-only policy filter the frozen list.
    #[tokio::test]
    async fn agent_tools_filter_and_prefix() {
        // a server listing one read-only, one unannotated, and one mutating tool
        let tools = vec![
            tool("alpha", Some(true)),
            tool("beta", None),
            tool("gamma", Some(false)),
        ];
        let (connections, task) = fixture("srv", tools, false).await;
        // a whole-server enable attaches everything, prefixed and sorted
        let batches = connections
            .agent_tools(&agent(vec!["mcp:srv"], vec![], false))
            .expect("whole server");
        assert_eq!(batches.len(), 1);
        let names: Vec<&str> = batches[0].tools.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["srv__alpha", "srv__beta", "srv__gamma"]);
        assert_eq!(batches[0].timeout, Duration::from_secs(1));
        // a disable drops exactly its tool
        let batches = connections
            .agent_tools(&agent(vec!["mcp:srv"], vec!["mcp:srv:gamma"], false))
            .expect("disable");
        let names: Vec<&str> = batches[0].tools.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["srv__alpha", "srv__beta"]);
        // a single-tool enable attaches just that tool
        let batches = connections
            .agent_tools(&agent(vec!["mcp:srv:beta"], vec![], false))
            .expect("subset");
        let names: Vec<&str> = batches[0].tools.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["srv__beta"]);
        // a read-only agent keeps only the tools annotated read-only
        let batches = connections
            .agent_tools(&agent(vec!["mcp:srv"], vec![], true))
            .expect("read only");
        let names: Vec<&str> = batches[0].tools.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["srv__alpha"]);
        // unknown enables and disables are typos worth rejecting
        let error = connections
            .agent_tools(&agent(vec!["mcp:srv:nope"], vec![], false))
            .unwrap_err()
            .to_string();
        assert!(error.contains("enables unknown MCP tool 'nope'"));
        let error = connections
            .agent_tools(&agent(vec!["mcp:srv"], vec!["mcp:srv:nope"], false))
            .unwrap_err()
            .to_string();
        assert!(error.contains("disables unknown MCP tool 'nope'"));
        // a reference to a server that was skipped resolves to nothing
        let batches = connections
            .agent_tools(&agent(vec!["mcp:other"], vec![], false))
            .expect("skipped server");
        assert!(batches.is_empty());
        task.abort();
    }

    /// A read-only server keeps every tool for a read-only agent.
    #[tokio::test]
    async fn read_only_servers_keep_unannotated_tools() {
        // the same mixed toolset on a server declared read-only wholesale
        let tools = vec![tool("alpha", Some(true)), tool("beta", None)];
        let (connections, task) = fixture("srv", tools, true).await;
        // the read-only agent keeps both tools
        let batches = connections
            .agent_tools(&agent(vec!["mcp:srv"], vec![], true))
            .expect("read only server");
        let names: Vec<&str> = batches[0].tools.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["srv__alpha", "srv__beta"]);
        task.abort();
    }

    /// Prefixed names must satisfy the strictest provider's rules.
    #[tokio::test]
    async fn prefixed_names_enforce_provider_limits() {
        // one tool whose prefixed name is too long, one with a bad charset
        let long = "a".repeat(MAX_TOOL_NAME_LEN);
        let (connections, task) = fixture("srv", vec![tool(&long, None)], false).await;
        let error = connections
            .agent_tools(&agent(vec!["mcp:srv"], vec![], false))
            .unwrap_err()
            .to_string();
        assert!(error.contains("exceeds"));
        task.abort();
        let (connections, task) = fixture("srv", vec![tool("with.dot", None)], false).await;
        let error = connections
            .agent_tools(&agent(vec!["mcp:srv"], vec![], false))
            .unwrap_err()
            .to_string();
        assert!(error.contains("with.dot"));
        task.abort();
    }

    /// A call dispatched through a connection's sink round-trips raw names.
    #[tokio::test]
    async fn sink_calls_use_raw_tool_names() {
        // connect a fixture server with a single tool
        let (connections, task) = fixture("srv", vec![tool("echo", None)], false).await;
        // call the tool by its raw name through the frozen sink
        let connection = &connections.servers["srv"];
        let result = connection
            .sink
            .call_tool(CallToolRequestParams::new("echo"))
            .await
            .expect("call should round trip");
        // the fixture echoes the wire-call name back
        let text = format!("{:?}", result.content);
        assert!(text.contains("called echo"));
        task.abort();
    }

    /// Connect-time reference validation backstops builder-only paths.
    #[tokio::test]
    async fn connect_validates_references_and_failure_policy() {
        // referencing an unconfigured server fails the connect pass
        let configs = BTreeMap::new();
        let agents = vec![agent(vec!["mcp:ghost"], vec![], false)];
        let error = McpConnections::connect(false, &configs, &agents, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("unconfigured MCP server 'ghost'"));
        // referencing the built-in server without the flag fails
        let agents = vec![agent(vec!["mcp:thorium"], vec![], false)];
        let error = McpConnections::connect(false, &configs, &agents, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("thorium_mcp"));
        // the flag alone is not enough without a credentials resolver
        let error = McpConnections::connect(true, &configs, &agents, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("resolver"));
        // a non-optional server that cannot spawn fails with the opt-out hint
        let server = McpServerConfig {
            command: Some("/nonexistent/thorpal-mcp-fixture".to_owned()),
            args: Vec::new(),
            env: BTreeMap::new(),
            url: None,
            headers: BTreeMap::new(),
            timeout_secs: None,
            optional: false,
            read_only: false,
        };
        let configs = BTreeMap::from([("sandbox".to_owned(), server.clone())]);
        let agents = vec![agent(vec!["mcp:sandbox"], vec![], false)];
        let error = format!(
            "{:#}",
            McpConnections::connect(false, &configs, &agents, None)
                .await
                .unwrap_err()
        );
        assert!(error.contains("optional: true"));
        // the same failure on an optional server degrades to no tools
        let mut optional = server;
        optional.optional = true;
        let configs = BTreeMap::from([("sandbox".to_owned(), optional)]);
        let connections = McpConnections::connect(false, &configs, &agents, None)
            .await
            .expect("optional failures should be skipped");
        assert!(connections.servers.is_empty());
        let batches = connections
            .agent_tools(&agents[0])
            .expect("skipped server resolves to nothing");
        assert!(batches.is_empty());
        // an unreferenced server is never connected at all
        let configs = BTreeMap::from([(
            "unused".to_owned(),
            McpServerConfig {
                command: Some("/nonexistent/thorpal-mcp-fixture".to_owned()),
                args: Vec::new(),
                env: BTreeMap::new(),
                url: None,
                headers: BTreeMap::new(),
                timeout_secs: None,
                optional: false,
                read_only: false,
            },
        )]);
        let agents = vec![agent(vec![], vec![], false)];
        let connections = McpConnections::connect(false, &configs, &agents, None)
            .await
            .expect("unreferenced servers are ignored");
        assert!(connections.servers.is_empty());
    }

    /// The built-in Thorium server connects and lists tools against a live API.
    ///
    /// Ignored by default: needs a reachable Thorium API plus `THORIUM_URL`
    /// and `THORIUM_TOKEN` in the environment, e.g.
    /// `THORIUM_URL=http://localhost:8080 THORIUM_TOKEN=... cargo test -p \
    /// thorpal live_thorium_mcp -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn live_thorium_mcp_lists_tools() {
        // the live instance to talk to comes from the environment
        let url = std::env::var("THORIUM_URL").expect("set THORIUM_URL");
        let token = std::env::var("THORIUM_TOKEN").expect("set THORIUM_TOKEN");
        // a resolver handing back the env credentials verbatim
        let resolver: ThoriumMcpResolver = Arc::new(move || {
            let (url, token) = (url.clone(), token.clone());
            Box::pin(async move {
                Ok(ThoriumMcpAuth {
                    url,
                    token,
                    accept_invalid_certs: false,
                })
            })
        });
        // connect the built-in server exactly like a real build does
        let configs = BTreeMap::new();
        let agents = vec![agent(vec!["mcp:thorium"], vec![], false)];
        let connections = McpConnections::connect(true, &configs, &agents, Some(&resolver))
            .await
            .expect("the live Thorium MCP server should connect");
        // the whole-server enable resolves the API's prefixed toolset
        let batches = connections
            .agent_tools(&agents[0])
            .expect("agent tools should resolve");
        assert_eq!(batches.len(), 1);
        let names: Vec<&str> = batches[0].tools.iter().map(|(n, _)| n.as_str()).collect();
        assert!(
            names.contains(&"thorium__get_sample"),
            "expected thorium__get_sample in {names:?}"
        );
    }
}
