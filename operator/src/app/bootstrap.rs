//! Setup of the backend roles and users a Thorium cluster needs
//!
//! Each step is idempotent. The privileged steps (Scylla role, external Elastic role and
//! user, and the admin user) only run when their inputs change, while Thorium's own Elastic
//! credentials and privileges are verified on every reconcile. The search streamer creates
//! Thorium's Elastic indexes, so nothing here creates them.
//!
//! The admin Secrets the privileged steps read may be deleted once setup is done: each step
//! first checks whether its work is already done and only reads its Secret when it isn't.

use chrono::{DateTime, Utc};
use elasticsearch::Elasticsearch;
use elasticsearch::auth::Credentials as ElasticAuth;
use elasticsearch::http::Url;
use elasticsearch::http::response::Response;
use elasticsearch::http::transport::{SingleNodeConnectionPool, TransportBuilder};
use elasticsearch::indices::{IndicesExistsParts, IndicesGetMappingParts};
use elasticsearch::security::{
    SecurityHasPrivilegesParts, SecurityPutRoleParts, SecurityPutUserParts,
};
use reqwest::StatusCode;
use scylla::client::session::Session;
use scylla::client::session_builder::{GenericSessionBuilder, SessionBuilder};
use scylla::errors::{
    ConnectionError, ConnectionPoolError, ConnectionSetupRequestErrorKind, DbError, MetadataError,
    NewSessionError,
};
use sha2::{Digest, Sha256};
use std::time::Duration;
use thorium::conf::Elastic;
use thorium::models::{ElasticIndex, ScrubbedUser, UserCreate, UserRole};
use thorium::{Error, client};

use super::helpers::{CheckError, error_chain};
use crate::k8s::clusters::ClusterMeta;
use crate::k8s::crds::{
    ClusterPhase, SecretCredentials, ThoriumBootstrap, ThoriumCluster, ThoriumClusterStatus,
};
use crate::k8s::secrets;

/// How long to wait when connecting to a backend
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// How long to wait for a Scylla session to connect, log in, and fetch its metadata
const SCYLLA_SESSION_TIMEOUT: Duration = Duration::from_secs(45);

/// The users the operator creates for itself, which never count as the bootstrap admin
const SERVICE_USERS: [&str; 3] = ["thorium-operator", "thorium", "thorium-kaboom"];

/// The result of a privileged bootstrap step that may need an admin Secret
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepOutcome {
    /// The step completed or had nothing left to do
    Done,
    /// The step had work to do but the Secret it needs doesn't exist
    MissingSecret(String),
}

/// A username and password read from a Secret
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    /// The username to authenticate as
    pub username: String,
    /// The password to authenticate with
    pub password: String,
}

impl std::fmt::Debug for Credentials {
    /// Format these credentials without revealing the password
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // print the username but mask the password
        f.debug_struct("Credentials")
            .field("username", &self.username)
            .field("password", &"***")
            .finish()
    }
}

/// Read a username and password from a Secret in the `ThoriumCluster`'s namespace
///
/// Returns None when the Secret doesn't exist so callers can decide whether they need it.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `kind` - What these credentials are for (e.g. "bootstrap admin") for errors
/// * `creds` - The Secret and keys holding the credentials
/// * `default_username` - The username to use if the credentials don't set one
async fn read_credentials(
    meta: &ClusterMeta,
    kind: &str,
    creds: &SecretCredentials,
    default_username: Option<&str>,
) -> Result<Option<Credentials>, Error> {
    // a deleted Secret is only an error for callers that actually need it
    if meta
        .secret_api
        .get_metadata_opt(&creds.name)
        .await?
        .is_none()
    {
        return Ok(None);
    }
    // get the username from the secret, a literal, or our default in that order
    let username = match (&creds.username_key, &creds.username, default_username) {
        (Some(key), _, _) => {
            secrets::get_secret_key(&meta.secret_api, kind, &creds.name, key).await?
        }
        (None, Some(username), _) => username.clone(),
        (None, None, Some(default)) => default.to_owned(),
        (None, None, None) => {
            return Err(Error::new(format!(
                "Credentials from secret {} need a username or username_key",
                creds.name
            )));
        }
    };
    // get the password from the secret
    let password =
        secrets::get_secret_key(&meta.secret_api, kind, &creds.name, &creds.password_key).await?;
    Ok(Some(Credentials { username, password }))
}

/// Build the error for a bootstrap step that needs a Secret that doesn't exist
///
/// # Arguments
///
/// * `field` - The bootstrap field naming the Secret (e.g. `bootstrap.scylla.admin_secret`)
/// * `name` - The name of the missing Secret
/// * `reason` - Why the step needs the Secret
fn missing_secret(field: &str, name: &str, reason: &str) -> String {
    format!("{field} {name} is missing and {reason}")
}

/// The version marker hashed for a referenced Secret that doesn't exist
///
/// # Arguments
///
/// * `name` - The name of the missing Secret
fn absent_marker(name: &str) -> String {
    format!("<absent:{name}>")
}

/// Hash every input of the privileged bootstrap steps
///
/// This covers the bootstrap spec, the resource versions of the Secrets it references, and
/// the rendered Thorium config (which holds the credentials Thorium's roles are created with).
/// A referenced Secret that doesn't exist contributes a fixed marker instead of failing, so a
/// site can delete its admin Secrets after setup; any other error reading a Secret is returned.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `config_hash` - The sha256 of the rendered thorium.yml
pub async fn input_hash(meta: &ClusterMeta, config_hash: &str) -> Result<String, Error> {
    // get the current version of every bootstrap secret
    let mut versions = Vec::new();
    for name in meta.cluster.bootstrap_secret_names() {
        // get just this secret's metadata, or a marker if it doesn't exist
        let version = match meta.secret_api.get_metadata_opt(name).await? {
            Some(secret) => secret.metadata.resource_version.unwrap_or_default(),
            None => absent_marker(name),
        };
        // track this secret's name and version
        versions.push((name, version));
    }
    // hash the bootstrap spec, config, and secret versions together
    hash_inputs(meta.cluster.spec.bootstrap.as_ref(), config_hash, &versions)
}

