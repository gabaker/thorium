//! Records redacted review evidence for integration tests
//!
//! When `THORIUM_EVIDENCE_DIR` is set, tests write API exchanges, read-backs, and logs as artifacts
//! into one directory per review claim (`<THORIUM_EVIDENCE_DIR>/<claim-id>/`). Tokens, keys, and
//! registered secrets are scrubbed before anything is written. When the variable is unset, every
//! record call is a no-op.

use regex::Regex;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use crate::client::Error;

/// The environment variable holding the root directory evidence is written to
pub const EVIDENCE_DIR_ENV: &str = "THORIUM_EVIDENCE_DIR";

/// The value every redacted secret is replaced with
pub const REDACTED: &str = "<redacted>";

/// Field and header names whose values are always redacted, compared lowercase
const SECRET_NAMES: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
    "x-api-key",
];

/// Field and header name suffixes whose values are always redacted, compared lowercase
const SECRET_SUFFIXES: &[&str] = &[
    "token",
    "password",
    "secret",
    "api_key",
    "apikey",
    "api-key",
    "secret_key",
    "access_key",
    "private_key",
    "credentials",
];

/// Name suffixes of objects whose every value is a secret unless it's listed in
/// [`PUBLIC_SECRET_FIELDS`], compared lowercase
const SECRET_CONTAINER_SUFFIXES: &[&str] = &["secrets"];

/// Fields inside a secrets object that only describe a secret and are kept
const PUBLIC_SECRET_FIELDS: &[&str] = &["fingerprint"];

/// Matches credentials following an auth scheme in free text, such as log lines
static AUTH_SCHEME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(bearer|token|basic)(\s+)[A-Za-z0-9._~+/=:-]+")
        .expect("the auth scheme pattern is valid")
});

