//! One model listing function per provider, and the sign-in each credential kind needs
//!
//! A listing either returns every model the provider reports with the limits and capabilities
//! it reports, or fails in a way the API turns into a ban. A body that can't be parsed is an
//! unreachable endpoint, never an empty listing, so a provider changing its format can't ban a
//! whole catalog.

use chrono::Utc;
use reqwest::{Method, RequestBuilder, Url};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;
use thorium::Error;
use thorium::models::{
    AzureApiMode, AzureClientAuth, CredentialKind, CredentialSecrets, GeminiApiVersion,
    ListedModel, MAX_LISTED_MODELS, ModelBanKind, ModelCapabilities, ModelProvider, ModelSecret,
    VertexLocation, VertexMultiRegion,
};

pub mod auth;
#[cfg(test)]
mod tests;

use auth::{AwsKeys, AwsScope};

/// How long a listing request may take to connect
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a listing request may take in total
pub const TOTAL_TIMEOUT: Duration = Duration::from_secs(30);

/// The most pages one listing reads before it stops
const MAX_PAGES: usize = 100;

/// The longest provider message kept in a ban
const MAX_MESSAGE: usize = 512;

/// The largest response body a listing reads
const MAX_BODY: usize = 16 * 1024 * 1024;

/// The scope Google tokens are requested for
const GCP_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";

/// The Google OAuth token url service account assertions are exchanged at
const GCP_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// The Vertex AI publishers whose models are listed
const VERTEX_PUBLISHERS: [&str; 2] = ["google", "anthropic"];

/// Build the HTTP client provider listings are sent with
///
/// It connects within 5 seconds, gives up after 30, and never follows redirects, so a
/// credential is only ever sent to the configured host.
pub fn listing_client() -> Result<reqwest::Client, Error> {
    // bound every request and refuse redirects
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(TOTAL_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|err| Error::new(format!("Failed to build the listing client: {err}")))
}

/// The bounds one listing runs under, sign-in included
#[derive(Debug, Clone, Copy)]
pub struct ListingLimits {
    /// How long one listing may take in total
    pub deadline: Duration,
    /// The most requests one listing sends, counting sign-in, pages, and model descriptions
    pub requests: usize,
}

impl Default for ListingLimits {
    /// Two minutes and a hundred requests per listing
    fn default() -> Self {
        ListingLimits {
            deadline: Duration::from_secs(120),
            requests: 100,
        }
    }
}

/// Refuse a listing that holds more models than a report may list
///
/// # Arguments
///
/// * `count` - How many models the listing holds so far
fn check_count(count: usize) -> Result<(), ListingError> {
    // a report can't hold more than the API accepts
    if count > MAX_LISTED_MODELS {
        return Err(ListingError::Unreachable(format!(
            "the endpoint listed more than {MAX_LISTED_MODELS} models"
        )));
    }
    Ok(())
}

/// Where the sign-in requests that aren't sent to an endpoint's own url go
#[derive(Debug, Clone)]
pub struct AuthHosts {
    /// The AWS STS url to assume roles at (unset = the endpoint region's STS)
    pub sts: Option<String>,
    /// The Entra ID login url
    pub entra: String,
    /// The Google IAM Credentials url
    pub iam_credentials: String,
    /// The Google OAuth token url, the only url a service account file's assertion is sent to
    pub gcp_token: String,
}

impl Default for AuthHosts {
    /// The public cloud sign-in hosts
    fn default() -> Self {
        AuthHosts {
            sts: None,
            entra: "https://login.microsoftonline.com".to_owned(),
            iam_credentials: "https://iamcredentials.googleapis.com".to_owned(),
            gcp_token: GCP_TOKEN_URL.to_owned(),
        }
    }
}

/// A credential a listing signs in with, and the access key an assumed role uses
#[derive(Debug, Clone)]
pub struct ListingCredential {
    /// What kind of credential this is and its metadata
    pub kind: CredentialKind,
    /// The credential's secrets
    pub secrets: CredentialSecrets,
    /// The access key credential an assumed role signs in with
    pub source: Option<Box<ListingCredential>>,
}

impl ListingCredential {
    /// Get the primary secret, which every kind but an assumed role holds
    fn primary(&self) -> Result<&ModelSecret, ListingError> {
        // a stored credential always has its secret, but a missing one can't sign in
        self.secrets.primary.as_ref().ok_or_else(|| {
            ListingError::local(format!(
                "the {} credential has no secret",
                self.kind.cred_type()
            ))
        })
    }

    /// Every secret this credential holds, so they can be kept out of messages
    fn secret_values(&self) -> Vec<String> {
        // collect the primary secret and session token, and the source's
        let mut values = Vec::new();
        if let Some(primary) = &self.secrets.primary {
            values.push(primary.expose().to_owned());
        }
        if let Some(token) = &self.secrets.session_token {
            values.push(token.expose().to_owned());
        }
        if let Some(source) = &self.source {
            values.extend(source.secret_values());
        }
        values
    }
}

/// Why a listing failed
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListingError {
    /// The provider or a sign-in endpoint refused the credential
    Rejected {
        /// The HTTP status, or 0 when the credential was refused before any request
        status: u16,
        /// Why it was refused
        message: String,
    },
    /// The provider reported the credential expired
    Expired(String),
    /// The endpoint couldn't be reached or answered with something that isn't a listing
    Unreachable(String),
    /// A failure that may pass, so the refresh is retried later
    Retry(String),
}

impl ListingError {
    /// Build the rejection for a credential refused before any request was sent
    ///
    /// # Arguments
    ///
    /// * `message` - Why it was refused
    #[must_use]
    pub fn local(message: impl Into<String>) -> Self {
        ListingError::Rejected {
            status: 0,
            message: message.into(),
        }
    }

    /// Get the failure a report records for this error, or nothing when it should be retried
    #[must_use]
    pub fn failure(&self) -> Option<ModelBanKind> {
        // a retry records nothing
        match self {
            ListingError::Rejected { status, message } => Some(ModelBanKind::CredentialRejected {
                status: *status,
                message: message.clone(),
            }),
            ListingError::Expired(_) => Some(ModelBanKind::CredentialExpired),
            ListingError::Unreachable(message) => {
                Some(ModelBanKind::EndpointUnreachable(message.clone()))
            }
            ListingError::Retry(_) => None,
        }
    }