/// Hash the bootstrap spec, rendered config, and bootstrap Secret versions
///
/// # Arguments
///
/// * `bootstrap` - The bootstrap spec
/// * `config_hash` - The sha256 of the rendered thorium.yml
/// * `versions` - The name and resource version of each bootstrap Secret
fn hash_inputs(
    bootstrap: Option<&ThoriumBootstrap>,
    config_hash: &str,
    versions: &[(&str, String)],
) -> Result<String, Error> {
    // hash each part after its length so bytes can't move between parts unnoticed
    let mut hasher = Sha256::new();
    let mut update = |part: &[u8]| {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part);
    };
    // start hashing with the bootstrap spec
    update(&serde_json::to_vec(&bootstrap)?);
    // hash the rendered config so credential changes rerun the bootstrap
    update(config_hash.as_bytes());
    // hash the name and version of every bootstrap secret
    for (name, version) in versions {
        update(name.as_bytes());
        update(version.as_bytes());
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Check whether the privileged bootstrap steps need to run
///
/// # Arguments
///
/// * `cluster` - The `ThoriumCluster` being reconciled
/// * `hash` - The hash of the current bootstrap inputs
pub fn is_due(cluster: &ThoriumCluster, hash: &str) -> bool {
    // run the bootstrap unless it last completed with these same inputs
    cluster
        .status
        .as_ref()
        .and_then(|status| status.bootstrap_hash.as_deref())
        != Some(hash)
}

/// Why a Scylla session couldn't be opened
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ScyllaConnectError {
    /// Scylla rejected the credentials, with a description of the failure
    Auth(String),
    /// Scylla couldn't be reached in time or failed for another reason, with a description
    Unavailable(String),
}

impl ScyllaConnectError {
    /// Classify a failed Scylla session by whether Scylla rejected its credentials
    ///
    /// # Arguments
    ///
    /// * `error` - The error the session failed with
    /// * `who` - Who the session connected as, for the description
    fn classify(error: &NewSessionError, who: &str) -> Self {
        // describe the error with every cause the driver reported
        let chain = error_chain(error);
        // only a rejected login is an authentication failure
        if is_scylla_auth_error(error) {
            Self::Auth(format!("Scylla rejected the credentials of {who}: {chain}"))
        } else {
            Self::Unavailable(format!(
                "Waiting for Scylla to accept connections as {who}: {chain}"
            ))
        }
    }
}

impl From<ScyllaConnectError> for Error {
    /// Turn a failed Scylla session into a Thorium error with its description
    ///
    /// # Arguments
    ///
    /// * `error` - The failed Scylla session
    fn from(error: ScyllaConnectError) -> Self {
        // both kinds already describe themselves fully
        match error {
            ScyllaConnectError::Auth(details) | ScyllaConnectError::Unavailable(details) => {
                Error::new(details)
            }
        }
    }
}

impl From<ScyllaConnectError> for CheckError {
    /// Wait on a Scylla that isn't accepting connections and fail on rejected credentials
    ///
    /// # Arguments
    ///
    /// * `error` - The failed Scylla session
    fn from(error: ScyllaConnectError) -> Self {
        match error {
            // a reachable Scylla rejecting our credentials needs attention
            ScyllaConnectError::Auth(details) => CheckError::Failed(Error::new(details)),
            // a Scylla that isn't up yet will accept connections on its own
            ScyllaConnectError::Unavailable(details) => CheckError::Waiting(details),
        }
    }
}

/// Check whether a failed Scylla session was rejected for its credentials
///
/// The driver reports a failed login as the last error of the control connection's pool when
/// the initial metadata fetch fails. Those nested errors aren't exposed through `source()`, so
/// they are matched directly.
///
/// # Arguments
///
/// * `error` - The error the session failed with
fn is_scylla_auth_error(error: &NewSessionError) -> bool {
    // get the last error of the control connection behind a failed metadata fetch
    let NewSessionError::MetadataError(MetadataError::ConnectionPoolError(
        ConnectionPoolError::Broken {
            last_connection_error: ConnectionError::ConnectionSetupRequestError(setup),
        },
    )) = error
    else {
        return false;
    };
    // Scylla answered the login with an authentication error or demanded credentials
    matches!(
        setup.error,
        ConnectionSetupRequestErrorKind::DbError(DbError::AuthenticationError, _)
            | ConnectionSetupRequestErrorKind::MissingAuthentication
    )
}

/// Open a Scylla session, bounded by an overall timeout
///
/// # Arguments
///
/// * `builder` - The session builder with any credentials already set
/// * `nodes` - The Scylla nodes to connect to
/// * `who` - Who the session connects as, for errors
async fn scylla_session(
    builder: SessionBuilder,
    nodes: &[String],
    who: &str,
) -> Result<Session, ScyllaConnectError> {
    // add our nodes with a bound on each connection attempt
    let builder = nodes.iter().fold(
        builder.connection_timeout(CONNECT_TIMEOUT),
        GenericSessionBuilder::known_node,
    );
    // bound connecting, logging in, and fetching metadata as a whole
    match tokio::time::timeout(SCYLLA_SESSION_TIMEOUT, builder.build()).await {
        Ok(Ok(session)) => Ok(session),
        Ok(Err(error)) => Err(ScyllaConnectError::classify(&error, who)),
        Err(_) => Err(ScyllaConnectError::Unavailable(format!(
            "Waiting for Scylla to accept connections as {who}: no session was established \
             within {}s",
            SCYLLA_SESSION_TIMEOUT.as_secs()
        ))),
    }
}

/// Connect to Scylla as a specific user
///
/// # Arguments
///
/// * `nodes` - The Scylla nodes to connect to
/// * `username` - The user to connect as
/// * `password` - The password for this user
pub(crate) async fn scylla_connect(
    nodes: &[String],
    username: &str,
    password: &str,
) -> Result<Session, ScyllaConnectError> {
    // build a session for this user
    let builder = SessionBuilder::new().user(username, password);
    scylla_session(builder, nodes, username).await
}

/// Connect to Scylla without credentials
///
/// # Arguments
///
/// * `nodes` - The Scylla nodes to connect to
pub(crate) async fn scylla_connect_anonymous(
    nodes: &[String],
) -> Result<Session, ScyllaConnectError> {
    // build a session without a user
    scylla_session(SessionBuilder::new(), nodes, "an anonymous user").await
}

/// Run a single CQL statement
///
/// # Arguments
///
/// * `session` - The Scylla session to use
/// * `statement` - The CQL statement to run
/// * `description` - A description of this statement for errors
async fn scylla_execute(
    session: &Session,
    statement: String,
    description: &str,
) -> Result<(), Error> {
    // run this statement
    session
        .query_unpaged(statement, &[])
        .await
        .map_err(|error| Error::new(format!("Failed to {description} in Scylla: {error}")))?;
    Ok(())
}

/// Quote a CQL identifier
///
/// # Arguments
///
/// * `identifier` - The identifier to quote
fn cql_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

/// Quote a CQL string literal
///
/// # Arguments
///
/// * `literal` - The literal to quote
fn cql_literal(literal: &str) -> String {
    format!("'{}'", literal.replace('\'', "''"))
}

/// Connect to Scylla as the superuser that creates Thorium's role
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `admin_secret` - The superuser credentials, or None to use cassandra/cassandra
async fn scylla_admin_connect(
    meta: &ClusterMeta,
    admin_secret: Option<&SecretCredentials>,
) -> Result<(Session, String), CheckError> {
    // get the nodes to connect to
    let nodes = &meta.conf.scylla.nodes;
    // connect with the configured superuser if one was given
    if let Some(creds) = admin_secret {
        // read the superuser's credentials, which are required since Thorium's role can't log in
        let Some(admin) = read_credentials(meta, "Scylla admin", creds, Some("cassandra")).await?
        else {
            return Err(Error::new(missing_secret(
                "bootstrap.scylla.admin_secret",
                &creds.name,
                "Thorium's role can't log in",
            ))
            .into());
        };
        // connect as this superuser
        let session = scylla_connect(nodes, &admin.username, &admin.password).await?;
        return Ok((session, admin.username));
    }
    // fall back to the default superuser, which only exists on a fresh Scylla
    let session = match scylla_connect(nodes, "cassandra", "cassandra").await {
        Ok(session) => session,
        // only a rejected login means a superuser Secret is needed
        Err(ScyllaConnectError::Auth(details)) => {
            return Err(Error::new(format!(
                "Could not log in to Scylla with the default cassandra/cassandra superuser, so \
                 Thorium's Scylla role can't be created. Set bootstrap.scylla.admin_secret to a \
                 Secret holding Scylla superuser credentials: {details}"
            ))
            .into());
        }
        // a Scylla that isn't accepting connections is waited on
        Err(error) => return Err(error.into()),
    };
    Ok((session, "cassandra".to_owned()))
}

/// Ensure Thorium's Scylla role exists
///
/// This runs before the API is deployed since the API needs this role to create its
/// keyspace. It does nothing unless `bootstrap.scylla` is set, and the admin Secret is only
/// read when Thorium's own role can't log in. A Scylla that isn't accepting connections yet
/// is reported as [`CheckError::Waiting`].
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
pub async fn scylla(meta: &ClusterMeta) -> Result<(), CheckError> {
    // skip this step unless scylla bootstrapping is enabled
    let Some(bootstrap) = meta
        .cluster
        .spec
        .bootstrap
        .as_ref()
        .and_then(|bootstrap| bootstrap.scylla.as_ref())
    else {
        return Ok(());
    };
    // get the credentials Thorium uses for Scylla
    let Some(auth) = &meta.conf.scylla.auth else {
        return Err(Error::new(
            "Scylla bootstrap requires scylla.auth to be set in the Thorium config",
        )
        .into());
    };
    // get the nodes to connect to
    let nodes = &meta.conf.scylla.nodes;
    // check if Thorium's role already works
    match scylla_connect(nodes, &auth.username, &auth.password).await {
        Ok(session) => {
            println!("Scylla role {} already exists", auth.username);
            // drop the default role if a previous run created our role but didn't finish
            if bootstrap.drop_default_role && auth.username != "cassandra" {
                scylla_execute(
                    &session,
                    "DROP ROLE IF EXISTS cassandra".to_owned(),
                    "drop the cassandra role",
                )
                .await?;
            }
            return Ok(());
        }
        // a rejected login means Thorium's role still has to be created
        Err(ScyllaConnectError::Auth(details)) => println!("{details}"),
        // a Scylla that isn't accepting connections can't be bootstrapped yet
        Err(error) => return Err(error.into()),
    }
    // connect as the superuser that creates our role
    let (session, admin_username) =
        scylla_admin_connect(meta, bootstrap.admin_secret.as_ref()).await?;
    // quote our role name and password for use in role statements that can't bind values
    let role = cql_identifier(&auth.username);
    let password = cql_literal(&auth.password);
    // create Thorium's role
    println!("Creating Scylla role {}", auth.username);
    scylla_execute(
        &session,
        format!("CREATE ROLE IF NOT EXISTS {role} WITH PASSWORD = {password} AND LOGIN = true AND SUPERUSER = true"),
        "create Thorium's role",
    )
    .await?;
    // make sure an existing role's password matches our config
    scylla_execute(
        &session,
        format!("ALTER ROLE {role} WITH PASSWORD = {password}"),
        "set Thorium's role password",
    )
    .await?;
    // drop the default role if requested and we used it
    if bootstrap.drop_default_role && admin_username == "cassandra" && auth.username != "cassandra"
    {
        // reconnect as Thorium's role since a role can't drop itself
        let thorium_session = scylla_connect(nodes, &auth.username, &auth.password).await?;
        // drop the default role
        println!("Dropping the default Scylla cassandra role");
        scylla_execute(
            &thorium_session,
            "DROP ROLE IF EXISTS cassandra".to_owned(),
            "drop the cassandra role",
        )
        .await?;
    }
    Ok(())
}

/// Build an Elastic client that validates certificates exactly like the Thorium API
///
/// # Arguments
///
/// * `conf` - The Elastic config for this cluster
/// * `creds` - The credentials to authenticate with
async fn elastic_client(conf: &Elastic, creds: &Credentials) -> Result<Elasticsearch, Error> {
    // parse the node to connect to
    let url = Url::parse(&conf.node)?;
    // get the cert validation the API uses
    let validation = conf.try_cert_validation().await?;
    // build our transport for this node
    let transport = TransportBuilder::new(SingleNodeConnectionPool::new(url))
        .auth(ElasticAuth::Basic(
            creds.username.clone(),
            creds.password.clone(),
        ))
        .cert_validation(validation)
        .timeout(CONNECT_TIMEOUT)
        .build()?;
    Ok(Elasticsearch::new(transport))
}

/// Check whether an Elastic request failed because Elastic couldn't be reached
///
/// A refused or timed out connection means Elastic isn't up yet, while a certificate that
/// fails validation is a config problem that waiting won't fix.
///
/// # Arguments
///
/// * `error` - The error the request failed with
fn elastic_unreachable(error: &elasticsearch::Error) -> bool {
    // a certificate that fails validation won't fix itself
    if error_chain(error).to_lowercase().contains("certificate") {
        return false;
    }
    // a request that timed out means Elastic didn't answer in time
    if error.is_timeout() {
        return true;
    }
    // look for a connection that couldn't be opened anywhere in the causes
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        if let Some(http) = cause.downcast_ref::<reqwest::Error>()
            && (http.is_connect() || http.is_timeout())
        {
            return true;
        }
        source = cause.source();
    }
    false
}