/// Matches a named value in free text: a JSON field (`"name": "value"`), a `name=value` pair, or
/// a `name: value` header, capturing the name and a quoted or bare value
static NAMED_VALUE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"("?)([A-Za-z0-9_.-]+)("?\s*[:=]\s*)(?:"([^"]*)"|([^"\s,;&}]+))"#)
        .expect("the named value pattern is valid")
});

/// Checks whether a field or header name holds a secret
///
/// # Arguments
///
/// * `name` - The field or header name to check
fn is_secret_name(name: &str) -> bool {
    // compare case insensitively
    let lower = name.to_lowercase();
    // a name is secret if it is a known secret name or ends with a secret suffix
    SECRET_NAMES.contains(&lower.as_str())
        || SECRET_SUFFIXES.iter().any(|suffix| lower.ends_with(suffix))
}

/// Checks whether a field name holds an object of secrets, like a credential's `secrets`
///
/// # Arguments
///
/// * `name` - The field name to check
fn is_secret_container(name: &str) -> bool {
    // compare case insensitively
    let lower = name.to_lowercase();
    SECRET_CONTAINER_SUFFIXES
        .iter()
        .any(|suffix| lower.ends_with(suffix))
}

/// Order registered secrets so that no secret is replaced before a longer one containing it
///
/// Replacing a short secret first would break up every longer secret that contains it and leave
/// the rest of the longer secret readable.
///
/// # Arguments
///
/// * `secrets` - The registered secrets
fn longest_first(secrets: &[String]) -> Vec<&str> {
    // skip empty secrets, which would match everywhere
    let mut ordered: Vec<&str> = secrets
        .iter()
        .map(String::as_str)
        .filter(|secret| !secret.is_empty())
        .collect();
    // replace the longest secrets first and each secret only once
    ordered.sort_by(|left, right| right.len().cmp(&left.len()).then(left.cmp(right)));
    ordered.dedup();
    ordered
}

/// Replace every registered secret in a string
///
/// # Arguments
///
/// * `text` - The text to scrub
/// * `secrets` - The registered secrets to replace
fn replace_secrets(text: &str, secrets: &[String]) -> String {
    // replace the longest secrets first so overlapping ones are fully hidden
    longest_first(secrets)
        .into_iter()
        .fold(text.to_owned(), |acc, secret| acc.replace(secret, REDACTED))
}

/// Replaces registered secrets, auth scheme credentials, and secret named values in a string
///
/// # Arguments
///
/// * `text` - The text to scrub
/// * `secrets` - The registered secrets to replace
#[must_use]
pub fn redact_text(text: &str, secrets: &[String]) -> String {
    // replace every registered secret
    let scrubbed = replace_secrets(text, secrets);
    // replace credentials that follow an auth scheme like `Bearer` or `token`
    let scrubbed = AUTH_SCHEME.replace_all(&scrubbed, format!("${{1}}${{2}}{REDACTED}"));
    // replace the values of secret fields, pairs, and headers
    NAMED_VALUE
        .replace_all(&scrubbed, |caps: &regex::Captures| {
            // get the whole match, the name, and the value
            let whole = &caps[0];
            let name = &caps[2];
            let value = caps
                .get(4)
                .or_else(|| caps.get(5))
                .map_or("", |m| m.as_str());
            // leave values that aren't secret, or that an auth scheme rule already handled
            let scheme = ["bearer", "basic", "token"].contains(&value.to_lowercase().as_str());
            if !is_secret_name(name) || scheme || value.starts_with(REDACTED) {
                return whole.to_owned();
            }
            // keep the name and separator and quote the redaction like the value was
            let quoted = caps.get(4).is_some();
            let value = if quoted {
                format!("\"{REDACTED}\"")
            } else {
                REDACTED.to_owned()
            };
            format!("{}{name}{}{value}", &caps[1], &caps[3])
        })
        .into_owned()
}

/// Redacts secret fields and registered secrets in a JSON value in place
///
/// Fields with secret names, every value inside a secrets object except the fields that only
/// describe a secret, and a lone `key` string (the shape of a bootstrap key) are replaced whole.
///
/// # Arguments
///
/// * `value` - The JSON value to scrub
/// * `secrets` - The registered secrets to replace in string values
pub fn redact_value(value: &mut Value, secrets: &[String]) {
    // scrub this value outside of any secrets object
    redact_value_in(value, secrets, false);
}

/// Redacts secret fields and registered secrets in a JSON value in place
///
/// # Arguments
///
/// * `value` - The JSON value to scrub
/// * `secrets` - The registered secrets to replace in string values
/// * `in_secrets` - Whether this value is inside a secrets object
fn redact_value_in(value: &mut Value, secrets: &[String], in_secrets: bool) {
    match value {
        // scrub each field, replacing the whole value of secret fields
        Value::Object(map) => {
            // an object holding only a `key` string is a key, not a reference to one
            let lone_key = map.len() == 1 && map.get("key").is_some_and(Value::is_string);
            for (name, field) in map.iter_mut() {
                // a scalar inside a secrets object is a secret unless it only describes one
                let secret_scalar = in_secrets
                    && !(field.is_object() || field.is_array())
                    && !PUBLIC_SECRET_FIELDS.contains(&name.to_lowercase().as_str());
                // secret fields lose their value entirely unless it's null
                if !field.is_null() && (secret_scalar || lone_key || is_secret_name(name)) {
                    *field = Value::String(REDACTED.to_owned());
                } else {
                    redact_value_in(field, secrets, in_secrets || is_secret_container(name));
                }
            }
        }
        // scrub each item of a list
        Value::Array(items) => items
            .iter_mut()
            .for_each(|item| redact_value_in(item, secrets, in_secrets)),
        // scrub registered secrets out of strings
        Value::String(text) => {
            // only replace registered secrets so plain values like `token xyz` notes stay readable
            let scrubbed = replace_secrets(text, secrets);
            if scrubbed != *text {
                *text = scrubbed;
            }
        }
        // numbers, booleans, and nulls hold nothing to redact
        _ => (),
    }
}

/// Turns a claim ID or artifact name into a single safe path component
///
/// # Arguments
///
/// * `name` - The name to sanitize
fn sanitize(name: &str) -> String {
    // keep only characters that are safe in a file name
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    // strip leading dots so the name can't be `..` or hidden
    let trimmed = cleaned.trim_start_matches('.');
    // fall back to a placeholder when nothing usable is left
    if trimmed.is_empty() {
        "unnamed".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// An API request captured as evidence
#[derive(Debug, Clone, Serialize)]
pub struct RecordedRequest {
    /// The HTTP method of the request
    pub method: String,
    /// The path (and query) of the request
    pub path: String,
    /// The request headers, keyed by lowercase name
    pub headers: BTreeMap<String, String>,
    /// The JSON body of the request, if it had one
    pub body: Option<Value>,
}

impl RecordedRequest {
    /// Creates a recorded request with no headers or body
    ///
    /// # Arguments
    ///
    /// * `method` - The HTTP method of the request
    /// * `path` - The path (and query) of the request
    pub fn new<M: Into<String>, P: Into<String>>(method: M, path: P) -> Self {
        // start with no headers or body
        RecordedRequest {
            method: method.into(),
            path: path.into(),
            headers: BTreeMap::default(),
            body: None,
        }
    }

    /// Adds a header to this request
    ///
    /// # Arguments
    ///
    /// * `name` - The header's name
    /// * `value` - The header's value
    pub fn header<N: AsRef<str>, V: Into<String>>(&mut self, name: N, value: V) -> &mut Self {
        // header names are case insensitive so store them lowercase
        self.headers
            .insert(name.as_ref().to_lowercase(), value.into());
        self
    }

    /// Sets the JSON body of this request
    ///
    /// # Arguments
    ///
    /// * `body` - The JSON body that was sent
    pub fn body(&mut self, body: Value) -> &mut Self {
        // replace any body set earlier
        self.body = Some(body);
        self
    }
}

/// An API response captured as evidence
#[derive(Debug, Clone, Serialize)]
pub struct RecordedResponse {
    /// The HTTP status code of the response
    pub status: u16,
    /// The response headers, keyed by lowercase name
    pub headers: BTreeMap<String, String>,
    /// The JSON body of the response
    pub body: Value,
}

impl RecordedResponse {
    /// Creates a recorded response with no headers
    ///
    /// # Arguments
    ///
    /// * `status` - The HTTP status code of the response
    /// * `body` - The JSON body of the response
    #[must_use]
    pub fn new(status: u16, body: Value) -> Self {
        // start with no headers
        RecordedResponse {
            status,
            headers: BTreeMap::default(),
            body,
        }
    }

    /// Adds a header to this response
    ///
    /// # Arguments
    ///
    /// * `name` - The header's name
    /// * `value` - The header's value
    pub fn header<N: AsRef<str>, V: Into<String>>(&mut self, name: N, value: V) -> &mut Self {
        // header names are case insensitive so store them lowercase
        self.headers
            .insert(name.as_ref().to_lowercase(), value.into());
        self
    }
}

/// Writes redacted evidence artifacts for one review claim
#[derive(Debug, Clone)]
pub struct EvidenceRecorder {
    /// The directory this claim's artifacts are written to, or `None` when recording is disabled
    claim_dir: Option<PathBuf>,
    /// Secrets that are replaced wherever they appear in an artifact
    secrets: Vec<String>,
}

impl EvidenceRecorder {
    /// Creates a recorder for a claim rooted at `THORIUM_EVIDENCE_DIR`, disabled when it's unset
    ///
    /// # Arguments
    ///
    /// * `claim` - The ID of the claim this recorder writes evidence for
    #[must_use]
    pub fn from_env(claim: &str) -> Self {
        // read the evidence root from the environment
        Self::from_env_value(std::env::var(EVIDENCE_DIR_ENV).ok(), claim)
    }

    /// Creates a recorder for a claim from the value of `THORIUM_EVIDENCE_DIR`
    ///
    /// # Arguments
    ///
    /// * `value` - The variable's value, if it's set
    /// * `claim` - The ID of the claim this recorder writes evidence for
    #[must_use]
    pub fn from_env_value(value: Option<String>, claim: &str) -> Self {
        // an empty value disables recording just like an unset one
        let root = value.filter(|value| !value.is_empty()).map(PathBuf::from);
        Self::new(root, claim)
    }

    /// Creates a recorder for a claim under an evidence root
    ///
    /// # Arguments
    ///
    /// * `root` - The evidence root directory, or `None` to disable recording
    /// * `claim` - The ID of the claim this recorder writes evidence for
    #[must_use]
    pub fn new(root: Option<PathBuf>, claim: &str) -> Self {
        // each claim gets its own sanitized directory under the root
        let claim_dir = root.map(|root| root.join(sanitize(claim)));
        EvidenceRecorder {
            claim_dir,
            secrets: Vec::default(),
        }
    }

    /// Whether this recorder writes anything
    #[must_use]
    pub fn enabled(&self) -> bool {
        // recording is enabled whenever there is a claim directory to write to
        self.claim_dir.is_some()
    }

    /// The directory this claim's artifacts are written to, if recording is enabled
    #[must_use]
    pub fn claim_dir(&self) -> Option<&Path> {
        self.claim_dir.as_deref()
    }

    /// Registers a secret to scrub from every artifact this recorder writes
    ///
    /// # Arguments
    ///
    /// * `secret` - The secret value to scrub
    pub fn register_secret<S: Into<String>>(&mut self, secret: S) -> &mut Self {
        // remember the secret so every later artifact is scrubbed of it
        self.secrets.push(secret.into());
        self
    }

    /// Writes an artifact's contents into the claim directory
    ///
    /// # Arguments
    ///
    /// * `file_name` - The sanitized file name to write
    /// * `contents` - The already redacted contents
    fn write(&self, file_name: &str, contents: &str) -> Result<Option<PathBuf>, Error> {
        // skip writing entirely when recording is disabled
        let Some(claim_dir) = &self.claim_dir else {
            return Ok(None);
        };
        // make sure the claim directory exists
        std::fs::create_dir_all(claim_dir)?;
        // write the artifact, replacing any copy from an earlier run
        let path = claim_dir.join(file_name);
        std::fs::write(&path, contents)?;
        Ok(Some(path))
    }

    /// Serializes a JSON artifact with every secret redacted and writes it
    ///
    /// # Arguments
    ///
    /// * `name` - The artifact's name
    /// * `value` - The artifact to redact and write
    fn write_json(&self, name: &str, mut value: Value) -> Result<Option<PathBuf>, Error> {
        // skip redacting and serializing when recording is disabled
        if !self.enabled() {
            return Ok(None);
        }
        // scrub secret fields and registered secrets
        redact_value(&mut value, &self.secrets);
        // write the artifact as pretty JSON for reviewers
        let contents = serde_json::to_string_pretty(&value)?;
        self.write(&format!("{}.json", sanitize(name)), &contents)
    }

    /// Records an API request and its response, returning the artifact's path when written
    ///
    /// # Arguments
    ///
    /// * `name` - The artifact's name
    /// * `request` - The request that was sent
    /// * `response` - The response that came back
    pub fn record_exchange(
        &self,
        name: &str,
        request: &RecordedRequest,
        response: &RecordedResponse,
    ) -> Result<Option<PathBuf>, Error> {
        // tag the exchange so the bundle script can tell artifacts apart
        let value = json!({"kind": "exchange", "request": request, "response": response});
        self.write_json(name, value)
    }

    /// Records a read-back proving what was stored, returning the artifact's path when written
    ///
    /// # Arguments
    ///
    /// * `name` - The artifact's name
    /// * `value` - The value that was read back
    pub fn record_read_back<T: Serialize>(
        &self,
        name: &str,
        value: &T,
    ) -> Result<Option<PathBuf>, Error> {
        // tag the read-back so the bundle script can tell artifacts apart
        let value = json!({"kind": "read_back", "value": serde_json::to_value(value)?});
        self.write_json(name, value)
    }

    /// Records log text, returning the artifact's path when written
    ///
    /// # Arguments
    ///
    /// * `name` - The artifact's name
    /// * `text` - The log text to scrub and write
    pub fn record_log(&self, name: &str, text: &str) -> Result<Option<PathBuf>, Error> {
        // skip redacting when recording is disabled
        if !self.enabled() {
            return Ok(None);
        }
        // scrub registered secrets and auth scheme credentials out of the log
        let scrubbed = redact_text(text, &self.secrets);
        self.write(&format!("{}.log", sanitize(name)), &scrubbed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;
    /// Builds a unique scratch root for one test
    fn scratch_root() -> PathBuf {
        // place the scratch directory under the system temp dir with a unique name
        std::env::temp_dir().join(format!("thorium-evidence-{}", uuid::Uuid::new_v4()))
    }
    /// Reads a recorded JSON artifact back
    ///
    /// # Arguments
    ///
    /// * `path` - The path of the artifact to read
    fn read_json(path: &std::path::Path) -> serde_json::Value {
        // read and parse the artifact
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }
    /// Exchanges are written into the claim's directory with tokens and keys redacted
    #[test]
    fn records_redacted_exchange() {
        // build a recorder rooted in a scratch directory
        let root = scratch_root();
        let mut recorder = EvidenceRecorder::new(Some(root.clone()), "admin-routes");
        recorder.register_secret("hunter2-secret-value");
        // build a request carrying an auth header, a secret field, and a registered secret
        let mut request = RecordedRequest::new("POST", "/api/models/credentials/");
        request.header("Authorization", "token abcdef123456");
        request.header("Content-Type", "application/json");
        request.body(json!({"name": "cred", "api_key": "sk-123", "nested": {"secret": "s", "note": "has hunter2-secret-value in it"}}));
        // build a response with a token field and a fingerprint that must survive
        let response = RecordedResponse::new(
            200,
            json!({"token": "abc", "token_expiration": "2026-01-01", "fingerprint": "sha256:aa", "result_key": "wf"}),
        );
        let path = recorder
            .record_exchange("create credential", &request, &response)
            .unwrap()
            .unwrap();
        // the artifact lands in the claim's directory under a sanitized name
        assert_eq!(
            path,
            root.join("admin-routes").join("create_credential.json")
        );
        let value = read_json(&path);
        assert_eq!(value["kind"], "exchange");
        assert_eq!(value["request"]["method"], "POST");
        assert_eq!(value["request"]["path"], "/api/models/credentials/");
        assert_eq!(value["request"]["headers"]["authorization"], REDACTED);
        assert_eq!(
            value["request"]["headers"]["content-type"],
            "application/json"
        );
        assert_eq!(value["request"]["body"]["name"], "cred");
        assert_eq!(value["request"]["body"]["api_key"], REDACTED);
        assert_eq!(value["request"]["body"]["nested"]["secret"], REDACTED);
        assert_eq!(
            value["request"]["body"]["nested"]["note"],
            format!("has {REDACTED} in it")
        );
        assert_eq!(value["response"]["status"], 200);
        assert_eq!(value["response"]["body"]["token"], REDACTED);
        assert_eq!(value["response"]["body"]["token_expiration"], "2026-01-01");
        assert_eq!(value["response"]["body"]["fingerprint"], "sha256:aa");
        assert_eq!(value["response"]["body"]["result_key"], "wf");
        // no secret survives anywhere in the written file
        let raw = std::fs::read_to_string(&path).unwrap();
        for secret in ["abcdef123456", "sk-123", "hunter2-secret-value", "\"abc\""] {
            assert!(!raw.contains(secret), "{secret} leaked into {raw}");
        }
        std::fs::remove_dir_all(root).unwrap();
    }
    /// Read-backs and logs are written redacted into the claim's directory
    #[test]
    fn records_redacted_read_back_and_log() {
        // build a recorder rooted in a scratch directory
        let root = scratch_root();
        let mut recorder = EvidenceRecorder::new(Some(root.clone()), "user-reads");
        recorder.register_secret("registered-secret");
        // record a read-back holding a key and a list of users with tokens
        let read_back = json!({"users": [{"username": "a", "token": "t1"}], "secret_key": "k", "kind": "Model"});
        let path = recorder
            .record_read_back("users", &read_back)
            .unwrap()
            .unwrap();
        assert_eq!(path, root.join("user-reads").join("users.json"));
        let value = read_json(&path);
        assert_eq!(value["kind"], "read_back");
        assert_eq!(value["value"]["users"][0]["username"], "a");
        assert_eq!(value["value"]["users"][0]["token"], REDACTED);
        assert_eq!(value["value"]["secret_key"], REDACTED);
        assert_eq!(value["value"]["kind"], "Model");
        // record a log with an auth header line and a registered secret
        let log = "GET /api/users/whoami\nauthorization: Bearer eyJabc.def\nused registered-secret here\n";
        let path = recorder.record_log("api", log).unwrap().unwrap();
        assert_eq!(path, root.join("user-reads").join("api.log"));
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("GET /api/users/whoami"));
        assert!(!raw.contains("eyJabc.def"));
        assert!(!raw.contains("registered-secret"));
        assert!(raw.contains(&format!("authorization: Bearer {REDACTED}")));
        std::fs::remove_dir_all(root).unwrap();
    }
    /// Secrets written as JSON fields, `key=value` pairs, or headers are scrubbed from logs
    #[test]
    fn log_field_forms_are_redacted() {
        // build a recorder rooted in a scratch directory
        let root = scratch_root();
        let recorder = EvidenceRecorder::new(Some(root.clone()), "log-forms");
        // record a log holding secrets in every field form
        let log = concat!(
            "{\"token\": \"t1\", \"note\": \"kept\"}\n",
            "GET /api/files?token=t2&limit=10\n",
            "x-api-key: t3\n",
            "password: t4\n",
            "\"api_key\":\"sk-t5\"\n",
            "\"password\": \"two words-t6\"\n",
            "authorization: Bearer t7\n",
        );
        let path = recorder.record_log("forms", log).unwrap().unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        // no secret survives, while the names and other values do
        for secret in ["t1", "t2", "t3", "t4", "sk-t5", "t6", "two words", "t7"] {
            assert!(!raw.contains(secret), "{secret} leaked into {raw}");
        }
        for kept in [
            "\"note\": \"kept\"",
            "limit=10",
            "x-api-key: ",
            "password: ",
            "Bearer ",
        ] {
            assert!(raw.contains(kept), "{kept} is missing from {raw}");
        }
        std::fs::remove_dir_all(root).unwrap();
    }
    /// Values under a secrets object and credential names are redacted, while object-valued
    /// `key` fields like an event's output key are kept
    #[test]
    fn secret_shapes_are_redacted() {
        // build a recorder rooted in a scratch directory
        let root = scratch_root();
        let recorder = EvidenceRecorder::new(Some(root.clone()), "secret-shapes");
        // record a credential read-back, an event read-back, and a bootstrap key
        let read_back = json!({
            "credential": {"secrets": {"primary": "sk-x", "fingerprint": "ab"}},
            "env": {"GOOGLE_APPLICATION_CREDENTIALS": "creds-y"},
            "event": {"data": {"key": {"Sample": "sha"}, "group": "g"}},
            "bootstrap": {"key": "boot-z"},
            "tag": {"key": "Family", "value": "kept"},
        });
        let path = recorder
            .record_read_back("shapes", &read_back)
            .unwrap()
            .unwrap();
        let value = read_json(&path)["value"].clone();
        assert_eq!(value["credential"]["secrets"]["primary"], REDACTED);
        assert_eq!(value["credential"]["secrets"]["fingerprint"], "ab");
        assert_eq!(value["env"]["GOOGLE_APPLICATION_CREDENTIALS"], REDACTED);
        assert_eq!(value["event"]["data"]["key"], json!({"Sample": "sha"}));
        assert_eq!(value["bootstrap"]["key"], REDACTED);
        assert_eq!(value["tag"], json!({"key": "Family", "value": "kept"}));
        std::fs::remove_dir_all(root).unwrap();
    }
    /// A registered secret that contains a shorter registered secret is still fully scrubbed
    #[test]
    fn longer_secrets_are_replaced_first() {
        // build a recorder whose secrets overlap
        let root = scratch_root();
        let mut recorder = EvidenceRecorder::new(Some(root.clone()), "overlap");
        recorder.register_secret("abc");
        recorder.register_secret("abcdef-token");
        // record a log and a read-back holding the longer secret
        let path = recorder
            .record_log("overlap", "value abcdef-token here")
            .unwrap()
            .unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(raw, format!("value {REDACTED} here"));
        let path = recorder
            .record_read_back("overlap", &json!({"note": "abcdef-token"}))
            .unwrap()
            .unwrap();
        assert_eq!(read_json(&path)["value"]["note"], REDACTED);
        std::fs::remove_dir_all(root).unwrap();
    }
    /// Without an evidence directory nothing is written
    #[test]
    fn disabled_writes_nothing() {
        // build a disabled recorder
        let recorder = EvidenceRecorder::new(None, "admin-routes");
        assert!(!recorder.enabled());
        assert!(recorder.claim_dir().is_none());
        // every record call succeeds without producing a path
        let request = RecordedRequest::new("GET", "/api/basic/identify");
        let response = RecordedResponse::new(200, json!({"token": "abc"}));
        assert!(
            recorder
                .record_exchange("identify", &request, &response)
                .unwrap()
                .is_none()
        );
        assert!(
            recorder
                .record_read_back("identify", &json!({}))
                .unwrap()
                .is_none()
        );
        assert!(recorder.record_log("identify", "text").unwrap().is_none());
    }
    /// The recorder reads its root from `THORIUM_EVIDENCE_DIR`
    #[test]
    fn root_from_env_value() {
        // an unset or empty variable disables the recorder
        assert!(
            EvidenceRecorder::from_env_value(None, "admin-routes")
                .claim_dir()
                .is_none()
        );
        assert!(
            EvidenceRecorder::from_env_value(Some(String::new()), "admin-routes")
                .claim_dir()
                .is_none()
        );
        // a set variable roots the claim directory there
        let recorder = EvidenceRecorder::from_env_value(Some("/tmp/ev".to_owned()), "job-reads");
        assert_eq!(
            recorder.claim_dir(),
            Some(std::path::Path::new("/tmp/ev/job-reads"))
        );
        assert_eq!(EVIDENCE_DIR_ENV, "THORIUM_EVIDENCE_DIR");
    }
    /// Claim IDs and artifact names can't escape the evidence directory
    #[test]
    fn names_are_sanitized() {
        // build a recorder whose claim tries to traverse upward
        let root = scratch_root();
        let recorder = EvidenceRecorder::new(Some(root.clone()), "../escape");
        let path = recorder
            .record_read_back("../../etc/passwd", &json!({"a": 1}))
            .unwrap()
            .unwrap();
        // the written file stays inside the root
        assert!(path.starts_with(&root));
        assert!(
            !path
                .components()
                .any(|c| c == std::path::Component::ParentDir)
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