    /// Remove every secret from this error's message
    ///
    /// # Arguments
    ///
    /// * `secrets` - The secret values to remove
    #[must_use]
    fn redacted(self, secrets: &[String]) -> Self {
        // replace each secret in the whole message, then shorten it, so no part of a secret
        // that crossed the limit is left behind
        let scrub = |message: String| {
            let scrubbed = secrets
                .iter()
                .filter(|secret| !secret.is_empty())
                .fold(message, |message, secret| {
                    message.replace(secret.as_str(), "<redacted>")
                });
            clip(&scrubbed)
        };
        match self {
            ListingError::Rejected { status, message } => ListingError::Rejected {
                status,
                message: scrub(message),
            },
            ListingError::Expired(message) => ListingError::Expired(scrub(message)),
            ListingError::Unreachable(message) => ListingError::Unreachable(scrub(message)),
            ListingError::Retry(message) => ListingError::Retry(scrub(message)),
        }
    }
}

/// Shorten a provider message so a ban never holds a whole response body
///
/// # Arguments
///
/// * `message` - The message to shorten
fn clip(message: &str) -> String {
    // keep whole characters up to the limit
    let trimmed = message.trim();
    match trimmed.char_indices().nth(MAX_MESSAGE) {
        Some((end, _)) => format!("{}…", &trimmed[..end]),
        None => trimmed.to_owned(),
    }
}

/// Pull the error message out of a provider's error body
///
/// The message is kept whole so its secrets can be removed before it's shortened.
///
/// # Arguments
///
/// * `body` - The response body
fn error_message(body: &str) -> String {
    // most providers nest the message under `error`
    let parsed = serde_json::from_str::<Value>(body).ok();
    let message = parsed.as_ref().and_then(|value| {
        value
            .pointer("/error/message")
            .or_else(|| value.pointer("/error_description"))
            .or_else(|| value.pointer("/message"))
            .or_else(|| value.pointer("/Error/Message"))
            .or_else(|| value.get("error"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    });
    message.unwrap_or_else(|| body.to_owned())
}

/// Map a provider's non-success status to a listing error
///
/// `401` and `403` reject the credential, or report it expired when the message says so;
/// `408` and `429` are retried; every other status means the endpoint can't serve a listing.
///
/// # Arguments
///
/// * `status` - The status the provider answered with
/// * `body` - The response body
#[must_use]
pub fn classify(status: u16, body: &str) -> ListingError {
    // read the provider's message
    let message = error_message(body);
    match status {
        401 | 403 if message.to_lowercase().contains("expired") => ListingError::Expired(message),
        401 | 403 => ListingError::Rejected { status, message },
        408 | 429 => ListingError::Retry(format!("HTTP {status}: {message}")),
        _ => ListingError::Unreachable(format!("HTTP {status}: {message}")),
    }
}

/// Map a request that got no response to a listing error
///
/// # Arguments
///
/// * `error` - The request error
fn transport_error(error: &reqwest::Error) -> ListingError {
    // name timeouts and connection failures plainly
    if error.is_timeout() {
        ListingError::Unreachable(format!("the request timed out: {error}"))
    } else if error.is_connect() {
        ListingError::Unreachable(format!("the connection failed: {error}"))
    } else {
        ListingError::Unreachable(format!("the request failed: {error}"))
    }
}

/// Send a request and read its JSON body, mapping every failure to a listing error
///
/// # Arguments
///
/// * `req` - The request to send
async fn send_json(req: RequestBuilder) -> Result<Value, ListingError> {
    // send the request
    let mut resp = req.send().await.map_err(|err| transport_error(&err))?;
    let status = resp.status().as_u16();
    // read the body, refusing one too large to be a listing
    let mut raw = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|err| transport_error(&err))? {
        if raw.len() + chunk.len() > MAX_BODY {
            return Err(ListingError::Unreachable(format!(
                "the response is larger than {MAX_BODY} bytes"
            )));
        }
        raw.extend_from_slice(&chunk);
    }
    let body = String::from_utf8_lossy(&raw);
    // a failed status is classified
    if !(200..300).contains(&status) {
        return Err(classify(status, &body));
    }
    // a body that isn't JSON, or nests too deeply to read, can't be a listing
    serde_json::from_str(&body)
        .map_err(|err| ListingError::Unreachable(format!("the response isn't JSON: {err}")))
}

/// Parse a url, mapping a bad url to an unreachable endpoint
///
/// # Arguments
///
/// * `raw` - The url to parse
fn parse_url(raw: &str) -> Result<Url, ListingError> {
    Url::parse(raw)
        .map_err(|err| ListingError::Unreachable(format!("`{raw}` isn't a valid url: {err}")))
}

/// Join a base url and a path with exactly one slash
///
/// # Arguments
///
/// * `base` - The base url
/// * `path` - The path to add
#[must_use]
pub fn join(base: &str, path: &str) -> String {
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

/// One page of a listing
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Page {
    /// The models on this page
    pub models: Vec<ListedModel>,
    /// Where the next page starts, if there is one
    pub next: Option<String>,
}

/// Get a JSON array from a listing body, refusing a body of the wrong shape
///
/// # Arguments
///
/// * `body` - The listing body
/// * `key` - The key the array is under
fn array<'a>(body: &'a Value, key: &str) -> Result<&'a Vec<Value>, String> {
    // a listing without its array isn't a listing
    body.get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("the listing has no `{key}` array"))
}

/// Get a positive integer field of a listed model
///
/// # Arguments
///
/// * `value` - The listed model
/// * `pointer` - The JSON pointer to the field
fn number(value: &Value, pointer: &str) -> Option<u64> {
    // only positive integers are limits
    value
        .pointer(pointer)
        .and_then(Value::as_u64)
        .filter(|n| *n > 0)
}

/// Get a non-empty string field
///
/// # Arguments
///
/// * `value` - The value holding the field
/// * `key` - The field's key
fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
}

/// Read whether a capability is supported, as a flag or an object with a `supported` flag
///
/// # Arguments
///
/// * `caps` - The listed capabilities
/// * `keys` - The keys the capability may be listed under
fn supported(caps: &Value, keys: &[&str]) -> bool {
    // any key that says it's supported counts
    keys.iter().any(|key| match caps.get(key) {
        Some(Value::Bool(flag)) => *flag,
        Some(inner @ Value::Object(_)) => {
            inner.get("supported").and_then(Value::as_bool) == Some(true)
        }
        _ => false,
    })
}