/// Check whether an Elastic response status means Elastic isn't ready to serve requests yet
///
/// # Arguments
///
/// * `status` - The status Elastic or a proxy in front of it answered with
fn elastic_unavailable(status: StatusCode) -> bool {
    // a proxy without a ready backend or an Elastic still starting answers with these
    matches!(
        status,
        StatusCode::BAD_GATEWAY | StatusCode::SERVICE_UNAVAILABLE | StatusCode::GATEWAY_TIMEOUT
    )
}

/// Classify an Elastic request that couldn't be sent or got no response
///
/// The description includes every cause, such as a TLS hostname mismatch or a refused
/// connection.
///
/// # Arguments
///
/// * `node` - The Elastic node the request was sent to
/// * `description` - A description of the request
/// * `error` - The error the request failed with
fn elastic_send_failure(node: &str, description: &str, error: &elasticsearch::Error) -> CheckError {
    // describe the error with every cause
    let chain = error_chain(error);
    // only an Elastic that can't be reached yet is waited on
    if elastic_unreachable(error) {
        CheckError::Waiting(format!(
            "Waiting for Elastic {node} to accept connections: failed to {description}: {chain}"
        ))
    } else {
        CheckError::Failed(Error::new(format!(
            "Failed to {description} at Elastic {node}: {chain}"
        )))
    }
}

/// Build the error for an Elastic request that couldn't be sent or got no response
///
/// # Arguments
///
/// * `conf` - The Elastic config for this cluster
/// * `description` - A description of the request
fn elastic_send_error(
    conf: &Elastic,
    description: &str,
) -> impl FnOnce(elasticsearch::Error) -> CheckError {
    // capture the node and description for the error
    let node = conf.node.clone();
    let description = description.to_owned();
    move |error| elastic_send_failure(&node, &description, &error)
}

/// Classify an Elastic response that answered a check with an unexpected status
///
/// # Arguments
///
/// * `status` - The status Elastic answered with
/// * `details` - A description of the failed check including the response
fn elastic_status_failure(status: StatusCode, details: String) -> CheckError {
    // an Elastic that isn't ready to serve requests yet is waited on
    if elastic_unavailable(status) {
        CheckError::Waiting(format!(
            "Waiting for Elastic to become available: {details}"
        ))
    } else {
        CheckError::Failed(Error::new(details))
    }
}

/// Return an error describing a failed Elastic request unless it succeeded
///
/// # Arguments
///
/// * `response` - The response to check
/// * `description` - A description of the request for errors
async fn elastic_check(response: Response, description: &str) -> Result<(), Error> {
    // a successful request needs no further handling
    let status = response.status_code();
    if status.is_success() {
        return Ok(());
    }
    // include the response body in our error
    let body = response.text().await.unwrap_or_default();
    Err(Error::new(format!(
        "Failed to {description}: {status} {body}"
    )))
}

/// Get the index patterns Thorium's Elastic role needs access to
///
/// This is every `thorium*` index plus any configured index outside that pattern.
///
/// # Arguments
///
/// * `conf` - The Elastic config for this cluster
fn elastic_index_patterns(conf: &Elastic) -> Vec<String> {
    // always grant access to every thorium index
    let mut patterns = vec!["thorium*".to_owned()];
    // add any configured index outside of that pattern
    patterns.extend(
        elastic_indexes()
            .iter()
            .map(|index| index.full_name(conf))
            .filter(|name| !name.starts_with("thorium"))
            .map(str::to_owned),
    );
    patterns
}

/// The Elastic indexes the Thorium API uses
fn elastic_indexes() -> [ElasticIndex; 4] {
    [
        ElasticIndex::SampleResults,
        ElasticIndex::RepoResults,
        ElasticIndex::SampleTags,
        ElasticIndex::RepoTags,
    ]
}

/// Build the body of a has-privileges request for the indexes Thorium's role needs
///
/// # Arguments
///
/// * `conf` - The Elastic config for this cluster
fn elastic_privileges_request(conf: &Elastic) -> serde_json::Value {
    serde_json::json!({
        "index": [{"names": elastic_index_patterns(conf), "privileges": ["all"]}]
    })
}

