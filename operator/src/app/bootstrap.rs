//! Setup of the backend roles and users a Thorium cluster needs
//!
//! Each step is idempotent so it can run on every reconcile.

use reqwest::StatusCode;
use scylla::client::session::Session;
use scylla::client::session_builder::{GenericSessionBuilder, SessionBuilder};
use std::time::Duration;
use thorium::conf::{Elastic, ElasticCertValidation};
use thorium::models::{UserCreate, UserRole};
use thorium::{Error, client};

use crate::k8s::clusters::ClusterMeta;
use crate::k8s::crds::SecretCredentials;
use crate::k8s::secrets;

/// How long to wait when connecting to a backend
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Read a username and password from a Secret
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `creds` - The Secret and keys holding the credentials
/// * `default_username` - The username to use if the credentials don't set one
async fn read_credentials(
    meta: &ClusterMeta,
    creds: &SecretCredentials,
    default_username: Option<&str>,
) -> Result<(String, String), Error> {
    // default to the ThoriumCluster's namespace
    let namespace = creds.namespace.as_deref().unwrap_or(&meta.namespace);
    // get the username from the secret, a literal, or our default in that order
    let username = match (&creds.username_key, &creds.username, default_username) {
        (Some(key), _, _) => {
            secrets::get_secret_key(&meta.client, namespace, &creds.name, key).await?
        }
        (None, Some(username), _) => username.clone(),
        (None, None, Some(default)) => default.to_owned(),
        (None, None, None) => {
            return Err(Error::new(format!(
                "Credentials from secret {namespace}/{} need a username or username_key",
                creds.name
            )));
        }
    };
    // get the password from the secret
    let password =
        secrets::get_secret_key(&meta.client, namespace, &creds.name, &creds.password_key).await?;
    Ok((username, password))
}