/// Parse an Anthropic model listing page
///
/// # Arguments
///
/// * `body` - The listing body
pub fn parse_anthropic(body: &Value) -> Result<Page, String> {
    // read each listed model with the limits and capabilities it reports
    let mut models = Vec::new();
    for entry in array(body, "data")? {
        let Some(id) = text(entry, "id") else {
            return Err("an Anthropic model has no id".to_owned());
        };
        let capabilities = entry
            .get("capabilities")
            .filter(|caps| caps.is_object())
            .map(|caps| ModelCapabilities {
                tool_calling: supported(caps, &["tool_use", "tool_calling", "tools"]),
                vision: supported(caps, &["image_input", "vision", "images"]),
                pdf_input: supported(caps, &["pdf_input", "pdf"]),
                structured_output: supported(caps, &["structured_outputs", "structured_output"]),
                thinking: supported(caps, &["thinking", "extended_thinking"]),
                ..ModelCapabilities::default()
            });
        models.push(ListedModel {
            model_id: id.to_owned(),
            context_window: number(entry, "/max_input_tokens"),
            max_output_tokens: number(entry, "/max_tokens")
                .or_else(|| number(entry, "/max_output_tokens")),
            capabilities,
        });
    }
    // a page with more after it names the last id to start after
    let next = if body.get("has_more").and_then(Value::as_bool) == Some(true) {
        text(body, "last_id").map(ToOwned::to_owned)
    } else {
        None
    };
    Ok(Page { models, next })
}

/// Parse a Gemini Developer API model listing page
///
/// # Arguments
///
/// * `body` - The listing body
pub fn parse_gemini(body: &Value) -> Result<Page, String> {
    // an empty listing leaves out the models array entirely
    let empty = Vec::new();
    let entries = match body.get("models") {
        Some(_) => array(body, "models")?,
        None if body.is_object() => &empty,
        None => return Err("the listing isn't an object".to_owned()),
    };
    // read each model, dropping the `models/` prefix of its name
    let mut models = Vec::new();
    for entry in entries {
        let Some(name) = text(entry, "name") else {
            return Err("a Gemini model has no name".to_owned());
        };
        models.push(ListedModel {
            model_id: name.strip_prefix("models/").unwrap_or(name).to_owned(),
            context_window: number(entry, "/inputTokenLimit"),
            max_output_tokens: number(entry, "/outputTokenLimit"),
            capabilities: None,
        });
    }
    let next = text(body, "nextPageToken").map(ToOwned::to_owned);
    Ok(Page { models, next })
}

/// Parse a `LiteLLM` model info listing
///
/// `max_input_tokens` and `max_output_tokens` are preferred; the legacy `max_tokens` is only
/// read, as the output limit, when both are missing.
///
/// # Arguments
///
/// * `body` - The listing body
pub fn parse_litellm(body: &Value) -> Result<Vec<ListedModel>, String> {
    // read each model with its info
    let mut models = Vec::new();
    for entry in array(body, "data")? {
        let Some(id) = text(entry, "model_name") else {
            return Err("a LiteLLM model has no model_name".to_owned());
        };
        let input = number(entry, "/model_info/max_input_tokens");
        let output = number(entry, "/model_info/max_output_tokens");
        let legacy = if input.is_none() && output.is_none() {
            number(entry, "/model_info/max_tokens")
        } else {
            None
        };
        let info = entry.get("model_info");
        let flag =
            |key: &str| info.and_then(|info| info.get(key)).and_then(Value::as_bool) == Some(true);
        let capabilities = info
            .filter(|info| info.is_object())
            .map(|_| ModelCapabilities {
                tool_calling: flag("supports_function_calling"),
                vision: flag("supports_vision"),
                pdf_input: flag("supports_pdf_input"),
                structured_output: flag("supports_response_schema"),
                thinking: flag("supports_reasoning"),
                ..ModelCapabilities::default()
            });
        models.push(ListedModel {
            model_id: id.to_owned(),
            context_window: input,
            max_output_tokens: output.or(legacy),
            capabilities,
        });
    }
    Ok(models)
}

/// Parse an `OpenAI`-compatible model listing, and tell whether the server is Ollama
///
/// Each server reports its limits under its own keys: `OpenRouter` and Together a
/// `context_length`, Groq a `context_window` and `max_completion_tokens`, vLLM a
/// `max_model_len`. Ollama marks every model as owned by `library`.
///
/// # Arguments
///
/// * `body` - The listing body
pub fn parse_openai_compatible(body: &Value) -> Result<(Vec<ListedModel>, bool), String> {
    // read each model with whatever limits its server reports
    let entries = array(body, "data")?;
    let mut models = Vec::new();
    for entry in entries {
        let Some(id) = text(entry, "id") else {
            return Err("a listed model has no id".to_owned());
        };
        models.push(ListedModel {
            model_id: id.to_owned(),
            context_window: number(entry, "/context_length")
                .or_else(|| number(entry, "/context_window"))
                .or_else(|| number(entry, "/max_model_len")),
            max_output_tokens: number(entry, "/top_provider/max_completion_tokens")
                .or_else(|| number(entry, "/max_completion_tokens")),
            capabilities: None,
        });
    }
    // Ollama owns every model it lists as `library`
    let ollama = !entries.is_empty()
        && entries
            .iter()
            .all(|entry| text(entry, "owned_by") == Some("library"));
    Ok((models, ollama))
}

/// Parse an Ollama tag listing into its model names
///
/// # Arguments
///
/// * `body` - The listing body
pub fn parse_ollama_tags(body: &Value) -> Result<Vec<String>, String> {
    // every tag has a name
    array(body, "models")?
        .iter()
        .map(|entry| {
            text(entry, "name")
                .map(ToOwned::to_owned)
                .ok_or_else(|| "an Ollama model has no name".to_owned())
        })
        .collect()
}

/// Read the context window from an Ollama model description
///
/// # Arguments
///
/// * `body` - The `api/show` body
#[must_use]
pub fn parse_ollama_show(body: &Value) -> Option<u64> {
    // the context length is keyed by the model's architecture
    let info = body.get("model_info")?;
    let arch = info.get("general.architecture").and_then(Value::as_str)?;
    info.get(format!("{arch}.context_length").as_str())
        .and_then(Value::as_u64)
        .filter(|n| *n > 0)
}