/// Check whether a has-privileges response shows Thorium already has every privilege it needs
///
/// Any failed request (such as a 401 for a missing user or a wrong password) means the role
/// and user still need to be created or updated.
///
/// # Arguments
///
/// * `status` - The status of the has-privileges request
/// * `body` - The body of the response, if it could be parsed
fn elastic_privileges_ok(status: StatusCode, body: Option<&serde_json::Value>) -> bool {
    // only a successful response reporting every privilege counts
    status.is_success()
        && body
            .and_then(|body| body.get("has_all_requested"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
}

/// Check whether Thorium's own Elastic credentials authenticate with the privileges it needs
///
/// # Arguments
///
/// * `conf` - The Elastic config for this cluster
async fn elastic_identity_ready(conf: &Elastic) -> Result<bool, CheckError> {
    // authenticate as Thorium's own user
    let creds = Credentials {
        username: conf.username.clone(),
        password: conf.password.clone(),
    };
    let client = elastic_client(conf, &creds).await?;
    // ask Elastic whether this user holds every privilege on Thorium's indexes
    let response = client
        .security()
        .has_privileges(SecurityHasPrivilegesParts::None)
        .body(elastic_privileges_request(conf))
        .send()
        .await
        .map_err(elastic_send_error(
            conf,
            "check the privileges of Thorium's Elastic user",
        ))?;
    // get the status before the body is consumed
    let status = response.status_code();
    // an Elastic that isn't ready yet can't tell us whether the bootstrap is needed
    if elastic_unavailable(status) {
        let error = elastic_error(response).await;
        return Err(CheckError::Waiting(format!(
            "Waiting for Elastic {} to become available: {error}",
            conf.node
        )));
    }
    let body = response.json::<serde_json::Value>().await.ok();
    Ok(elastic_privileges_ok(status, body.as_ref()))
}

/// Ensure Thorium's role and user exist in an externally managed Elasticsearch
///
/// This runs before the API is deployed and does nothing unless `bootstrap.elastic` is
/// set. An Elasticsearch managed by the Thorium chart gets this role and user from ECK's
/// file realm instead. When Thorium's own credentials already authenticate with every
/// privilege it needs nothing is changed and the admin Secret is never read.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
pub async fn elastic_identity(meta: &ClusterMeta) -> Result<StepOutcome, CheckError> {
    // skip this step unless external elastic bootstrapping is enabled
    let Some(bootstrap) = meta
        .cluster
        .spec
        .bootstrap
        .as_ref()
        .and_then(|bootstrap| bootstrap.elastic.as_ref())
    else {
        return Ok(StepOutcome::Done);
    };
    // get the Elastic config for this cluster
    let conf = &meta.conf.elastic;
    // skip the superuser entirely when Thorium's role and user are already in place
    if elastic_identity_ready(conf).await? {
        println!(
            "Elastic user {} already has the privileges it needs",
            conf.username
        );
        return Ok(StepOutcome::Done);
    }
    // get the superuser credentials to configure Elastic with
    let Some(admin) = read_credentials(
        meta,
        "Elastic admin",
        &bootstrap.admin_secret,
        Some("elastic"),
    )
    .await?
    else {
        return Ok(StepOutcome::MissingSecret(missing_secret(
            "bootstrap.elastic.admin_secret",
            &bootstrap.admin_secret.name,
            "Thorium's Elastic user can't authenticate with the privileges it needs",
        )));
    };
    // build a client for Elastic as the superuser
    let client = elastic_client(conf, &admin).await?;
    // create or update Thorium's role
    println!("Ensuring Elastic role {}", conf.username);
    let role = serde_json::json!({
        "indices": [{"names": elastic_index_patterns(conf), "privileges": ["all"]}]
    });
    let response = client
        .security()
        .put_role(SecurityPutRoleParts::Name(&conf.username))
        .body(role)
        .send()
        .await
        .map_err(elastic_send_error(conf, "create Thorium's Elastic role"))?;
    elastic_check(response, &format!("create Elastic role {}", conf.username)).await?;
    // create or update Thorium's user so its password always matches our config
    println!("Ensuring Elastic user {}", conf.username);
    let user = serde_json::json!({
        "password": conf.password,
        "roles": [conf.username],
        "full_name": "Thorium",
    });
    let response = client
        .security()
        .put_user(SecurityPutUserParts::Username(&conf.username))
        .body(user)
        .send()
        .await
        .map_err(elastic_send_error(conf, "create Thorium's Elastic user"))?;
    elastic_check(response, &format!("create Elastic user {}", conf.username)).await?;
    Ok(StepOutcome::Done)
}

/// Describe an Elastic error response by its type and reason
///
/// # Arguments
///
/// * `response` - The failed response to describe
async fn elastic_error(response: Response) -> String {
    // get the status before the body is consumed
    let status = response.status_code();
    // pull the type and reason out of the error body if it has one
    match response.exception().await {
        Ok(Some(exception)) => {
            // get the type and reason of this error
            let error = exception.error();
            let ty = error.ty().unwrap_or("unknown_error");
            let reason = error.reason().unwrap_or("no reason given");
            format!("{status} {ty}: {reason}")
        }
        // without a parseable body all we have is the status
        _ => status.to_string(),
    }
}

/// Check whether an index mapping maps `group` as a keyword
///
/// The mapping is the body of a `GET <index>/_mapping` request, which is keyed by the
/// concrete index name (so an alias resolves to its backing indexes). Every index in the
/// body must map `group` as a keyword.
///
/// # Arguments
///
/// * `mapping` - The `_mapping` response body
fn group_is_keyword(mapping: &serde_json::Value) -> bool {
    // get every concrete index in this response
    let Some(indexes) = mapping.as_object() else {
        return false;
    };
    // every index must map group as a keyword
    !indexes.is_empty()
        && indexes.values().all(|index| {
            index
                .pointer("/mappings/properties/group/type")
                .and_then(serde_json::Value::as_str)
                == Some("keyword")
        })
}

/// Check that an existing index maps `group` as a keyword
///
/// Returns a note describing the problem when it doesn't, since an index created from old
/// dynamic mappings filters groups incorrectly until it is reindexed.
///
/// # Arguments
///
/// * `client` - The Elastic client to use
/// * `name` - The name of the index to check
async fn check_index_mapping(client: &Elasticsearch, name: &str) -> Result<Option<String>, Error> {
    // get this index's mappings
    let response = client
        .indices()
        .get_mapping(IndicesGetMappingParts::Index(&[name]))
        .send()
        .await
        .map_err(|error| {
            Error::new(format!(
                "Failed to read the mappings of Elastic index {name}: {}",
                error_chain(&error)
            ))
        })?;
    // a failed request is reported without failing the reconcile
    if !response.status_code().is_success() {
        let error = elastic_error(response).await;
        return Ok(Some(format!(
            "Could not read the mappings of Elastic index {name}: {error}"
        )));
    }
    // check how group is mapped
    let mapping: serde_json::Value = response.json().await?;
    if group_is_keyword(&mapping) {
        return Ok(None);
    }
    Ok(Some(format!(
        "Elastic index {name} does not map group as a keyword, so group filters may not \
         match; reindex it with the search-streamer's --reindex flag"
    )))
}

/// The index privileges the API and search streamer need on each of Thorium's indexes
///
/// The search streamer checks whether each index exists (`view_index_metadata`) and bulk
/// creates and deletes documents in it (`write`), while the API searches it through point in
/// time queries (`read`). Recreating an index with the search streamer's `--reindex` flag also
/// needs `delete_index`, which is left to the site since it is a manual operation.
const ELASTIC_INDEX_PRIVILEGES: [&str; 3] = ["view_index_metadata", "write", "read"];

/// The extra privilege the search streamer needs on indexes that don't exist yet
const ELASTIC_CREATE_PRIVILEGE: &str = "create_index";

/// Whether one of Thorium's indexes already exists in Elastic
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IndexState {
    /// The index exists
    Exists,
    /// The index doesn't exist yet, so the search streamer will create it
    Missing,
    /// Thorium's user may not check whether the index exists
    Forbidden,
}

impl IndexState {
    /// Get the state of an index from the status of a `HEAD /<index>` request
    ///
    /// Returns `None` for any status that doesn't tell us whether the index exists.
    ///
    /// # Arguments
    ///
    /// * `status` - The status of the exists request
    fn from_status(status: StatusCode) -> Option<Self> {
        // map each status that answers whether the index exists
        match status {
            status if status.is_success() => Some(IndexState::Exists),
            StatusCode::NOT_FOUND => Some(IndexState::Missing),
            StatusCode::FORBIDDEN => Some(IndexState::Forbidden),
            _ => None,
        }
    }
}

/// Build the body of a has-privileges request for the privileges Thorium uses on its indexes
///
/// Every index needs the privileges in `ELASTIC_INDEX_PRIVILEGES`, and indexes that don't
/// exist yet also need `create_index` so the search streamer can create them. An index whose
/// existence couldn't be checked is only asked for the base privileges, since the missing
/// `view_index_metadata` is reported for it on its own.
///
/// # Arguments
///
/// * `indexes` - The name and state of each index Thorium uses
fn elastic_access_request(indexes: &[(&str, IndexState)]) -> serde_json::Value {
    // split the indexes by whether they still have to be created
    let mut present = Vec::with_capacity(indexes.len());
    let mut missing = Vec::with_capacity(indexes.len());
    for (name, state) in indexes {
        if *state == IndexState::Missing {
            missing.push(*name);
        } else {
            present.push(*name);
        }
    }
    // ask for the base privileges on indexes that exist
    let mut entries = Vec::with_capacity(2);
    if !present.is_empty() {
        entries.push(serde_json::json!({"names": present, "privileges": ELASTIC_INDEX_PRIVILEGES}));
    }
    // ask for the base privileges and create_index on indexes that don't exist yet
    if !missing.is_empty() {
        let mut privileges = ELASTIC_INDEX_PRIVILEGES.to_vec();
        privileges.push(ELASTIC_CREATE_PRIVILEGE);
        entries.push(serde_json::json!({"names": missing, "privileges": privileges}));
    }
    serde_json::json!({ "index": entries })
}

/// List the index privileges a has-privileges response reports as missing
///
/// Each missing privilege is listed as `<index>: <privilege>`. A response without a
/// `has_all_requested` of true but with no privilege marked false is reported as a single
/// unknown entry so it is never mistaken for success.
///
/// # Arguments
///
/// * `body` - The body of the has-privileges response
fn missing_privileges(body: &serde_json::Value) -> Vec<String> {
    // nothing is missing when Elastic says every privilege is held
    if body
        .get("has_all_requested")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
    {
        return Vec::new();
    }
    // list every privilege marked false on each index
    let mut missing = body
        .get("index")
        .and_then(serde_json::Value::as_object)
        .into_iter()
        .flatten()
        .flat_map(|(index, privileges)| {
            privileges
                .as_object()
                .into_iter()
                .flatten()
                .filter(|(_, held)| held.as_bool() == Some(false))
                .map(move |(privilege, _)| format!("{index}: {privilege}"))
        })
        .collect::<Vec<String>>();
    // never report an unexpected response as having every privilege
    if missing.is_empty() {
        missing.push("unknown (unexpected has-privileges response)".to_owned());
    }
    missing
}

/// List every index privilege Thorium's user is missing
///
/// This is every privilege the has-privileges response reports as missing plus
/// `view_index_metadata` on each index whose existence Thorium's user was forbidden to check.
///
/// # Arguments
///
/// * `indexes` - The name and state of each index Thorium uses
/// * `body` - The body of the has-privileges response
fn access_problems(indexes: &[(&str, IndexState)], body: &serde_json::Value) -> Vec<String> {
    // start with what Elastic reports as missing
    let mut missing = missing_privileges(body);
    // add the privilege a forbidden exists check shows is missing unless already listed
    for (name, state) in indexes {
        if *state == IndexState::Forbidden {
            let entry = format!("{name}: view_index_metadata");
            if !missing.contains(&entry) {
                missing.push(entry);
            }
        }
    }
    missing
}

/// Check whether an Elastic node is reached through an in-cluster ECK HTTP service
///
/// ECK names the HTTP service of an Elasticsearch `<name>-es-http`, and the chart's own
/// Elasticsearch is one of these, getting Thorium's user from ECK's file realm.
///
/// # Arguments
///
/// * `node` - The url of the Elastic node
fn is_eck_service(node: &str) -> bool {
    // get the host of this node
    let Some(host) = Url::parse(node)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
    else {
        return false;
    };
    // the service name is the first label and an in-cluster name is short or under .svc
    let mut labels = host.split('.');
    let service = labels.next().unwrap_or_default();
    let in_cluster = !host.contains('.') || labels.any(|label| label == "svc");
    service.ends_with("-es-http") && in_cluster
}

/// How long Elastic may keep rejecting Thorium's credentials before an ECK service fails
///
/// ECK's file realm applies the chart's users within a minute or two, so a rejection that
/// outlasts this is a wrong password rather than one still propagating.
const ECK_CREDENTIALS_GRACE_SECS: i64 = 300;

/// Check whether a cluster's status shows Elastic has rejected Thorium's credentials long enough
///
/// This is true when the cluster already failed on rejected credentials, or when it has
/// reported the same wait on them (and nothing else) for longer than the grace period. A wait
/// reported by an upgrade step is embedded in a longer message, so the status only has to
/// contain the wait.
///
/// # Arguments
///
/// * `current` - The status last written for this cluster
/// * `waiting` - The message reported while waiting on the credentials
/// * `rejected` - Text every failure on these rejected credentials contains
/// * `now` - The current time
fn rejected_past_grace(
    current: &ThoriumClusterStatus,
    waiting: &str,
    rejected: &str,
    now: DateTime<Utc>,
) -> bool {
    // get the message this cluster last reported
    let Some(message) = current.message.as_deref() else {
        return false;
    };
    // a cluster already failed on these credentials keeps failing instead of waiting again
    if current.phase == Some(ClusterPhase::Error) && message.contains(rejected) {
        return true;
    }
    // otherwise it must have reported this wait since a transition past the grace period
    if !message.contains(waiting) {
        return false;
    }
    current
        .last_transition
        .as_deref()
        .and_then(|since| DateTime::parse_from_rfc3339(since).ok())
        .is_some_and(|since| {
            now.signed_duration_since(since) > chrono::Duration::seconds(ECK_CREDENTIALS_GRACE_SECS)
        })
}

/// Classify Elastic rejecting Thorium's own credentials
///
/// An Elasticsearch behind an ECK service (like the chart's) gets Thorium's user from ECK's
/// file realm, which can take a minute to apply, so it is waited on for a few minutes. The
/// operator can't tell the chart's ECK from another one (like a converted deployment's), so a
/// rejection that outlasts that grace period fails with how to fix it. Any other Elasticsearch
/// answering 401 without `bootstrap.elastic` set won't start accepting the credentials on its
/// own, so that fails right away. Every other status is waited on.
///
/// # Arguments
///
/// * `conf` - The Elastic config for this cluster
/// * `status` - The status Elastic answered the authenticate request with
/// * `bootstrapped` - Whether `bootstrap.elastic` is set so the operator creates the user
/// * `error` - The error Elastic answered with
/// * `current` - The status last written for this cluster
/// * `now` - The current time
fn rejected_credentials(
    conf: &Elastic,
    status: StatusCode,
    bootstrapped: bool,
    error: &str,
    current: &ThoriumClusterStatus,
    now: DateTime<Utc>,
) -> CheckError {
    // every failure on rejected credentials names them the same way
    let rejected = format!("rejected Thorium's credentials (user {})", conf.username);
    // an external Elastic without a bootstrap won't create the user by itself
    let unauthorized = status == StatusCode::UNAUTHORIZED && !bootstrapped;
    if unauthorized && !is_eck_service(&conf.node) {
        return CheckError::Failed(Error::new(format!(
            "Elastic {} {rejected}: {error}. This Elasticsearch isn't one the Thorium chart \
             manages, so Thorium's user must already exist with the password in the config: \
             create it with the privileges Thorium needs, or set bootstrap.elastic (Helm \
             value operator.cluster.bootstrap.elastic) with an Elastic superuser Secret so \
             the operator creates it",
            conf.node
        )));
    }
    // describe the wait for ECK's file realm to apply the user
    let waiting = format!(
        "Waiting for Elastic to accept Thorium's credentials (user {}): {error}. The chart's \
         Elasticsearch gets this user from ECK's file realm, which can take a minute to \
         apply; an external Elasticsearch needs the user created by the site or by \
         bootstrap.elastic",
        conf.username
    );
    // an ECK service still rejecting the credentials after the grace period won't accept them
    if unauthorized && rejected_past_grace(current, &waiting, &rejected, now) {
        return CheckError::Failed(Error::new(format!(
            "Elastic {} has {rejected} for over {} minutes: {error}. ECK's file realm applies \
             the chart's users within a minute or two, so the password in Thorium's config is \
             likely wrong for this Elasticsearch (such as the ECK of a converted pre-Helm \
             deployment): set the password Thorium's user has there in Thorium's config, \
             create the user with that password and the privileges Thorium needs, or set \
             bootstrap.elastic (Helm value operator.cluster.bootstrap.elastic) with an \
             Elastic superuser Secret so the operator creates it",
            conf.node,
            ECK_CREDENTIALS_GRACE_SECS / 60
        )));
    }
    CheckError::Waiting(waiting)
}

/// Verify Thorium's own Elastic credentials before any component is deployed
///
/// This authenticates as Thorium's user, checks which configured indexes already exist, and
/// checks it holds the privileges the API and search streamer use on them: `read`, `write`
/// and `view_index_metadata` on every index, plus `create_index` on indexes that don't exist
/// yet. The search streamer creates the indexes itself (with their mappings) and streams
/// every existing item into them, so nothing is created here. The mappings of indexes that
/// already exist are checked too, and any problem with them is returned as a note rather
/// than an error.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
pub async fn elastic_access(meta: &ClusterMeta) -> Result<Vec<String>, CheckError> {
    // get the Elastic config for this cluster
    let conf = &meta.conf.elastic;
    // authenticate as Thorium's own user
    let creds = Credentials {
        username: conf.username.clone(),
        password: conf.password.clone(),
    };
    // build a client for Elastic
    let client = elastic_client(conf, &creds).await?;
    // make sure Elastic accepts Thorium's credentials, which ECK's file realm may not yet
    let response = client
        .security()
        .authenticate()
        .send()
        .await
        .map_err(elastic_send_error(
            conf,
            "authenticate Thorium's Elastic user",
        ))?;
    let status = response.status_code();
    if !status.is_success() {
        let error = elastic_error(response).await;
        // a rejection is judged by the bootstrap and how long this cluster has reported it
        let bootstrap = meta.cluster.spec.bootstrap.as_ref();
        let bootstrapped = bootstrap.is_some_and(|bootstrap| bootstrap.elastic.is_some());
        let current = meta.status.current();
        let now = Utc::now();
        return Err(rejected_credentials(
            conf,
            status,
            bootstrapped,
            &error,
            &current,
            now,
        ));
    }
    // check which indexes exist with Thorium's own credentials
    let mut indexes = Vec::with_capacity(4);
    for index in elastic_indexes() {
        // get this index's name
        let name = index.full_name(conf);
        // ask whether this index exists, which needs view_index_metadata
        let exists = client
            .indices()
            .exists(IndicesExistsParts::Index(&[name]))
            .send()
            .await
            .map_err(elastic_send_error(
                conf,
                &format!("check for Elastic index {name}"),
            ))?;
        // any status that doesn't answer whether the index exists fails this check
        let status = exists.status_code();
        let Some(state) = IndexState::from_status(status) else {
            let error = elastic_error(exists).await;
            return Err(elastic_status_failure(
                status,
                format!(
                    "Failed to check for Elastic index {name} as user {}: {error}",
                    conf.username
                ),
            ));
        };
        indexes.push((name, state));
    }
    // ask Elastic whether this user holds every privilege Thorium uses on its indexes
    let response = client
        .security()
        .has_privileges(SecurityHasPrivilegesParts::None)
        .body(elastic_access_request(&indexes))
        .send()
        .await
        .map_err(elastic_send_error(
            conf,
            "check the index privileges of Thorium's Elastic user",
        ))?;
    let status = response.status_code();
    if !status.is_success() {
        let error = elastic_error(response).await;
        return Err(elastic_status_failure(
            status,
            format!(
                "Failed to check the Elastic privileges of user {}: {error}",
                conf.username
            ),
        ));
    }
    // fail with every privilege this user is missing
    let body: serde_json::Value = response.json().await.map_err(Error::from)?;
    let missing = access_problems(&indexes, &body);
    if !missing.is_empty() {
        return Err(Error::new(format!(
            "Elastic user {} is missing index privileges Thorium needs: {}",
            conf.username,
            missing.join(", ")
        ))
        .into());
    }
    // collect any problems with the mappings of indexes the search streamer already created
    let mut notes = Vec::new();
    for (name, state) in indexes {
        if state == IndexState::Exists
            && let Some(note) = check_index_mapping(&client, name).await?
        {
            println!("Warning: {note}");
            notes.push(note);
        }
    }
    Ok(notes)
}

/// List Thorium's existing Elastic indexes that don't map `group` as a keyword
///
/// Indexes that don't exist yet are left out since the search streamer creates them with the
/// right mappings. Reading the mappings needs only Thorium's own credentials.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
pub async fn indexes_needing_reindex(meta: &ClusterMeta) -> Result<Vec<String>, CheckError> {
    // get the Elastic config for this cluster
    let conf = &meta.conf.elastic;
    // authenticate as Thorium's own user
    let creds = Credentials {
        username: conf.username.clone(),
        password: conf.password.clone(),
    };
    let client = elastic_client(conf, &creds).await?;
    // check each configured index that exists
    let mut bad = Vec::new();
    for index in elastic_indexes() {
        // ask whether this index exists
        let name = index.full_name(conf);
        let exists = client
            .indices()
            .exists(IndicesExistsParts::Index(&[name]))
            .send()
            .await
            .map_err(elastic_send_error(
                conf,
                &format!("check for Elastic index {name}"),
            ))?;
        let status = exists.status_code();
        match IndexState::from_status(status) {
            // missing indexes are created correctly by the search streamer
            Some(IndexState::Missing) => continue,
            Some(IndexState::Exists) => (),
            // anything else means we can't tell
            _ => {
                let error = elastic_error(exists).await;
                return Err(elastic_status_failure(
                    status,
                    format!(
                        "Failed to check for Elastic index {name} as user {}: {error}",
                        conf.username
                    ),
                ));
            }
        }
        // read this index's mappings
        let response = client
            .indices()
            .get_mapping(IndicesGetMappingParts::Index(&[name]))
            .send()
            .await
            .map_err(elastic_send_error(
                conf,
                &format!("read the mappings of Elastic index {name}"),
            ))?;
        let status = response.status_code();
        if !status.is_success() {
            let error = elastic_error(response).await;
            return Err(elastic_status_failure(
                status,
                format!("Failed to read the mappings of Elastic index {name}: {error}"),
            ));
        }
        // keep indexes whose group field isn't a keyword
        let mapping: serde_json::Value = response.json().await.map_err(Error::from)?;
        if !group_is_keyword(&mapping) {
            bad.push(name.to_owned());
        }
    }
    Ok(bad)
}

/// Get the bootstrap admin's username when it is known without reading its Secret
///
/// # Arguments
///
/// * `creds` - The Secret and keys holding the admin's credentials
fn known_admin_username(creds: &SecretCredentials) -> Option<&str> {
    // a username key means the name lives in the Secret
    match &creds.username_key {
        Some(_) => None,
        None => creds.username.as_deref(),
    }
}

/// Check whether the bootstrap admin already exists
///
/// With a known username that user must exist. Without one (the name lives in the missing
/// Secret) any admin other than the operator's own service users counts.
///
/// # Arguments
///
/// * `users` - The username and role of every Thorium user
/// * `username` - The bootstrap admin's username, if known
fn admin_exists<'a>(
    mut users: impl Iterator<Item = (&'a str, &'a UserRole)>,
    username: Option<&str>,
) -> bool {
    match username {
        // the named user must exist
        Some(username) => users.any(|(name, _)| name == username),
        // otherwise look for any admin the operator didn't create for itself
        None => {
            users.any(|(name, role)| *role == UserRole::Admin && !SERVICE_USERS.contains(&name))
        }
    }
}

/// Ensure the initial Thorium admin user exists
///
/// An existing user is left untouched so its password is never reset. This does
/// nothing unless `bootstrap.admin` is set. When the admin Secret has been deleted and the
/// admin already exists this step is skipped; it only fails when the admin must be created.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `url` - The url of the Thorium API
/// * `operator` - A Thorium client for the operator's admin user
pub async fn admin(
    meta: &ClusterMeta,
    url: &str,
    operator: &thorium::Thorium,
) -> Result<StepOutcome, Error> {
    // skip this step unless admin bootstrapping is enabled
    let Some(bootstrap) = meta
        .cluster
        .spec
        .bootstrap
        .as_ref()
        .and_then(|bootstrap| bootstrap.admin.as_ref())
    else {
        return Ok(StepOutcome::Done);
    };
    // get the admin's username and password if its Secret still exists
    let Some(Credentials { username, password }) =
        read_credentials(meta, "bootstrap admin", &bootstrap.secret, None).await?
    else {
        // without the Secret we can only check that the admin was already created
        let username = known_admin_username(&bootstrap.secret);
        let users = operator.users.list_details().await?;
        let roles = users
            .iter()
            .map(|user: &ScrubbedUser| (user.username.as_str(), &user.role));
        if admin_exists(roles, username) {
            println!(
                "Admin Secret {} is missing but the admin user already exists",
                bootstrap.secret.name
            );
            return Ok(StepOutcome::Done);
        }
        return Ok(StepOutcome::MissingSecret(missing_secret(
            "bootstrap.admin.secret",
            &bootstrap.secret.name,
            "the admin user doesn't exist yet",
        )));
    };
    // build a local admin user that skips email verification
    let user_req = UserCreate::new(&username, &password, format!("{username}@localhost"))
        .role(UserRole::Admin)
        .skip_verification()
        .local();
    // create this user with our secret key
    let settings = super::helpers::client_settings();
    let result = client::Users::create(
        url,
        user_req,
        Some(&meta.conf.thorium.secret_key),
        &settings,
    )
    .await;
    // an existing user is left alone
    match result {
        Ok(_) => {
            println!("Created admin user {username}");
            Ok(StepOutcome::Done)
        }
        Err(error) if error.status() == Some(StatusCode::CONFLICT) => {
            println!("Admin user {username} already exists");
            Ok(StepOutcome::Done)
        }
        Err(error) => Err(Error::new(format!(
            "Failed to create admin user {username}: {error}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::k8s::clusters::tests::{cluster_from_spec, sample_conf};

    /// Scylla connection failures other than a rejected login are waited on
    #[test]
    fn scylla_connect_failures_classified() {
        // a control connection that timed out isn't an authentication failure
        let timeout = NewSessionError::MetadataError(MetadataError::ConnectionPoolError(
            ConnectionPoolError::Broken {
                last_connection_error: ConnectionError::ConnectTimeout,
            },
        ));
        assert!(!is_scylla_auth_error(&timeout));
        let classified = ScyllaConnectError::classify(&timeout, "thorium");
        let ScyllaConnectError::Unavailable(details) = &classified else {
            panic!("a timeout should be unavailable: {classified:?}");
        };
        // the description says we are waiting and includes the driver's cause
        assert!(details.starts_with("Waiting for Scylla to accept connections as thorium: "));
        assert!(details.contains("Connect timeout elapsed"));
        // a config error isn't an authentication failure either
        assert!(!is_scylla_auth_error(&NewSessionError::EmptyKnownNodesList));
        // both kinds become an error with their description
        let auth = ScyllaConnectError::Auth("rejected".to_owned());
        assert_eq!(
            Error::from(auth).to_string(),
            Error::new("rejected").to_string()
        );
        assert_eq!(
            Error::from(classified.clone()).to_string(),
            Error::new(details.clone()).to_string()
        );
    }

    /// Scylla is waited on when it isn't accepting connections but fails on a rejected login
    #[test]
    fn scylla_check_errors_classified() {
        // a Scylla that isn't up yet is waited on with its description
        let unavailable = ScyllaConnectError::Unavailable("Waiting for Scylla".to_owned());
        let CheckError::Waiting(details) = CheckError::from(unavailable) else {
            panic!("an unavailable Scylla should be waited on");
        };
        assert_eq!(details, "Waiting for Scylla");
        // a rejected login needs attention
        let auth = ScyllaConnectError::Auth("rejected".to_owned());
        let CheckError::Failed(error) = CheckError::from(auth) else {
            panic!("a rejected login should be a failure");
        };
        assert_eq!(error.to_string(), Error::new("rejected").to_string());
    }

    /// An Elastic that refuses connections is waited on while other send errors fail
    #[tokio::test]
    async fn elastic_send_failures_classified() {
        // connecting directly to a closed local port is refused
        let refused = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("a plain client")
            .get("http://127.0.0.1:1")
            .send()
            .await
            .expect_err("nothing listens on port 1");
        let refused = elasticsearch::Error::from(refused);
        assert!(elastic_unreachable(&refused));
        let CheckError::Waiting(details) =
            elastic_send_failure("http://elastic:9200", "authenticate", &refused)
        else {
            panic!("a refused connection should be waited on");
        };
        assert!(details.starts_with(
            "Waiting for Elastic http://elastic:9200 to accept connections: failed to \
             authenticate: "
        ));
        // an error that isn't about the connection needs attention
        let invalid = elasticsearch::Error::from(std::io::Error::other("bad body"));
        assert!(!elastic_unreachable(&invalid));
        let CheckError::Failed(error) =
            elastic_send_failure("http://elastic:9200", "authenticate", &invalid)
        else {
            panic!("a non-connection error should be a failure");
        };
        assert!(
            error
                .to_string()
                .contains("Failed to authenticate at Elastic")
        );
    }

    /// Only statuses meaning Elastic isn't ready yet are waited on
    #[test]
    fn elastic_status_failures_classified() {
        // a proxy without a backend or a starting Elastic is waited on
        for status in [
            StatusCode::BAD_GATEWAY,
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::GATEWAY_TIMEOUT,
        ] {
            assert!(elastic_unavailable(status));
            assert!(matches!(
                elastic_status_failure(status, "failed".to_owned()),
                CheckError::Waiting(details) if details.ends_with(": failed")
            ));
        }
        // rejected credentials or missing privileges need attention
        for status in [
            StatusCode::UNAUTHORIZED,
            StatusCode::FORBIDDEN,
            StatusCode::BAD_REQUEST,
            StatusCode::INTERNAL_SERVER_ERROR,
        ] {
            assert!(!elastic_unavailable(status));
            assert!(matches!(
                elastic_status_failure(status, "failed".to_owned()),
                CheckError::Failed(_)
            ));
        }
    }

    /// Moving bytes between bootstrap hash inputs changes the hash
    #[test]
    fn input_hash_parts_dont_collide() {
        // the same bytes split differently between a secret's name and version hash differently
        let joined = vec![("ab", "c".to_owned())];
        let split = vec![("a", "bc".to_owned())];
        assert_ne!(
            hash_inputs(None, "config", &joined).expect("hash"),
            hash_inputs(None, "config", &split).expect("hash")
        );
        // moving bytes between the config hash and a secret name changes the hash too
        let named = vec![("gx", "1".to_owned())];
        let short = vec![("x", "1".to_owned())];
        assert_ne!(
            hash_inputs(None, "confi", &named).expect("hash"),
            hash_inputs(None, "config", &short).expect("hash")
        );
    }

    /// Build a `ThoriumCluster` with a bootstrap spec
    fn bootstrap_cluster() -> ThoriumCluster {
        cluster_from_spec(serde_json::json!({
            "components": {"api": {}},
            "registry": "registry/thorium",
            "config": {},
            "bootstrap": {"admin": {"secret": {"name": "admin", "password_key": "password"}}}
        }))
    }

    /// The input hash is stable and changes with each of its inputs
    #[test]
    fn input_hash_tracks_inputs() {
        // hash a set of inputs twice
        let cluster = bootstrap_cluster();
        let bootstrap = cluster.spec.bootstrap.as_ref();
        let versions = [("admin", "1".to_owned())];
        let base = hash_inputs(bootstrap, "config", &versions).expect("hash");
        assert_eq!(
            base,
            hash_inputs(bootstrap, "config", &versions).expect("hash")
        );
        // a new config changes the hash
        assert_ne!(
            base,
            hash_inputs(bootstrap, "other", &versions).expect("hash")
        );
        // a new secret version changes the hash
        let bumped = [("admin", "2".to_owned())];
        assert_ne!(
            base,
            hash_inputs(bootstrap, "config", &bumped).expect("hash")
        );
        // a new bootstrap spec changes the hash
        assert_ne!(base, hash_inputs(None, "config", &versions).expect("hash"));
    }

    /// A deleted Secret hashes as a stable marker that differs from any real version
    #[test]
    fn input_hash_marks_absent_secrets() {
        // hash a cluster whose admin secret was deleted twice
        let cluster = bootstrap_cluster();
        let bootstrap = cluster.spec.bootstrap.as_ref();
        let absent = [("admin", absent_marker("admin"))];
        let base = hash_inputs(bootstrap, "config", &absent).expect("hash");
        assert_eq!(
            base,
            hash_inputs(bootstrap, "config", &absent).expect("hash")
        );
        // the marker names the missing secret
        assert_eq!(absent_marker("admin"), "<absent:admin>");
        // deleting a secret changes the hash so the bootstrap reruns once
        let present = [("admin", "1".to_owned())];
        assert_ne!(
            base,
            hash_inputs(bootstrap, "config", &present).expect("hash")
        );
        // a config change while the secret is absent still changes the hash
        assert_ne!(
            base,
            hash_inputs(bootstrap, "other", &absent).expect("hash")
        );
    }

    /// A missing Secret error names the field, the Secret, and why it is needed
    #[test]
    fn missing_secret_message() {
        // build the scylla error
        let message = missing_secret(
            "bootstrap.scylla.admin_secret",
            "scylla-admin",
            "Thorium's role can't log in",
        );
        assert_eq!(
            message,
            "bootstrap.scylla.admin_secret scylla-admin is missing and Thorium's role can't log in"
        );
    }

    /// Only a successful response with every privilege skips the Elastic admin
    #[test]
    fn elastic_privileges_check() {
        // every privilege held skips the admin
        let all = serde_json::json!({"has_all_requested": true});
        assert!(elastic_privileges_ok(StatusCode::OK, Some(&all)));
        // a missing privilege needs the admin
        let some = serde_json::json!({"has_all_requested": false});
        assert!(!elastic_privileges_ok(StatusCode::OK, Some(&some)));
        // a bad password or missing user needs the admin
        assert!(!elastic_privileges_ok(StatusCode::UNAUTHORIZED, Some(&all)));
        // an unparseable body needs the admin
        assert!(!elastic_privileges_ok(StatusCode::OK, None));
    }

    /// The request covers every index pattern Thorium's role is granted
    #[test]
    fn elastic_privileges_request_covers_patterns() {
        // build the request for the default indexes
        let conf = sample_conf();
        let request = elastic_privileges_request(&conf.elastic);
        assert_eq!(
            request,
            serde_json::json!({"index": [{"names": ["thorium*"], "privileges": ["all"]}]})
        );
    }

    /// Each exists status maps to the index state it shows
    #[test]
    fn index_state_from_status() {
        // a success means the index exists
        assert_eq!(
            IndexState::from_status(StatusCode::OK),
            Some(IndexState::Exists)
        );
        // a not found means the search streamer still has to create it
        assert_eq!(
            IndexState::from_status(StatusCode::NOT_FOUND),
            Some(IndexState::Missing)
        );
        // a forbidden means view_index_metadata is missing
        assert_eq!(
            IndexState::from_status(StatusCode::FORBIDDEN),
            Some(IndexState::Forbidden)
        );
        // anything else doesn't answer whether the index exists
        assert_eq!(IndexState::from_status(StatusCode::UNAUTHORIZED), None);
        assert_eq!(
            IndexState::from_status(StatusCode::INTERNAL_SERVER_ERROR),
            None
        );
    }

    /// The access check only asks for `create_index` on indexes that don't exist yet
    #[test]
    fn elastic_access_request_by_state() {
        // a mix of existing, missing and unknown indexes
        let indexes = [
            ("thorium_sample_results", IndexState::Exists),
            ("custom_results", IndexState::Missing),
            ("thorium_sample_tags", IndexState::Forbidden),
            ("thorium_repo_tags", IndexState::Missing),
        ];
        assert_eq!(
            elastic_access_request(&indexes),
            serde_json::json!({"index": [
                {
                    "names": ["thorium_sample_results", "thorium_sample_tags"],
                    "privileges": ["view_index_metadata", "write", "read"]
                },
                {
                    "names": ["custom_results", "thorium_repo_tags"],
                    "privileges": ["view_index_metadata", "write", "read", "create_index"]
                }
            ]})
        );
        // only existing indexes never ask for create_index
        let existing = [("thorium_sample_results", IndexState::Exists)];
        assert_eq!(
            elastic_access_request(&existing),
            serde_json::json!({"index": [{
                "names": ["thorium_sample_results"],
                "privileges": ["view_index_metadata", "write", "read"]
            }]})
        );
        // only missing indexes ask for create_index on all of them
        let missing = [("thorium_sample_results", IndexState::Missing)];
        assert_eq!(
            elastic_access_request(&missing),
            serde_json::json!({"index": [{
                "names": ["thorium_sample_results"],
                "privileges": ["view_index_metadata", "write", "read", "create_index"]
            }]})
        );
    }

    /// Every configured index is checked, including ones outside the thorium pattern
    #[test]
    fn elastic_access_indexes_configured() {
        // a config with one index outside the thorium pattern
        let mut conf = sample_conf();
        conf.elastic.results.samples = "custom_results".to_owned();
        let names = elastic_indexes()
            .iter()
            .map(|index| index.full_name(&conf.elastic).to_owned())
            .collect::<Vec<String>>();
        assert_eq!(names.len(), 4);
        assert!(names.contains(&"custom_results".to_owned()));
    }

    /// A forbidden exists check is reported as a missing `view_index_metadata` exactly once
    #[test]
    fn access_problems_include_forbidden() {
        // a forbidden index that has-privileges doesn't list is still reported
        let indexes = [
            ("thorium_sample_tags", IndexState::Forbidden),
            ("thorium_repo_tags", IndexState::Missing),
        ];
        let body = serde_json::json!({
            "has_all_requested": false,
            "index": {"thorium_repo_tags": {"create_index": false, "read": true}}
        });
        assert_eq!(
            access_problems(&indexes, &body),
            vec![
                "thorium_repo_tags: create_index".to_owned(),
                "thorium_sample_tags: view_index_metadata".to_owned()
            ]
        );
        // a forbidden index has-privileges already lists isn't repeated
        let body = serde_json::json!({
            "has_all_requested": false,
            "index": {"thorium_sample_tags": {"view_index_metadata": false}}
        });
        assert_eq!(
            access_problems(&indexes, &body),
            vec!["thorium_sample_tags: view_index_metadata".to_owned()]
        );
        // every privilege held with indexes that exist or will be created reports nothing
        let indexes = [("thorium_repo_tags", IndexState::Missing)];
        let all = serde_json::json!({"has_all_requested": true});
        assert_eq!(access_problems(&indexes, &all), Vec::<String>::new());
    }

    /// Missing privileges are listed per index and an odd response never passes
    #[test]
    fn missing_privileges_listed() {
        // every privilege held lists nothing
        let all = serde_json::json!({"has_all_requested": true, "index": {}});
        assert_eq!(missing_privileges(&all), Vec::<String>::new());
        // each privilege marked false is listed with its index
        let some = serde_json::json!({
            "has_all_requested": false,
            "index": {
                "thorium_sample_tags": {"create_index": false, "read": true, "write": false}
            }
        });
        assert_eq!(
            missing_privileges(&some),
            vec![
                "thorium_sample_tags: create_index".to_owned(),
                "thorium_sample_tags: write".to_owned()
            ]
        );
        // a response without has_all_requested is never treated as success
        assert_eq!(missing_privileges(&serde_json::json!({})).len(), 1);
    }

    /// The admin's username is only known without its Secret when it is a literal
    #[test]
    fn admin_username_known() {
        // a literal username is known
        let mut creds = SecretCredentials {
            name: "admin".to_owned(),
            username: Some("root".to_owned()),
            username_key: None,
            password_key: "password".to_owned(),
        };
        assert_eq!(known_admin_username(&creds), Some("root"));
        // a username key takes precedence and lives in the Secret
        creds.username_key = Some("username".to_owned());
        assert_eq!(known_admin_username(&creds), None);
    }

    /// The admin counts as existing only when the named user or a non-service admin exists
    #[test]
    fn admin_existence() {
        // the operator's own users and a regular user
        let users = [
            ("thorium-operator", UserRole::Admin),
            ("thorium", UserRole::Admin),
            ("thorium-kaboom", UserRole::Admin),
            ("alice", UserRole::User),
        ];
        let iter = || users.iter().map(|(name, role)| (*name, role));
        // a named user must exist
        assert!(admin_exists(iter(), Some("alice")));
        assert!(!admin_exists(iter(), Some("root")));
        // service users don't count as the bootstrap admin
        assert!(!admin_exists(iter(), None));
        // any other admin does
        let with_admin = [("root", UserRole::Admin)];
        let iter = || {
            users
                .iter()
                .chain(with_admin.iter())
                .map(|(name, role)| (*name, role))
        };
        assert!(admin_exists(iter(), None));
    }

    /// The bootstrap is due until it completes with the same inputs
    #[test]
    fn bootstrap_is_due() {
        // a cluster that never completed a bootstrap is due
        let mut cluster = bootstrap_cluster();
        assert!(is_due(&cluster, "hash"));
        // a cluster that completed this bootstrap is not
        cluster.status = Some(crate::k8s::crds::ThoriumClusterStatus {
            bootstrap_hash: Some("hash".to_owned()),
            ..Default::default()
        });
        assert!(!is_due(&cluster, "hash"));
        // new inputs make it due again
        assert!(is_due(&cluster, "new"));
    }

    /// The role covers every thorium index plus configured indexes outside that pattern
    #[test]
    fn index_patterns() {
        // the default index names are all covered by thorium*
        let mut conf = sample_conf();
        assert_eq!(elastic_index_patterns(&conf.elastic), vec!["thorium*"]);
        // an index outside the pattern is added on its own
        conf.elastic.results.samples = "custom_results".to_owned();
        assert_eq!(
            elastic_index_patterns(&conf.elastic),
            vec!["thorium*", "custom_results"]
        );
    }

    /// Every index maps group as a keyword so group filters match exactly
    #[test]
    fn create_body_maps_group_as_keyword() {
        // check the body of every index we create
        let conf = sample_conf();
        for index in elastic_indexes() {
            let body = index.create_body(&conf.elastic);
            assert_eq!(
                body.pointer("/mappings/properties/group/type"),
                Some(&serde_json::json!("keyword")),
                "{}",
                index.full_name(&conf.elastic)
            );
        }
    }

    /// Only mappings where every index maps group as a keyword pass
    #[test]
    fn group_keyword_check() {
        // a keyword mapping passes
        let good = serde_json::json!({"idx": {"mappings": {"properties": {"group": {"type": "keyword"}}}}});
        assert!(group_is_keyword(&good));
        // a dynamic text mapping fails
        let text =
            serde_json::json!({"idx": {"mappings": {"properties": {"group": {"type": "text"}}}}});
        assert!(!group_is_keyword(&text));
        // a missing group field fails
        let missing = serde_json::json!({"idx": {"mappings": {}}});
        assert!(!group_is_keyword(&missing));
        // an alias with one bad backing index fails
        let mixed = serde_json::json!({
            "a": {"mappings": {"properties": {"group": {"type": "keyword"}}}},
            "b": {"mappings": {"properties": {"group": {"type": "text"}}}}
        });
        assert!(!group_is_keyword(&mixed));
        // an empty response fails
        assert!(!group_is_keyword(&serde_json::json!({})));
    }

    /// Identifiers are wrapped in double quotes with embedded quotes doubled
    #[test]
    fn cql_identifier_quotes() {
        // a plain identifier is just wrapped
        assert_eq!(cql_identifier("thorium"), "\"thorium\"");
        // embedded double quotes are doubled
        assert_eq!(cql_identifier("a\"b"), "\"a\"\"b\"");
        // single quotes need no escaping inside an identifier
        assert_eq!(cql_identifier("o'neil"), "\"o'neil\"");
    }

    /// An injection-shaped identifier stays a single quoted identifier
    #[test]
    fn cql_identifier_injection() {
        // try to break out of the identifier and run a second statement
        let quoted = cql_identifier("x\" WITH SUPERUSER = true; DROP ROLE \"cassandra");
        // every quote inside the identifier is doubled so it can't be closed early
        assert_eq!(
            quoted,
            "\"x\"\" WITH SUPERUSER = true; DROP ROLE \"\"cassandra\""
        );
        // stripping the outer quotes leaves no lone quote behind
        let inner = &quoted[1..quoted.len() - 1];
        assert!(!inner.replace("\"\"", "").contains('"'));
    }

    /// Literals are wrapped in single quotes with embedded quotes doubled
    #[test]
    fn cql_literal_quotes() {
        // a plain literal is just wrapped
        assert_eq!(cql_literal("secret"), "'secret'");
        // embedded single quotes are doubled
        assert_eq!(cql_literal("it's"), "'it''s'");
        // double quotes need no escaping inside a literal
        assert_eq!(cql_literal("a\"b"), "'a\"b'");
        // an empty literal is still quoted
        assert_eq!(cql_literal(""), "''");
    }

    /// An injection-shaped literal stays a single quoted literal
    #[test]
    fn cql_literal_injection() {
        // try to close the password and grant superuser
        let quoted = cql_literal("x' AND SUPERUSER = true; --");
        // the embedded quote is doubled so the literal can't be closed early
        assert_eq!(quoted, "'x'' AND SUPERUSER = true; --'");
        // stripping the outer quotes leaves no lone quote behind
        let inner = &quoted[1..quoted.len() - 1];
        assert!(!inner.replace("''", "").contains('\''));
    }

    /// Debug output never reveals a password
    #[test]
    fn credentials_debug_masks_password() {
        // build some credentials
        let creds = Credentials {
            username: "thorium".to_owned(),
            password: "hunter2".to_owned(),
        };
        // format them for debugging
        let debug = format!("{creds:?}");
        // the username is shown but the password is masked
        assert!(debug.contains("thorium"));
        assert!(debug.contains("***"));
        assert!(!debug.contains("hunter2"));
    }

    /// Only in-cluster ECK HTTP services count as the chart's kind of Elasticsearch
    #[test]
    fn eck_services_detected() {
        // the chart's Elasticsearch and other in-cluster ECK services
        assert!(is_eck_service(
            "https://elastic-es-http.elastic.svc.cluster.local:9200"
        ));
        assert!(is_eck_service(
            "https://elastic-es-http.dev-elastic.svc:9200"
        ));
        assert!(is_eck_service("https://elastic-es-http:9200"));
        // external Elasticsearches and other services
        assert!(!is_eck_service("https://elastic.example.com:9200"));
        assert!(!is_eck_service("https://search-es-http.example.com:9200"));
        assert!(!is_eck_service(
            "https://elastic.elastic.svc.cluster.local:9200"
        ));
        assert!(!is_eck_service("not a url"));
    }

    /// Classify Elastic answering 401 to Thorium's credentials for a cluster's status
    ///
    /// # Arguments
    ///
    /// * `conf` - The Elastic config for this cluster
    /// * `bootstrapped` - Whether `bootstrap.elastic` is set so the operator creates the user
    /// * `current` - The status last written for this cluster
    /// * `now` - The current time
    fn reject(
        conf: &Elastic,
        bootstrapped: bool,
        current: &ThoriumClusterStatus,
        now: DateTime<Utc>,
    ) -> CheckError {
        rejected_credentials(
            conf,
            StatusCode::UNAUTHORIZED,
            bootstrapped,
            "denied",
            current,
            now,
        )
    }

    /// Rejected credentials fail for an external Elastic without a bootstrap and are waited on
    /// otherwise
    #[test]
    fn rejected_credentials_by_backend() {
        // a cluster that hasn't reported anything yet
        let fresh = ThoriumClusterStatus::default();
        let now = Utc::now();
        // an external Elastic without a bootstrap fails with how to fix it
        let mut conf = sample_conf().elastic;
        conf.node = "https://elastic.example.com:9200".to_owned();
        let failed = reject(&conf, false, &fresh, now);
        let CheckError::Failed(error) = failed else {
            panic!("an external Elastic rejecting credentials should fail");
        };
        assert!(error.to_string().contains("bootstrap.elastic"));
        assert!(error.to_string().contains("elastic.example.com"));
        // with a bootstrap the user is being created, so it is waited on
        assert!(matches!(
            reject(&conf, true, &fresh, now),
            CheckError::Waiting(_)
        ));
        // so is an Elastic that isn't ready to answer yet
        assert!(matches!(
            rejected_credentials(
                &conf,
                StatusCode::SERVICE_UNAVAILABLE,
                false,
                "starting",
                &fresh,
                now
            ),
            CheckError::Waiting(_)
        ));
        // the chart's ECK file realm can take a minute to apply
        let chart = sample_conf().elastic;
        assert!(matches!(
            reject(&chart, false, &fresh, now),
            CheckError::Waiting(_)
        ));
    }

    /// An ECK service rejecting Thorium's credentials is waited on for the grace period only
    #[test]
    fn rejected_eck_credentials_fail_after_grace() {
        // the wait an in-cluster ECK service rejecting the credentials reports
        let chart = sample_conf().elastic;
        let start = Utc::now();
        let fresh = ThoriumClusterStatus::default();
        let CheckError::Waiting(waiting) = reject(&chart, false, &fresh, start) else {
            panic!("a fresh rejection by an ECK service should be waited on");
        };
        // build a status that has reported a message since a given time
        let reported =
            |phase: ClusterPhase, message: &str, since: DateTime<Utc>| ThoriumClusterStatus {
                phase: Some(phase),
                message: Some(message.to_owned()),
                last_transition: Some(since.to_rfc3339()),
                ..ThoriumClusterStatus::default()
            };
        let waited = reported(ClusterPhase::Provisioning, &waiting, start);
        // within the grace period the rejection is still waited on
        let early = start + chrono::Duration::seconds(ECK_CREDENTIALS_GRACE_SECS - 30);
        assert!(matches!(
            reject(&chart, false, &waited, early),
            CheckError::Waiting(_)
        ));
        // past it the rejection fails with how to fix it
        let late = start + chrono::Duration::seconds(ECK_CREDENTIALS_GRACE_SECS + 30);
        let CheckError::Failed(error) = reject(&chart, false, &waited, late) else {
            panic!("a rejection outlasting the grace period should fail");
        };
        let failure = error.to_string();
        assert!(failure.contains("for over 5 minutes"), "{failure}");
        assert!(failure.contains("bootstrap.elastic"), "{failure}");
        // an upgrade step embedding the wait in its message is bounded the same way
        let step = reported(
            ClusterPhase::Upgrading,
            &format!("Step elastic-identity is waiting: {waiting}"),
            start,
        );
        assert!(matches!(
            reject(&chart, false, &step, late),
            CheckError::Failed(_)
        ));
        // a cluster that already failed on these credentials keeps failing instead of waiting
        let errored = reported(ClusterPhase::Error, &failure, late);
        assert!(matches!(
            reject(&chart, false, &errored, late),
            CheckError::Failed(_)
        ));
        // a different long wait doesn't count toward the credentials' grace period
        let other = reported(ClusterPhase::Provisioning, "Waiting for Redis", start);
        assert!(matches!(
            reject(&chart, false, &other, late),
            CheckError::Waiting(_)
        ));
        // a bootstrap creating the user is still waited on however long it takes
        assert!(matches!(
            reject(&chart, true, &waited, late),
            CheckError::Waiting(_)
        ));
    }

    /// Answer every Elastic request with a 401
    ///
    /// # Arguments
    ///
    /// * `_method` - The request method
    /// * `_path` - The request path
    fn unauthorized_elastic(_method: &str, _path: &str) -> (u16, String) {
        (
            401,
            r#"{"error":{"type":"security_exception","reason":"unable to authenticate user"}}"#
                .to_owned(),
        )
    }

    /// The access check fails right away when an external Elastic rejects Thorium's user
    #[tokio::test]
    async fn external_elastic_rejection_fails_access_check() {
        // a cluster pointed at an external Elastic that rejects every request
        let fake = crate::k8s::clusters::tests::FakeKube::default();
        let mut meta = crate::k8s::clusters::tests::meta_for(
            crate::k8s::clusters::tests::namespaced_cluster(
                crate::k8s::clusters::tests::full_spec(),
            ),
            &fake.client(),
        );
        meta.conf.elastic.node = crate::upgrades::tests::fake_elastic(unauthorized_elastic).await;
        // the check fails instead of waiting forever
        let error = elastic_access(&meta).await.expect_err("rejected");
        let CheckError::Failed(error) = error else {
            panic!("an external Elastic rejecting credentials should fail: {error}");
        };
        assert!(error.to_string().contains("unable to authenticate user"));
    }
}