/// Connect to Scylla as a specific user
///
/// # Arguments
///
/// * `nodes` - The Scylla nodes to connect to
/// * `username` - The user to connect as
/// * `password` - The password for this user
async fn scylla_connect(
    nodes: &[String],
    username: &str,
    password: &str,
) -> Result<Session, Error> {
    // build a session for this user
    let builder = SessionBuilder::new()
        .user(username, password)
        .connection_timeout(CONNECT_TIMEOUT);
    // add our nodes and connect
    nodes
        .iter()
        .fold(builder, GenericSessionBuilder::known_node)
        .build()
        .await
        .map_err(|error| {
            Error::new(format!(
                "Failed to connect to Scylla as {username}: {error}"
            ))
        })
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

/// Ensure Thorium's Scylla role exists
///
/// This runs before the API is deployed since the API needs this role to create its
/// keyspace. It does nothing unless `bootstrap.scylla` is set.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
pub async fn scylla(meta: &ClusterMeta) -> Result<(), Error> {
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
        ));
    };
    // get the nodes to connect to
    let nodes = &meta.conf.scylla.nodes;
    // check if Thorium's role already works
    if let Ok(session) = scylla_connect(nodes, &auth.username, &auth.password).await {
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
    // get the superuser credentials to create our role with
    let (admin_username, admin_password) = match &bootstrap.admin_secret {
        Some(creds) => read_credentials(meta, creds, Some("cassandra")).await?,
        None => ("cassandra".to_owned(), "cassandra".to_owned()),
    };
    // connect as the superuser
    let session = scylla_connect(nodes, &admin_username, &admin_password).await?;
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

/// Build an HTTP client for Elastic that respects the configured cert validation
///
/// # Arguments
///
/// * `conf` - The Elastic config for this cluster
async fn elastic_client(conf: &Elastic) -> Result<reqwest::Client, Error> {
    // start building our client
    let mut builder = reqwest::Client::builder().timeout(CONNECT_TIMEOUT);
    // apply our cert validation settings
    if conf.insecure_certificates {
        // skip cert validation entirely
        builder = builder.danger_accept_invalid_certs(true);
    } else if let Some(ElasticCertValidation::Full(path) | ElasticCertValidation::CA(path)) =
        &conf.cert_validation
    {
        // read the CA cert to trust
        let pem = tokio::fs::read(path).await.map_err(|error| {
            Error::new(format!(
                "Failed to read Elastic CA cert {}: {error}",
                path.display()
            ))
        })?;
        // parse the CA cert
        let cert = reqwest::Certificate::from_pem(&pem).map_err(|error| {
            Error::new(format!(
                "Failed to parse Elastic CA cert {}: {error}",
                path.display()
            ))
        })?;
        // trust this CA cert
        builder = builder.add_root_certificate(cert);
    }
    // build our client
    builder
        .build()
        .map_err(|error| Error::new(format!("Failed to build Elastic client: {error}")))
}

/// Send a PUT request to Elastic
///
/// Returns the status and body of the response.
///
/// # Arguments
///
/// * `client` - The HTTP client to use
/// * `url` - The URL to PUT to
/// * `creds` - The username and password to authenticate with
/// * `body` - The JSON body to send
async fn elastic_put(
    client: &reqwest::Client,
    url: &str,
    creds: &(String, String),
    body: &serde_json::Value,
) -> Result<(StatusCode, String), Error> {
    // send our request
    let resp = client
        .put(url)
        .basic_auth(&creds.0, Some(&creds.1))
        .json(body)
        .send()
        .await
        .map_err(|error| Error::new(format!("Failed to send request to {url}: {error}")))?;
    // get the status and body of our response
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    Ok((status, text))
}

/// Ensure Thorium's Elastic role, user, and indexes exist
///
/// This runs before the API is deployed. It does nothing unless `bootstrap.elastic` is set.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
pub async fn elastic(meta: &ClusterMeta) -> Result<(), Error> {
    // skip this step unless elastic bootstrapping is enabled
    let Some(bootstrap) = meta
        .cluster
        .spec
        .bootstrap
        .as_ref()
        .and_then(|bootstrap| bootstrap.elastic.as_ref())
    else {
        return Ok(());
    };
    // get the Elastic config for this cluster
    let conf = &meta.conf.elastic;
    // get the superuser credentials to configure Elastic with
    let admin = read_credentials(meta, &bootstrap.admin_secret, Some("elastic")).await?;
    // build a client for Elastic
    let client = elastic_client(conf).await?;
    // get the base url for Elastic
    let node = conf.node.trim_end_matches('/');
    // get the indexes the API uses
    let indexes = [
        &conf.results.samples,
        &conf.results.repos,
        &conf.tags.samples,
        &conf.tags.repos,
    ];
    // grant access to all thorium indexes and any configured index outside that pattern
    let mut patterns = vec!["thorium*".to_owned()];
    patterns.extend(
        indexes
            .iter()
            .filter(|index| !index.starts_with("thorium"))
            .map(|index| (*index).clone()),
    );
    // create or update Thorium's role
    println!("Ensuring Elastic role {}", conf.username);
    let role = serde_json::json!({"indices": [{"names": patterns, "privileges": ["all"]}]});
    let url = format!("{node}/_security/role/{}", conf.username);
    let (status, body) = elastic_put(&client, &url, &admin, &role).await?;
    if !status.is_success() {
        return Err(Error::new(format!(
            "Failed to create Elastic role {}: {status} {body}",
            conf.username
        )));
    }
    // create or update Thorium's user so its password always matches our config
    println!("Ensuring Elastic user {}", conf.username);
    let user = serde_json::json!({
        "password": conf.password,
        "roles": [conf.username],
        "full_name": "Thorium",
    });
    let url = format!("{node}/_security/user/{}", conf.username);
    let (status, body) = elastic_put(&client, &url, &admin, &user).await?;
    if !status.is_success() {
        return Err(Error::new(format!(
            "Failed to create Elastic user {}: {status} {body}",
            conf.username
        )));
    }
    // create each index the API uses if it doesn't already exist
    for index in indexes {
        // try to create this index
        let url = format!("{node}/{index}");
        let (status, body) = elastic_put(&client, &url, &admin, &serde_json::json!({})).await?;
        // an index that already exists is fine
        if status.is_success() {
            println!("Created Elastic index {index}");
        } else if !body.contains("resource_already_exists_exception") {
            return Err(Error::new(format!(
                "Failed to create Elastic index {index}: {status} {body}"
            )));
        }
    }
    Ok(())
}

/// Ensure the initial Thorium admin user exists
///
/// An existing user is left untouched so its password is never reset. This does
/// nothing unless `bootstrap.admin` is set.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `url` - The url of the Thorium API
pub async fn admin(meta: &ClusterMeta, url: &str) -> Result<(), Error> {
    // skip this step unless admin bootstrapping is enabled
    let Some(bootstrap) = meta
        .cluster
        .spec
        .bootstrap
        .as_ref()
        .and_then(|bootstrap| bootstrap.admin.as_ref())
    else {
        return Ok(());
    };
    // get the admin's username and password
    let (username, password) = read_credentials(meta, &bootstrap.secret, None).await?;
    // build a local admin user that skips email verification
    let user_req = UserCreate::new(&username, &password, format!("{username}@localhost"))
        .role(UserRole::Admin)
        .skip_verification()
        .local();
    // create this user with our secret key
    let settings = client::ClientSettings::default();
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
            Ok(())
        }
        Err(error) if error.status() == Some(StatusCode::CONFLICT) => {
            println!("Admin user {username} already exists");
            Ok(())
        }
        Err(error) => Err(Error::new(format!(
            "Failed to create admin user {username}: {error}"
        ))),
    }
}