/// Parse a listing that reports only model ids
///
/// # Arguments
///
/// * `body` - The listing body
/// * `list_key` - The key the models are under
/// * `id_key` - The key each model's id is under
/// * `next_key` - The key the next page's token is under, if the listing is paged
pub fn parse_ids(
    body: &Value,
    list_key: &str,
    id_key: &str,
    next_key: Option<&str>,
) -> Result<Page, String> {
    // read each id
    let mut models = Vec::new();
    for entry in array(body, list_key)? {
        let Some(id) = text(entry, id_key) else {
            return Err(format!("a listed model has no `{id_key}`"));
        };
        models.push(ListedModel::new(id));
    }
    let next = next_key
        .and_then(|key| text(body, key))
        .map(ToOwned::to_owned);
    Ok(Page { models, next })
}

/// Parse a Vertex AI publisher model listing, keeping the last part of each model's name
///
/// # Arguments
///
/// * `body` - The listing body
pub fn parse_vertex(body: &Value) -> Result<Page, String> {
    // an empty listing leaves out the models array entirely
    if body.is_object() && body.get("publisherModels").is_none() {
        return Ok(Page::default());
    }
    let mut page = parse_ids(body, "publisherModels", "name", Some("nextPageToken"))?;
    for model in &mut page.models {
        if let Some((_, id)) = model.model_id.rsplit_once('/') {
            model.model_id = id.to_owned();
        }
    }
    Ok(page)
}

/// Get the url a Gemini Developer API endpoint lists its models at
///
/// # Arguments
///
/// * `base_url` - The configured base url, if any
/// * `version` - The API version to call
#[must_use]
pub fn gemini_url(base_url: Option<&str>, version: GeminiApiVersion) -> String {
    // the version is a path segment
    let version = match version {
        GeminiApiVersion::V1 => "v1",
        GeminiApiVersion::V1Beta => "v1beta",
    };
    let base = base_url.unwrap_or("https://generativelanguage.googleapis.com");
    join(base, &format!("{version}/models"))
}

/// Get the url an Azure `OpenAI` endpoint lists its base models at
///
/// # Arguments
///
/// * `endpoint` - The resource endpoint
/// * `mode` - Which API surface to call
#[must_use]
pub fn azure_url(endpoint: &str, mode: &AzureApiMode) -> String {
    // the v1 surface takes no api version
    match mode {
        AzureApiMode::V1 => join(endpoint, "openai/v1/models"),
        AzureApiMode::Legacy { api_version } => format!(
            "{}?api-version={}",
            join(endpoint, "openai/models"),
            auth::percent_encode(api_version)
        ),
    }
}

/// Get the base url of a Vertex AI endpoint
///
/// # Arguments
///
/// * `location` - Where the endpoint calls Vertex AI
/// * `endpoint_url` - The configured url to use instead, if any
#[must_use]
pub fn vertex_base(location: &VertexLocation, endpoint_url: Option<&str>) -> String {
    // a configured url wins
    if let Some(url) = endpoint_url {
        return url.trim_end_matches('/').to_owned();
    }
    match location {
        VertexLocation::Region(region) => format!("https://{region}-aiplatform.googleapis.com"),
        VertexLocation::Global => "https://aiplatform.googleapis.com".to_owned(),
        VertexLocation::MultiRegion(VertexMultiRegion::Us) => {
            "https://aiplatform.us.rep.googleapis.com".to_owned()
        }
        VertexLocation::MultiRegion(VertexMultiRegion::Eu) => {
            "https://aiplatform.eu.rep.googleapis.com".to_owned()
        }
    }
}

/// An STS `AssumeRole` reply, read as JSON
#[derive(Deserialize)]
struct AssumeRoleReply {
    /// The reply's response element
    #[serde(rename = "AssumeRoleResponse")]
    response: AssumeRoleResponse,
}

/// The response element of an STS `AssumeRole` reply
#[derive(Deserialize)]
struct AssumeRoleResponse {
    /// The result of assuming the role
    #[serde(rename = "AssumeRoleResult")]
    result: AssumeRoleResult,
}

/// The result of assuming a role
#[derive(Deserialize)]
struct AssumeRoleResult {
    /// The assumed session's temporary keys
    #[serde(rename = "Credentials")]
    credentials: AssumeRoleCredentials,
}

/// The temporary keys of an assumed role session
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AssumeRoleCredentials {
    /// The session's access key id
    access_key_id: String,
    /// The session's secret access key
    secret_access_key: String,
    /// The session's token
    session_token: String,
}

/// How a listing request authenticates
#[derive(Debug, Clone)]
enum Auth {
    /// No authentication
    None,
    /// A header carrying the key
    Header(String, ModelSecret),
    /// A bearer token
    Bearer(ModelSecret),
    /// AWS request signing
    Aws {
        /// The keys to sign with
        keys: AwsKeys,
        /// The region to sign for
        region: String,
    },
}

/// Lists an endpoint's models through its provider's API
pub struct Lister<'a> {
    /// The client requests are sent with
    client: &'a reqwest::Client,
    /// Where sign-in requests go
    hosts: &'a AuthHosts,
    /// The bounds each listing runs under
    limits: ListingLimits,
    /// The requests this listing has sent
    sent: AtomicUsize,
    /// The tokens this listing's sign-in got, kept out of its messages like stored secrets
    derived: Mutex<Vec<String>>,
}

impl<'a> Lister<'a> {
    /// Build a lister over a listing client
    ///
    /// # Arguments
    ///
    /// * `client` - The client requests are sent with
    /// * `hosts` - Where sign-in requests go
    #[must_use]
    pub fn new(client: &'a reqwest::Client, hosts: &'a AuthHosts) -> Self {
        // listings run under the default bounds
        let limits = ListingLimits::default();
        Lister {
            client,
            hosts,
            limits,
            sent: AtomicUsize::new(0),
            derived: Mutex::default(),
        }
    }

    /// Run each listing under other bounds
    ///
    /// # Arguments
    ///
    /// * `limits` - The bounds each listing runs under
    #[must_use]
    pub fn limits(mut self, limits: ListingLimits) -> Self {
        self.limits = limits;
        self
    }

    /// List every model an endpoint serves
    ///
    /// A listing that runs past its deadline, needs more requests than its budget, or lists more
    /// models than a report holds is an unreachable endpoint. No secret ever appears in a
    /// returned error.
    ///
    /// # Arguments
    ///
    /// * `provider` - The endpoint's provider and connection settings
    /// * `credential` - The credential to sign in with, if the endpoint needs one
    pub async fn list(
        &self,
        provider: &ModelProvider,
        credential: Option<&ListingCredential>,
    ) -> Result<Vec<ListedModel>, ListingError> {
        // list with a lister of its own, which holds this listing's request budget and only the
        // tokens this listing gets, and give up at the deadline
        let listing = Lister::new(self.client, self.hosts).limits(self.limits);
        let deadline = self.limits.deadline;
        let result = tokio::time::timeout(deadline, listing.list_raw(provider, credential))
            .await
            .unwrap_or_else(|_| {
                Err(ListingError::Unreachable(format!(
                    "the listing took longer than {deadline:?}"
                )))
            });
        // keep every stored secret and every token sign-in got out of the error
        let mut secrets = credential.map_or_default(ListingCredential::secret_values);
        secrets.extend(
            listing
                .derived
                .into_inner()
                .unwrap_or_else(PoisonError::into_inner),
        );
        let models = result.map_err(|err| err.redacted(&secrets))?;
        // keep each id once
        let mut seen = HashSet::new();
        let models = models
            .into_iter()
            .filter(|model| seen.insert(model.model_id.clone()))
            .collect::<Vec<ListedModel>>();
        // a listing larger than a report holds can't be reported
        check_count(models.len())?;
        Ok(models)
    }

    /// Whether this listing has requests left in its budget
    fn has_budget(&self) -> bool {
        self.sent.load(Ordering::SeqCst) < self.limits.requests
    }

    /// Send a request and read its JSON body, spending one of the listing's requests
    ///
    /// # Arguments
    ///
    /// * `req` - The request to send
    async fn send(&self, req: RequestBuilder) -> Result<Value, ListingError> {
        // a listing that has spent its budget sends nothing more
        if self.sent.fetch_add(1, Ordering::SeqCst) >= self.limits.requests {
            return Err(ListingError::Unreachable(format!(
                "the listing needed more than {} requests",
                self.limits.requests
            )));
        }
        send_json(req).await
    }

    /// List every model an endpoint serves, by its provider
    ///
    /// # Arguments
    ///
    /// * `provider` - The endpoint's provider and connection settings
    /// * `credential` - The credential to sign in with, if the endpoint needs one
    // one arm per provider keeps each provider's listing call in one place
    #[allow(clippy::too_many_lines)]
    async fn list_raw(
        &self,
        provider: &ModelProvider,
        credential: Option<&ListingCredential>,
    ) -> Result<Vec<ListedModel>, ListingError> {
        // each provider has its own listing call
        match provider {
            ModelProvider::OpenAi {
                base_url,
                organization,
                project,
            } => {
                let auth = Self::key_auth(credential, None)?;
                let base = base_url.as_deref().unwrap_or("https://api.openai.com/v1");
                let mut extra = Vec::new();
                if let Some(org) = organization {
                    extra.push(("openai-organization".to_owned(), org.clone()));
                }
                if let Some(project) = project {
                    extra.push(("openai-project".to_owned(), project.clone()));
                }
                let body = self.get(&join(base, "models"), &auth, &extra).await?;
                Ok(parse_ids(&body, "data", "id", None)
                    .map_err(ListingError::Unreachable)?
                    .models)
            }
            ModelProvider::Anthropic {
                base_url, version, ..
            } => {
                let key = credential
                    .ok_or_else(|| ListingError::local("Anthropic needs an API key"))?
                    .primary()?
                    .clone();
                let auth = Auth::Header("x-api-key".to_owned(), key);
                let base = base_url.as_deref().unwrap_or("https://api.anthropic.com");
                let extra = vec![("anthropic-version".to_owned(), version.clone())];
                self.anthropic(base, &auth, &extra).await
            }
            ModelProvider::Gemini {
                base_url,
                api_version,
            } => {
                let key = credential
                    .ok_or_else(|| ListingError::local("Gemini needs an API key"))?
                    .primary()?
                    .clone();
                let auth = Auth::Header("x-goog-api-key".to_owned(), key);
                let url = gemini_url(base_url.as_deref(), *api_version);
                self.paged(&url, "pageToken", &auth, &[], parse_gemini)
                    .await
            }
            ModelProvider::LiteLlm {
                base_url,
                key_header,
            } => {
                let auth = Self::key_auth(credential, key_header.as_deref())?;
                let body = self.get(&join(base_url, "model/info"), &auth, &[]).await?;
                parse_litellm(&body).map_err(ListingError::Unreachable)
            }
            ModelProvider::OpenAiCompatible {
                base_url,
                extra_headers,
                key_header,
            } => {
                let auth = Self::key_auth(credential, key_header.as_deref())?;
                let extra = extra_headers
                    .iter()
                    .map(|(name, value)| (name.clone(), value.clone()))
                    .collect::<Vec<(String, String)>>();
                self.openai_compatible(base_url, &auth, &extra).await
            }
            ModelProvider::AzureOpenAi {
                endpoint,
                api_mode,
                entra_scope,
            } => {
                let credential = credential
                    .ok_or_else(|| ListingError::local("Azure OpenAI needs a credential"))?;
                let auth = match &credential.kind {
                    CredentialKind::AzureEntraClient {
                        tenant_id,
                        client_id,
                        auth,
                    } => {
                        let scope = entra_scope.clone().unwrap_or_else(|| match api_mode {
                            AzureApiMode::V1 => "https://ai.azure.com/.default".to_owned(),
                            AzureApiMode::Legacy { .. } => {
                                "https://cognitiveservices.azure.com/.default".to_owned()
                            }
                        });
                        let token = self
                            .entra_token(tenant_id, client_id, *auth, credential.primary()?, &scope)
                            .await?;
                        Auth::Bearer(token)
                    }
                    _ => Auth::Header("api-key".to_owned(), credential.primary()?.clone()),
                };
                let body = self.get(&azure_url(endpoint, api_mode), &auth, &[]).await?;
                Ok(parse_ids(&body, "data", "id", None)
                    .map_err(ListingError::Unreachable)?
                    .models)
            }
            ModelProvider::Bedrock {
                region,
                endpoint_url,
                ..
            } => {
                let credential =
                    credential.ok_or_else(|| ListingError::local("Bedrock needs a credential"))?;
                let auth = self.aws_auth(credential, region).await?;
                let base = endpoint_url
                    .clone()
                    .unwrap_or_else(|| format!("https://bedrock.{region}.amazonaws.com"));
                let mut models = self
                    .paged(
                        &join(&base, "foundation-models"),
                        "nextToken",
                        &auth,
                        &[],
                        |body| parse_ids(body, "modelSummaries", "modelId", Some("nextToken")),
                    )
                    .await?;
                models.extend(
                    self.paged(
                        &join(&base, "inference-profiles"),
                        "nextToken",
                        &auth,
                        &[],
                        |body| {
                            parse_ids(
                                body,
                                "inferenceProfileSummaries",
                                "inferenceProfileId",
                                Some("nextToken"),
                            )
                        },
                    )
                    .await?,
                );
                Ok(models)
            }
            ModelProvider::Vertex {
                project_id,
                location,
                endpoint_url,
            } => {
                let credential = credential
                    .ok_or_else(|| ListingError::local("Vertex AI needs a credential"))?;
                let auth = match &credential.kind {
                    CredentialKind::GcpCredentialsJson {
                        impersonate_service_account,
                    } => Auth::Bearer(
                        self.gcp_token(
                            credential.primary()?,
                            impersonate_service_account.as_deref(),
                        )
                        .await?,
                    ),
                    _ => Auth::Header("x-goog-api-key".to_owned(), credential.primary()?.clone()),
                };
                let mut extra = Vec::new();
                if let Some(project) = project_id {
                    extra.push(("x-goog-user-project".to_owned(), project.clone()));
                }
                let base = vertex_base(location, endpoint_url.as_deref());
                let mut models = Vec::new();
                for publisher in VERTEX_PUBLISHERS {
                    let url = join(&base, &format!("v1beta1/publishers/{publisher}/models"));
                    models.extend(
                        self.paged(&url, "pageToken", &auth, &extra, parse_vertex)
                            .await?,
                    );
                }
                Ok(models)
            }
        }
    }

    /// Get the auth for a provider that takes an API key, or none for a keyless gateway
    ///
    /// # Arguments
    ///
    /// * `credential` - The credential, if there is one
    /// * `key_header` - The header to send the key in instead of a bearer token
    fn key_auth(
        credential: Option<&ListingCredential>,
        key_header: Option<&str>,
    ) -> Result<Auth, ListingError> {
        // a gateway without a credential takes no key
        let Some(credential) = credential else {
            return Ok(Auth::None);
        };
        let key = credential.primary()?.clone();
        Ok(match key_header {
            Some(header) => Auth::Header(header.to_owned(), key),
            None => Auth::Bearer(key),
        })
    }

    /// Build and authenticate a request
    ///
    /// # Arguments
    ///
    /// * `method` - The request's method
    /// * `url` - The request's url
    /// * `auth` - How the request authenticates
    /// * `extra` - Other headers to send
    /// * `body` - The request's JSON body, if it has one
    fn request(
        &self,
        method: &Method,
        url: &str,
        auth: &Auth,
        extra: &[(String, String)],
        body: Option<&Value>,
    ) -> Result<RequestBuilder, ListingError> {
        // start the request with its headers and body
        let parsed = parse_url(url)?;
        let raw_body = body.map_or_default(Value::to_string);
        let mut req = self.client.request(method.clone(), parsed.clone());
        for (name, value) in extra {
            req = req.header(name.as_str(), value.as_str());
        }
        if body.is_some() {
            req = req
                .header("content-type", "application/json")
                .body(raw_body.clone());
        }
        // add the credential the way it's sent
        Ok(match auth {
            Auth::None => req,
            Auth::Header(name, key) => req.header(name.as_str(), key.expose()),
            Auth::Bearer(token) => req.bearer_auth(token.expose()),
            Auth::Aws { keys, region } => {
                let mut signed = extra.to_vec();
                if body.is_some() {
                    signed.push(("content-type".to_owned(), "application/json".to_owned()));
                }
                let scope = AwsScope {
                    region,
                    service: "bedrock",
                    time: Utc::now(),
                };
                let headers = auth::sigv4_headers(
                    method.as_str(),
                    &parsed,
                    &signed,
                    raw_body.as_bytes(),
                    keys,
                    scope,
                )
                .map_err(ListingError::Unreachable)?;
                headers.iter().fold(req, |req, (name, value)| {
                    req.header(name.as_str(), value.as_str())
                })
            }
        })
    }

    /// Send an authenticated `GET` and read its JSON body
    ///
    /// # Arguments
    ///
    /// * `url` - The url to get
    /// * `auth` - How the request authenticates
    /// * `extra` - Other headers to send
    async fn get(
        &self,
        url: &str,
        auth: &Auth,
        extra: &[(String, String)],
    ) -> Result<Value, ListingError> {
        self.send(self.request(&Method::GET, url, auth, extra, None)?)
            .await
    }

    /// Read every page of a listing that's paged with a token query parameter
    ///
    /// # Arguments
    ///
    /// * `url` - The listing's url
    /// * `token_param` - The query parameter the next page's token is sent in
    /// * `auth` - How the requests authenticate
    /// * `extra` - Other headers to send
    /// * `parse` - Parses one page
    async fn paged<F>(
        &self,
        url: &str,
        token_param: &str,
        auth: &Auth,
        extra: &[(String, String)],
        parse: F,
    ) -> Result<Vec<ListedModel>, ListingError>
    where
        F: Fn(&Value) -> Result<Page, String>,
    {
        // read pages until there's no next one, up to a bound
        let mut models = Vec::new();
        let mut token: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let mut page_url = parse_url(url)?;
            if let Some(token) = &token {
                page_url.query_pairs_mut().append_pair(token_param, token);
            }
            let body = self.get(page_url.as_str(), auth, extra).await?;
            let page = parse(&body).map_err(ListingError::Unreachable)?;
            models.extend(page.models);
            // stop once the listing holds more than a report can
            check_count(models.len())?;
            match page.next {
                Some(next) => token = Some(next),
                None => return Ok(models),
            }
        }
        Ok(models)
    }

    /// Read every page of an Anthropic model listing
    ///
    /// # Arguments
    ///
    /// * `base` - The base url
    /// * `auth` - How the requests authenticate
    /// * `extra` - Other headers to send
    async fn anthropic(
        &self,
        base: &str,
        auth: &Auth,
        extra: &[(String, String)],
    ) -> Result<Vec<ListedModel>, ListingError> {
        // page with the last id of each page
        let mut models = Vec::new();
        let mut after: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let mut url = parse_url(&join(base, "v1/models"))?;
            url.query_pairs_mut().append_pair("limit", "1000");
            if let Some(after) = &after {
                url.query_pairs_mut().append_pair("after_id", after);
            }
            let body = self.get(url.as_str(), auth, extra).await?;
            let page = parse_anthropic(&body).map_err(ListingError::Unreachable)?;
            models.extend(page.models);
            // stop once the listing holds more than a report can
            check_count(models.len())?;
            match page.next {
                Some(next) => after = Some(next),
                None => return Ok(models),
            }
        }
        Ok(models)
    }

    /// List an `OpenAI`-compatible server's models, asking Ollama for each model's context
    ///
    /// Ollama is asked about each model only while the listing's request budget lasts; the
    /// models past it are listed without a context window.
    ///
    /// # Arguments
    ///
    /// * `base_url` - The server's base url
    /// * `auth` - How the requests authenticate
    /// * `extra` - Other headers to send
    async fn openai_compatible(
        &self,
        base_url: &str,
        auth: &Auth,
        extra: &[(String, String)],
    ) -> Result<Vec<ListedModel>, ListingError> {
        // list the models the OpenAI way
        let body = self.get(&join(base_url, "models"), auth, extra).await?;
        let (models, ollama) = parse_openai_compatible(&body).map_err(ListingError::Unreachable)?;
        if !ollama {
            return Ok(models);
        }
        // Ollama's own API is at the server's root and reports each model's context
        let root = base_url.trim_end_matches('/').trim_end_matches("/v1");
        let tags = self.get(&join(root, "api/tags"), auth, extra).await?;
        let names = parse_ollama_tags(&tags).map_err(ListingError::Unreachable)?;
        // a server with more models than a report holds isn't described model by model
        check_count(names.len())?;
        let mut listed = Vec::with_capacity(names.len());
        for name in names {
            // the models past the request budget are listed without a description
            if !self.has_budget() {
                listed.push(ListedModel {
                    model_id: name,
                    context_window: None,
                    max_output_tokens: None,
                    capabilities: None,
                });
                continue;
            }
            let show = self.request(
                &Method::POST,
                &join(root, "api/show"),
                auth,
                extra,
                Some(&json!({ "model": name })),
            )?;
            let shown = self.send(show).await?;
            listed.push(ListedModel {
                model_id: name,
                context_window: parse_ollama_show(&shown),
                max_output_tokens: None,
                capabilities: None,
            });
        }
        Ok(listed)
    }

    /// Get the AWS signing auth for a Bedrock credential, assuming a role first if needed
    ///
    /// # Arguments
    ///
    /// * `credential` - The credential
    /// * `region` - The endpoint's region
    async fn aws_auth(
        &self,
        credential: &ListingCredential,
        region: &str,
    ) -> Result<Auth, ListingError> {
        // each AWS credential kind signs in its own way
        match &credential.kind {
            CredentialKind::AwsAccessKey { access_key_id } => Ok(Auth::Aws {
                keys: AwsKeys {
                    access_key_id: access_key_id.clone(),
                    secret: credential.primary()?.clone(),
                    session_token: credential.secrets.session_token.clone(),
                },
                region: region.to_owned(),
            }),
            CredentialKind::AwsAssumeRole { .. } => Ok(Auth::Aws {
                keys: self.assume_role(credential, region).await?,
                region: region.to_owned(),
            }),
            CredentialKind::AwsBearerToken => Ok(Auth::Bearer(credential.primary()?.clone())),
            other => Err(ListingError::local(format!(
                "Bedrock doesn't take a {} credential",
                other.cred_type()
            ))),
        }
    }

    /// Assume a role with its source access key and get the session's keys
    ///
    /// The reply is read as JSON.
    ///
    /// # Arguments
    ///
    /// * `credential` - The assumed role credential
    /// * `region` - The region whose STS is called
    async fn assume_role(
        &self,
        credential: &ListingCredential,
        region: &str,
    ) -> Result<AwsKeys, ListingError> {
        // the role's settings
        let CredentialKind::AwsAssumeRole {
            role_arn,
            external_id,
            session_name,
            duration_seconds,
            ..
        } = &credential.kind
        else {
            return Err(ListingError::local("not an assumed role"));
        };
        // the source access key signs the call
        let source = credential.source.as_deref().ok_or_else(|| {
            ListingError::local("the assumed role's source access key is missing")
        })?;
        let CredentialKind::AwsAccessKey { access_key_id } = &source.kind else {
            return Err(ListingError::local(
                "an assumed role's source must be an AwsAccessKey credential",
            ));
        };
        let keys = AwsKeys {
            access_key_id: access_key_id.clone(),
            secret: source.primary()?.clone(),
            session_token: source.secrets.session_token.clone(),
        };
        // build the AssumeRole call
        let mut url = parse_url(
            &self
                .hosts
                .sts
                .clone()
                .unwrap_or_else(|| format!("https://sts.{region}.amazonaws.com/")),
        )?;
        {
            let mut query = url.query_pairs_mut();
            query
                .append_pair("Action", "AssumeRole")
                .append_pair("Version", "2011-06-15")
                .append_pair("RoleArn", role_arn)
                .append_pair(
                    "RoleSessionName",
                    session_name.as_deref().unwrap_or("thorium-model-refresh"),
                );
            if let Some(external_id) = external_id {
                query.append_pair("ExternalId", external_id);
            }
            if let Some(duration) = duration_seconds {
                query.append_pair("DurationSeconds", &duration.to_string());
            }
        }
        let extra = vec![("accept".to_owned(), "application/json".to_owned())];
        let scope = AwsScope {
            region,
            service: "sts",
            time: Utc::now(),
        };
        let headers = auth::sigv4_headers("POST", &url, &extra, b"", &keys, scope)
            .map_err(ListingError::Unreachable)?;
        let req = headers.iter().fold(
            self.client.post(url).header("accept", "application/json"),
            |req, (name, value)| req.header(name.as_str(), value.as_str()),
        );
        // a refused call rejects the credential
        let body = match self.send(req).await {
            Err(ListingError::Unreachable(message)) if message.starts_with("HTTP 4") => {
                return Err(ListingError::Rejected {
                    status: 400,
                    message,
                });
            }
            other => other?,
        };
        // read the session's keys out of the reply
        let reply = AssumeRoleReply::deserialize(body).map_err(|err| {
            ListingError::Unreachable(clip(&format!("the AssumeRole reply can't be read: {err}")))
        })?;
        let creds = reply.response.result.credentials;
        // keep the session's keys out of any later message
        self.derive(&creds.secret_access_key);
        self.derive(&creds.session_token);
        Ok(AwsKeys {
            access_key_id: creds.access_key_id,
            secret: ModelSecret::from(creds.secret_access_key),
            session_token: Some(ModelSecret::from(creds.session_token)),
        })
    }

    /// Send a token request and read its token, rejecting the credential when it's refused
    ///
    /// # Arguments
    ///
    /// * `req` - The token request
    /// * `field` - The JSON field the token is in
    async fn token(&self, req: RequestBuilder, field: &str) -> Result<ModelSecret, ListingError> {
        // a refused exchange rejects the credential
        let body = match self.send(req).await {
            Err(ListingError::Unreachable(message)) if message.starts_with("HTTP 4") => {
                let status = message
                    .get(5..8)
                    .and_then(|code| code.parse().ok())
                    .unwrap_or(400);
                return Err(ListingError::Rejected { status, message });
            }
            other => other?,
        };
        let token = body
            .get(field)
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .ok_or_else(|| ListingError::Unreachable(format!("the token reply has no {field}")))?;
        // keep the token out of any later message
        self.derive(token);
        Ok(ModelSecret::from(token))
    }

    /// Note a token this listing's sign-in got, so it's kept out of the listing's messages
    ///
    /// # Arguments
    ///
    /// * `token` - The token
    fn derive(&self, token: &str) {
        // a poisoned list still holds every token noted before the panic
        self.derived
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(token.to_owned());
    }

    /// Exchange a Google credentials file for an access token, impersonating an account if asked
    ///
    /// # Arguments
    ///
    /// * `file` - The credentials JSON
    /// * `impersonate` - The service account to impersonate, if any
    async fn gcp_token(
        &self,
        file: &ModelSecret,
        impersonate: Option<&str>,
    ) -> Result<ModelSecret, ListingError> {
        // only service account files are supported
        let parsed = serde_json::from_str::<Value>(file.expose())
            .map_err(|_| ListingError::local("the GcpCredentialsJson secret isn't valid JSON"))?;
        match parsed.get("type").and_then(Value::as_str) {
            Some("service_account") => (),
            Some(other) => {
                return Err(ListingError::local(format!(
                    "GcpCredentialsJson files of type `{other}` aren't supported for model listing; use a service_account file"
                )));
            }
            None => {
                return Err(ListingError::local(
                    "the GcpCredentialsJson file has no type",
                ));
            }
        }
        let field = |key: &str| {
            parsed.get(key).and_then(Value::as_str).ok_or_else(|| {
                ListingError::local(format!("the GcpCredentialsJson file has no {key}"))
            })
        };
        let email = field("client_email")?;
        // the assertion is only ever sent to the pinned token url, so a file can't point the
        // worker at another host
        let token_url = self.hosts.gcp_token.as_str();
        if let Some(token_uri) = parsed.get("token_uri").and_then(Value::as_str)
            && token_uri.trim_end_matches('/') != token_url.trim_end_matches('/')
        {
            return Err(ListingError::local(format!(
                "the GcpCredentialsJson file's token_uri `{}` isn't {token_url}, the only url service account tokens are exchanged at",
                clip(token_uri)
            )));
        }
        let key = auth::rsa_key(field("private_key")?).map_err(ListingError::local)?;
        // sign an assertion for the pinned token url and exchange it for an access token
        let now = Utc::now().timestamp();
        let claims = json!({
            "iss": email,
            "scope": GCP_SCOPE,
            "aud": token_url,
            "iat": now,
            "exp": now + 3600,
        });
        let mut header = Vec::new();
        if let Some(kid) = parsed.get("private_key_id").and_then(Value::as_str) {
            header.push(("kid", kid));
        }
        let assertion = auth::rs256_jwt(&header, &claims, &key).map_err(ListingError::local)?;
        let req = self.client.post(parse_url(token_url)?).form(&[
            ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
            ("assertion", assertion.as_str()),
        ]);
        let token = self.token(req, "access_token").await?;
        // impersonate another account with that token if asked
        let Some(account) = impersonate else {
            return Ok(token);
        };
        let url = join(
            &self.hosts.iam_credentials,
            &format!(
                "v1/projects/-/serviceAccounts/{}:generateAccessToken",
                auth::percent_encode(account)
            ),
        );
        let body = json!({ "scope": [GCP_SCOPE] });
        let req = self
            .client
            .post(parse_url(&url)?)
            .bearer_auth(token.expose())
            .header("content-type", "application/json")
            .body(body.to_string());
        self.token(req, "accessToken").await
    }

    /// Get an Entra ID token with client credentials
    ///
    /// # Arguments
    ///
    /// * `tenant` - The Entra ID tenant
    /// * `client_id` - The application id
    /// * `method` - Whether the secret is a client secret or a PEM certificate
    /// * `secret` - The client secret or PEM certificate with its key
    /// * `scope` - The scope to request
    async fn entra_token(
        &self,
        tenant: &str,
        client_id: &str,
        method: AzureClientAuth,
        secret: &ModelSecret,
        scope: &str,
    ) -> Result<ModelSecret, ListingError> {
        // the token url is per tenant
        let url = join(
            &self.hosts.entra,
            &format!("{}/oauth2/v2.0/token", auth::percent_encode(tenant)),
        );
        let mut form = vec![
            ("grant_type".to_owned(), "client_credentials".to_owned()),
            ("client_id".to_owned(), client_id.to_owned()),
            ("scope".to_owned(), scope.to_owned()),
        ];
        match method {
            AzureClientAuth::ClientSecret => {
                form.push(("client_secret".to_owned(), secret.expose().to_owned()));
            }
            AzureClientAuth::Certificate => {
                // sign a client assertion naming the certificate by its thumbprint
                let key = auth::rsa_key(secret.expose()).map_err(ListingError::local)?;
                let thumbprint =
                    auth::certificate_thumbprint(secret.expose()).map_err(ListingError::local)?;
                let now = Utc::now().timestamp();
                let claims = json!({
                    "aud": url,
                    "iss": client_id,
                    "sub": client_id,
                    "jti": uuid::Uuid::new_v4().to_string(),
                    "nbf": now,
                    "exp": now + 600,
                });
                let assertion = auth::rs256_jwt(&[("x5t", &thumbprint)], &claims, &key)
                    .map_err(ListingError::local)?;
                form.push((
                    "client_assertion_type".to_owned(),
                    "urn:ietf:params:oauth:client-assertion-type:jwt-bearer".to_owned(),
                ));
                form.push(("client_assertion".to_owned(), assertion));
            }
        }
        let req = self.client.post(parse_url(&url)?).form(&form);
        self.token(req, "access_token").await
    }
}
