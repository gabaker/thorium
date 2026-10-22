//! Wrappers for interacting with users within Thorium with different backends
//! Currently only Redis is supported

use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Version};
use axum::extract::multipart::Field;
use axum::extract::{FromRef, FromRequestParts};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use chrono::prelude::*;
use headers::{Header, HeaderName, HeaderValue};
use ldap3::{Scope, SearchEntry};
use rand::prelude::*;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::str;
use std::sync::{Arc, LazyLock};
use tokio::sync::Semaphore;
use tracing::{Level, Span, event, instrument};
use uuid::Uuid;

use super::db;
use crate::conf::Ldap;
use crate::models::backends::GraphicSupport;
use crate::models::backends::NotificationSupport;
use crate::models::backends::entities::models::{
    RefreshedEndpoint, apply_credential_effect, check_report, credential_effect,
    merge_refresh_report, refresh_allowed, refreshable_endpoint, sync_broken_reference,
    sync_whitelist_ban,
};
use crate::models::{
    AiEndpoint, AiEndpointUpdate, AiSettings, AiSettingsUpdate, AuthResponse, AuthedUser,
    CredentialType, Entity, EntityKinds, EntityMetadata, Event, Group, ImageScaler, Key, ModelBan,
    ModelBanKind, ModelBanUpdate, ModelConfigRef, ModelProvider, ModelRefreshReason,
    ModelRefreshStatus, ModelRefreshTarget, ModelsConf, RedactSecrets, ScopedModelBan, ScopedToken,
    ScopedTokenRequest, ScopedTokenRole, ScopedTokenUpdate, ScopedUser, ScrubbedUser, UnixInfo,
    User, UserCreate, UserCredentialRef, UserModelSettings, UserModelSettingsUpdate, UserRole,
    UserSettings, UserSettingsUpdate, UserUpdate,
};
use crate::utils::shared::EmailClient;
use crate::utils::{ApiError, AppState, Shared, bounder};
use crate::{bad, conflict, is_admin, ldap, not_found, unauthorized, unavailable, update};

/// The header name for our secret key
static SECRET_KEY_HEADER: HeaderName = HeaderName::from_static("secret-key");
static SECRET_KEY_HEADER_REF: &HeaderName = &SECRET_KEY_HEADER;

/// Return unauthorized if a function return an error
macro_rules! check_unauth {
    ($func:expr) => {
        match $func {
            Ok(val) => val,
            Err(_) => return crate::unauthorized!(),
        }
    };
}

impl Header for Key {
    fn name() -> &'static HeaderName {
        SECRET_KEY_HEADER_REF
    }

    fn decode<'i, I>(values: &mut I) -> Result<Self, headers::Error>
    where
        Self: Sized,
        I: Iterator<Item = &'i HeaderValue>,
    {
        // get our header value
        let value = values.next().ok_or_else(headers::Error::invalid)?;
        // decode the header as a str without panicking so a non-UTF8 secret-key header is
        // rejected as an invalid header instead of crashing the request task
        let decoded = value.to_str().map_err(|_| headers::Error::invalid())?;
        // cast this header into a secret key
        let key = Key {
            key: decoded.to_owned(),
        };
        Ok(key)
    }

    fn encode<E: Extend<HeaderValue>>(&self, values: &mut E) {
        // wrap our secret key
        let value = HeaderValue::from_str(&self.key);
        // add this to the header
        values.extend(value);
    }
}

/// Hashes a users password
#[macro_export]
macro_rules! hash_pw {
    ($raw:expr, $secret_key:expr) => {
        // hash this password with our salt
        Argon2::new_with_secret(
            $secret_key.as_bytes(),
            Algorithm::Argon2id,
            Version::V0x13,
            argon2::Params::default(),
        )?
        .hash_password($raw.as_bytes(), &SaltString::generate(&mut OsRng))?
        .to_string()
    };
}

/// generate a token
#[macro_export]
macro_rules! token {
    () => {{
        let mut rng = rand::rng();
        let token: [u8; 32] = rng.random();
        hex::encode(token)
    }};
}

/// get the time a new token should expire
#[macro_export]
macro_rules! token_expire {
    ($shared:expr) => {
        // update token expiration
        Utc::now() + chrono::Duration::days(i64::from($shared.config.thorium.auth.token_expire))
    };
}

/// Authenticate a user by token
///
/// # Arguments
///
/// * `token` - The token to authenticate with
/// * `shared` - Shared objects in Thorium
#[instrument(name = "backends::user::token_auth", skip_all, err(Debug))]
async fn token_auth(token: &str, shared: &Shared) -> Result<AuthedUser, ApiError> {
    // look this token up in the primary and scoped token maps
    match db::users::get_token_match(token, shared).await? {
        // this token is a users primary token
        db::users::TokenMatch::Primary(username) => {
            // get this users data
            let mut user = db::users::get(&username, shared).await?;
            // throw unauthorized if token doesn't match
            // this should only happen if the token map is somehow wrong
            // which should never happen
            if user.token != token {
                event!(Level::ERROR, msg = "Token Map Corruption Likely");
                return unauthorized!();
            }
            // Check if this users token has expired
            if user.token_expiration < Utc::now() {
                // token is expired so generate a new one and bounce this user
                event!(Level::INFO, msg = "Regenerating Expired Token");
                user.regen_token(shared).await?;
                return unauthorized!();
            }
            // return authed user
            Ok(AuthedUser::Full(user))
        }
        // this token is a scoped token so authenticate with it
        db::users::TokenMatch::Scoped(scoped) => scoped_token_auth(scoped, token, shared).await,
    }
}

/// Authenticate a user by a scoped token
///
/// The returned user has its groups limited to this scoped tokens scope and
/// carries the scoped tokens value/expiration instead of the users primary
/// token. The user acts with the scoped token's role capped at the owner's
/// current role (see [`ScopedTokenRole::effective_role`]).
///
/// # Arguments
///
/// * `scoped` - The scoped token to authenticate with
/// * `token` - The raw token value that was used to authenticate
/// * `shared` - Shared objects in Thorium
#[instrument(name = "backends::user::scoped_token_auth", skip_all, err(Debug))]
async fn scoped_token_auth(
    mut scoped: ScopedToken,
    token: &str,
    shared: &Shared,
) -> Result<AuthedUser, ApiError> {
    // throw unauthorized if the token doesn't match
    // this should only happen if the scoped token map is somehow wrong
    // which should never happen
    if scoped.token != token {
        event!(Level::ERROR, msg = "Scoped Token Map Corruption Likely");
        return unauthorized!();
    }
    // if this scoped token is ephemeral then check if it has permanently expired
    if let Some(expires) = scoped.expires
        && expires < Utc::now()
    {
        // this scoped token has permanently expired so remove it from redis
        event!(Level::INFO, msg = "Deleting Expired Ephemeral Scoped Token");
        db::users::delete_scoped_token(&scoped, shared).await?;
        return unauthorized!();
    }
    // check if this scoped tokens value has expired
    if scoped.token_expiration < Utc::now() {
        // this value is expired so rotate it and bounce this user
        event!(Level::INFO, msg = "Rotating Expired Scoped Token");
        scoped.rotate(shared).await?;
        return unauthorized!();
    }
    // get the user this scoped token is tied to
    let mut user = db::users::get(&scoped.owner, shared).await?;
    // limit this users groups to this scoped tokens scope
    user.groups.retain(|group| scoped.groups.contains(group));
    // act with this scoped token's role capped at the owner's current role so scoped
    // tokens never act as admins or analysts or with more developer rights than either has
    user.role = scoped.role.effective_role(&user.role);
    // replace this users primary token info with this scoped tokens info
    user.token = scoped.token;
    user.token_expiration = scoped.token_expiration;
    // build our scoped user
    let scoped_user = ScopedUser {
        user,
        name: scoped.name,
    };
    Ok(AuthedUser::Scoped(scoped_user))
}

/// Check that a stored password hash uses the algorithm and cost Thorium hashes with
///
/// Argon2 verification recomputes a hash with the algorithm and costs written in the stored hash,
/// so a hash naming a huge memory or time cost would make every login as its user allocate or
/// spin that much. Only argon2id with [`argon2::Params::default`] is accepted, which is what
/// Thorium writes.
///
/// # Arguments
///
/// * `hash` - The stored password hash to check
pub(crate) fn check_password_hash(hash: &str) -> Result<(), String> {
    // parse the hash without computing anything
    let parsed = PasswordHash::new(hash).map_err(|err| format!("unparsable hash: {err}"))?;
    // only accept the algorithm Thorium hashes with
    if parsed.algorithm != Algorithm::Argon2id.ident() {
        return Err(format!("algorithm {} isn't argon2id", parsed.algorithm));
    }
    // only accept the costs Thorium hashes with
    let params = argon2::Params::try_from(&parsed).map_err(|err| format!("bad costs: {err}"))?;
    let default = argon2::Params::default();
    if (params.m_cost(), params.t_cost(), params.p_cost())
        != (default.m_cost(), default.t_cost(), default.p_cost())
    {
        return Err(format!(
            "costs m={},t={},p={} aren't m={},t={},p={}",
            params.m_cost(),
            params.t_cost(),
            params.p_cost(),
            default.m_cost(),
            default.t_cost(),
            default.p_cost()
        ));
    }
    Ok(())
}

/// Limits how many password verifications run at once, since each one allocates the full
/// Argon2 memory cost and the blocking pool would otherwise run hundreds of them together
static VERIFY_PERMITS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(verify_permits())));

/// Get how many password verifications may run at once: one per available CPU
fn verify_permits() -> usize {
    // fall back to a single verification when the CPU count is unknown
    std::thread::available_parallelism().map_or(1, NonZeroUsize::get)
}

/// Run a blocking task once a permit is free, holding the permit until the task finishes
///
/// The permit moves into the task, so a request that is dropped mid-verify doesn't free its
/// permit while the verification still runs.
///
/// # Arguments
///
/// * `permits` - The permits that bound how many tasks run at once
/// * `task` - The blocking task to run
async fn bounded_blocking<T, F>(permits: &Arc<Semaphore>, task: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    // wait for a free permit
    let permit = Arc::clone(permits)
        .acquire_owned()
        .await
        .map_err(|err| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, Some(err.to_string())))?;
    // run our task on a blocking thread, releasing the permit when it finishes
    tokio::task::spawn_blocking(move || {
        let output = task();
        drop(permit);
        output
    })
    .await
    .map_err(|err| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, Some(err.to_string())))
}

/// Authenticate a user with basic auth stored in redis
///
/// # Arguments
///
/// * `username` - The name of the user we are authenticating
/// * `password` - The password to check against
/// * `password_hash` - The user's stored password hash
/// * `shared` - Shared objects in Thorium
#[instrument(name = "backends::user::basic_auth_redis", skip_all, err(Debug))]
async fn basic_auth_redis(
    username: &str,
    password: &str,
    password_hash: &str,
    shared: &Shared,
) -> Result<(), ApiError> {
    // refuse hashes whose algorithm or cost Thorium didn't choose before computing anything
    if let Err(reason) = check_password_hash(password_hash) {
        event!(
            Level::ERROR,
            user = username,
            msg = "Refusing stored password hash",
            reason = %reason
        );
        return unauthorized!();
    }
    // verify on a blocking thread so the hash doesn't stall the async workers
    let secret_key = shared.config.thorium.secret_key.clone();
    let password = password.to_owned();
    let password_hash = password_hash.to_owned();
    let verified = bounded_blocking(&VERIFY_PERMITS, move || -> Result<bool, ApiError> {
        // parse our password hash
        let parsed_hash = PasswordHash::new(&password_hash)?;
        // build an argon hasher
        let argon = Argon2::new_with_secret(
            secret_key.as_bytes(),
            Algorithm::Argon2id,
            Version::V0x13,
            argon2::Params::default(),
        )?;
        // check if this user provided the correct password
        Ok(argon
            .verify_password(password.as_bytes(), &parsed_hash)
            .is_ok())
    })
    .await??;
    // reject users that provided the wrong password
    if !verified {
        // log this authorization failure
        event!(Level::ERROR, user = username, msg = "Invalid password");
        // This user is not authorized to view this route/data
        return unauthorized!();
    }
    Ok(())
}

/// Reject an LDAP password that must never reach a simple bind
///
/// An LDAP simple bind with an empty password is treated by most directory servers as an
/// *unauthenticated bind* that succeeds, so a bind with a real DN and an empty password authenticates
/// as that user without knowing their credentials. The caller-level `password_auth` already
/// rejects empty passwords, but this bind site must reject them too as defense-in-depth so no future
/// caller can reach `simple_bind` with an empty password.
///
/// # Arguments
///
/// * `password` - The password about to be used for a simple bind
fn reject_empty_ldap_password(password: &str) -> Result<(), ApiError> {
    // an empty password would become an unauthenticated bind that spuriously succeeds
    if password.is_empty() {
        // refuse to attempt a bind that would authenticate as the user without a real password
        return unauthorized!();
    }
    Ok(())
}

/// Escape a username before it is interpolated into an LDAP search filter
///
/// `get_unix_info` builds its search filter with `format!`, so an unescaped username carrying LDAP
/// filter metacharacters (`(`, `)`, `*`, `\`, NUL) can alter the filter's meaning — a filter
/// injection that, depending on the directory, broadens the match or exfiltrates other users'
/// attributes. This centralizes the filter construction so the username is always escaped
/// per RFC 4515 before it reaches the directory.
///
/// # Arguments
///
/// * `search_filter_prepend` - The configured attribute prefix (e.g. `uid=`)
/// * `username` - The attacker-influenced username to embed in the filter
fn build_ldap_user_filter(search_filter_prepend: &str, username: &str) -> String {
    // escape the username per RFC 4515 before embedding it so filter metacharacters
    // (`*`, `(`, `)`, `\`, NUL) cannot alter the filter's meaning (LDAP filter injection)
    let escaped = ldap3::ldap_escape(username);
    // build the filter with the now-neutralized username
    format!("(&({search_filter_prepend}{escaped})(objectClass=*))")
}

/// Authenticate a user with basic auth using LDAP
///
/// # Arguments
///
/// * `username` - The name of the user to authenticate
/// * `password` - The password to authenticate with
/// * `shared` - Shared objects in Thorium
#[instrument(
    name = "backends::user::basic_auth_ldap",
    skip(password, shared),
    err(Debug)
)]
async fn basic_auth_ldap(
    username: &str,
    password: &str,
    shared: &Shared,
) -> Result<ldap3::Ldap, ApiError> {
    if let Some(ldap_conf) = &shared.config.thorium.auth.ldap {
        // reject empty passwords here too so a simple bind can never become an unauthenticated bind
        reject_empty_ldap_password(password)?;
        //  build an ldap connection
        //  we do this on demand instead of having it in shared because it nees to be mutable
        let (conn, mut ldap) = ldap!(ldap_conf).await?;
        ldap3::drive!(conn);
        // try to bind to ldap with this users creds
        let query = format!(
            "{}{username}{}",
            ldap_conf.user_prepend, ldap_conf.user_append
        );
        let res = ldap.simple_bind(&query, password).await?.success();
        // check if the bind failed or ot
        if let Err(err) = res {
            // log this auth failure
            event!(Level::ERROR, user = username, err = &err.to_string());
            // the bind failed return unauthorized
            return unauthorized!();
        }
        Ok(ldap)
    } else {
        unavailable!("ldap is not configured!".to_owned())
    }
}

/// Authenticate a user with basic auth stored in redis or using ldap
///
/// # Arguments
///
/// * `username` - The name of the user to authenticate
/// * `password` - The password to authenticate with
/// * `shared` - Shared objects in Thorium
#[instrument(
    name = "backends::users::password_auth",
    skip(password, shared),
    err(Debug)
)]
async fn password_auth(username: &str, password: &str, shared: &Shared) -> Result<User, ApiError> {
    // get the user doc we are authenticating against
    let mut possible = db::users::get(username, shared).await?;
    event!(
        Level::INFO,
        user = &possible.username,
        msg = "Attempting authentication",
    );
    // reject any empty passwords
    if password.is_empty() {
        // log an event that this user is trying to authenticate with an empty password
        event!(
            Level::WARN,
            msg = "Trying to authenticate with empty password"
        );
        return unauthorized!();
    }
    // try to authenticate against redis or ldap based on if a password is set
    if let Some(password_hash) = &possible.password {
        // a password is set use basic auth
        basic_auth_redis(username, password, password_hash, shared).await?;
    } else {
        // no password was set so use ldap
        let mut ldap = basic_auth_ldap(username, password, shared).await?;
        // if no unix info is set then try to get it and save it
        if possible.unix.is_none() {
            // get this users unix info from ldap
            let unix = get_unix_info(
                username,
                shared.config.thorium.auth.ldap.as_ref().unwrap(),
                &mut ldap,
            )
            .await?;
            // unbind our ldap socket
            ldap.unbind().await?;
            // set our new unix info
            possible.unix = Some(unix);
            // save the updated user object to redis
            db::users::save(&possible, shared).await?;
        }
    }

    // check if our token is expired and regenerate it if it is
    if possible.token_expiration < Utc::now() {
        event!(Level::INFO, msg = "refreshing token");
        // token is expired so generate a new one and bounce this user
        possible.regen_token(shared).await?;
    }
    Ok(possible)
}

/// Build an ldap connection with our system user for listing info (not auth checks)
#[instrument(name = "backends::users::bind_to_ldap_system_user", skip_all, fields(creds_set = ldap_conf.credentials.is_some()), err(Debug))]
pub async fn bind_to_ldap_system_user(
    ldap: &mut ldap3::Ldap,
    ldap_conf: &Ldap,
) -> Result<(), ApiError> {
    // if we have any system creds set then use them
    if let Some(creds) = &ldap_conf.credentials {
        // try to bind to ldap with this users creds
        let query = format!(
            "{}{}{}",
            ldap_conf.user_prepend, creds.user, ldap_conf.user_append
        );
        let res = ldap.simple_bind(&query, &creds.password).await?.success();
        // check if the bind failed or ot
        if let Err(err) = res {
            // log this auth failure
            event!(Level::ERROR, user = &creds.user, err = &err.to_string());
            // the bind failed return unavailable
            return unavailable!(format!("Failed to bind with user {}", &creds.user));
        }
    }
    Ok(())
}

/// Pull the unix info about a user from ldap
///
/// # Argumens
///
/// * `username` - The username to get info
/// * `conf` - The Thorium Ldap config
/// * `ldap` - A bound and authenticated ldap connection
#[instrument(name = "backends::users::get_unix_info", skip(conf, ldap), err(Debug))]
async fn get_unix_info(
    username: &str,
    conf: &Ldap,
    ldap: &mut ldap3::Ldap,
) -> Result<UnixInfo, ApiError> {
    // build a filter to get info on this user, escaping the username against filter injection
    let filter = build_ldap_user_filter(&conf.search_filter_prepend, username);
    // search for this users info in ldap
    let mut stream = ldap
        .streaming_search(&conf.scope, Scope::Subtree, &filter, vec!["*"])
        .await?;
    // crawl over the search entries for our user and pull their info
    if let Some(entry) = stream.next().await? {
        // try to cast this entry to strings instead of binary arrays
        let mut entry = SearchEntry::construct(entry);
        // get the group and user id if they exist
        let user = conf.user_unix_id.attr.get(&mut entry);
        let group = conf.group_unix_id.attr.get(&mut entry);
        // build UnixInfo object if they exist or error out
        if let (Some(user), Some(group)) = (user, group) {
            // cast our user and group ids
            let user = conf.user_unix_id.cast.cast(user)?;
            let group = conf.group_unix_id.cast.cast(group)?;
            return Ok(UnixInfo { user, group });
        }
        // we didn't get the attrs we expected so log what we got to jaeger
        let ldap_str = format!("{entry:#?}");
        event!(Level::ERROR, ldap_entry = ldap_str);
    }
    unavailable!(format!("Ldap did not return UNIX info for {}", username))
}

/// The different support auth methods
enum AuthMethods {
    /// Authenticate with a token
    Token(String),
    /// Authenticate with a password
    Password { username: String, password: String },
}

impl AuthMethods {
    /// Authenticates a user based on an auth header
    ///
    /// # Arguments
    ///
    /// * `verify_email` - Whether to require a verified email or not
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "User::authenticate", skip_all, err(Debug))]
    pub async fn authenticate(
        &self,
        verify_email: bool,
        shared: &Shared,
    ) -> Result<AuthedUser, ApiError> {
        // try to authenticate this user
        let user = match self {
            Self::Token(token) => token_auth(token, shared).await,
            Self::Password { username, password } => password_auth(username, password, shared)
                .await
                .map(AuthedUser::Full),
        }?;
        // reject disabled users no matter how they authenticated; this runs
        // after credential checks so unauthenticated callers never learn
        // whether an account is disabled
        if user.role == UserRole::Disabled {
            // this user has been disabled so reject this request
            return unauthorized!("This user has been disabled".to_owned());
        }
        // make sure this user's email has been verified
        if verify_email && !user.verified {
            // our user has not been verified yet so reject this request
            return unauthorized!("Email has not been verified".to_owned());
        }
        Ok(user)
    }

    /// Build our auth method from a str
    ///
    /// # Arguments
    ///
    /// * `raw` - The str to build our auth method from
    /// * `span` - The span to log traces under
    #[instrument(name = "User::from_str", skip_all, err(Debug))]
    pub fn from_str(raw: &str) -> Result<Self, ApiError> {
        // get our current span
        let span = Span::current();
        // get the first index where a space exists
        if let Some(index) = raw.find(' ') {
            // make sure this isn't the last character
            if raw.len() < index + 1 {
                event!(parent: span, Level::ERROR, error = "Auth header is too short", len = raw.len(), index = index);
                return unauthorized!();
            }
            // try to get the correct auth method
            match &raw[..index] {
                "token" | "Token" | "bearer" | "Bearer" => Self::token(&raw[index + 1..]),
                "basic" | "Basic" => Self::password(&raw[index + 1..]),
                _ => {
                    event!(parent: span, Level::ERROR, error = "Unknown auth type", auth_type = &raw[..index]);
                    unauthorized!()
                }
            }
        } else {
            event!(parent: span, Level::ERROR, error = "Couldn't find space to split auth header");
            unauthorized!()
        }
    }

    /// Builds the token auth method
    ///
    /// # Arguments
    ///
    /// * `raw` - The str to pull our token from
    fn token(raw: &str) -> Result<Self, ApiError> {
        // try to decode this token value
        let decoded = b64_decode(raw)?;
        Ok(AuthMethods::Token(decoded))
    }

    /// Builds the password auth method
    ///
    /// # Arguments
    ///
    /// * `raw` - The str to pull our username and password from
    fn password(raw: &str) -> Result<Self, ApiError> {
        // try to decode this token value
        let decoded = b64_decode(raw)?;
        // find the first ':' that we should split on
        if let Some(index) = decoded.find(':') {
            // split this decoded string into a username and password
            let username = decoded[..index].to_owned();
            let password = decoded[index + 1..].to_owned();
            Ok(AuthMethods::Password { username, password })
        } else {
            // we couldn't find a ':' to split on
            unauthorized!()
        }
    }
}

impl AiEndpointUpdate {
    /// Create a full endpoing from this update
    ///
    /// If all required fields are not set this will error
    pub fn to_endpoint(self) -> Result<AiEndpoint, ApiError> {
        // convert this update into a new endpoint
        match (self.url, self.api_key, self.model) {
            (Some(url), Some(api_key), Some(model)) => Ok(AiEndpoint {
                url,
                api_key,
                model,
            }),
            _ => bad!("Missing required fields for new endpoint".to_string()),
        }
    }

    /// Apply this update to an existing endpoint
    ///
    /// # Arguments
    ///
    /// * `endpoint` - The endpoint to apply this update too
    #[must_use]
    pub fn apply(mut self, mut endpoint: AiEndpoint) -> AiEndpoint {
        // update this endpoints settings
        update!(endpoint.url, self.url);
        update!(endpoint.api_key, self.api_key);
        update!(endpoint.model, self.model);
        endpoint
    }
}

/// What checking a user's model settings update needs from outside the user
#[derive(Debug, Default)]
pub(crate) struct UserModelFacts {
    /// The deployment's allowed model endpoints
    pub(crate) allowed: ModelsConf,
    /// The providers of the group endpoints the update names that the user can see
    pub(crate) group_endpoints: HashMap<Uuid, ModelProvider>,
    /// The model profiles the update names that the user can see
    pub(crate) profiles: HashSet<Uuid>,
}

/// Map a model validation message to a `400`
///
/// # Arguments
///
/// * `result` - The result of a model validation
fn bad_request<T>(result: Result<T, String>) -> Result<T, ApiError> {
    // turn any validation message into a bad request
    match result {
        Ok(value) => Ok(value),
        Err(msg) => bad!(msg),
    }
}

/// Get a visible entity, or nothing if it doesn't exist or the user can't see it
///
/// # Arguments
///
/// * `owner` - The user who must see the entity
/// * `id` - The entity's id
/// * `shared` - Shared Thorium objects
async fn visible_entity(
    owner: &User,
    id: Uuid,
    shared: &Shared,
) -> Result<Option<Entity>, ApiError> {
    // a missing or hidden entity is nothing
    match Entity::get(owner, id, shared).await {
        Ok(entity) => Ok(Some(entity)),
        Err(err) if err.status() == StatusCode::NOT_FOUND => Ok(None),
        Err(err) => Err(err),
    }
}

/// Build the stored form of a user credential from a submitted one
///
/// An omitted or empty secret keeps the stored one of the same kind; a new secret rotates the
/// credential and clears the bans a rejected or expired secret earned.
///
/// # Arguments
///
/// * `submitted` - The submitted credential
/// * `existing` - The stored credential for the same endpoint, if there is one
fn build_user_credential(
    submitted: UserCredentialRef,
    existing: Option<UserCredentialRef>,
) -> Result<UserCredentialRef, ApiError> {
    // the kind's metadata must be valid
    bad_request(submitted.kind.validate())?;
    // empty secrets are treated as missing
    let secret = submitted
        .secrets
        .primary
        .filter(|secret| !secret.is_empty());
    let session_token = submitted
        .secrets
        .session_token
        .filter(|token| !token.is_empty());
    // an assumed role holds no secret of its own
    let cred_type = submitted.kind.cred_type();
    if !submitted.kind.needs_secret() && secret.is_some() {
        return bad!(
            "an AwsAssumeRole credential holds no secret; its source credential does".to_owned()
        );
    }
    // only access keys have session tokens
    if session_token.is_some() && cred_type != CredentialType::AwsAccessKey {
        return bad!("only an AwsAccessKey credential takes a session token".to_owned());
    }
    // a stored credential of the same type keeps its id, secrets, and bans
    let kept = existing.filter(|existing| existing.kind.cred_type() == cred_type);
    let mut stored = match (secret, kept) {
        // a new secret rotates the credential
        (Some(secret), kept) => {
            let (id, mut bans) = match kept {
                Some(kept) => (kept.id, kept.bans),
                None => (submitted.id, HashMap::new()),
            };
            bans.retain(|_, ban| {
                !matches!(
                    ban.ban_kind,
                    ModelBanKind::CredentialRejected { .. } | ModelBanKind::CredentialExpired
                )
            });
            let secrets = crate::models::CredentialSecrets {
                fingerprint: Some(secret.fingerprint()),
                primary: Some(secret),
                session_token: None,
            };
            UserCredentialRef {
                id,
                kind: submitted.kind.clone(),
                secrets,
                expires: submitted.expires,
                rotated: Some(Utc::now()),
                bans,
            }
        }
        // no new secret keeps the stored one
        (None, Some(kept)) => UserCredentialRef {
            kind: submitted.kind.clone(),
            expires: submitted.expires,
            ..kept
        },
        // a new credential without a secret is only an assumed role
        (None, None) => UserCredentialRef {
            id: submitted.id,
            kind: submitted.kind.clone(),
            secrets: crate::models::CredentialSecrets::default(),
            expires: submitted.expires,
            rotated: None,
            bans: HashMap::new(),
        },
    };
    // a kind that needs a secret must end up with one
    if submitted.kind.needs_secret() && stored.secrets.primary.is_none() {
        return bad!(format!("a {cred_type} credential needs a secret"));
    }
    // a submitted session token replaces the stored one
    if let Some(token) = session_token {
        stored.secrets.session_token = Some(token);
    }
    Ok(stored)
}

/// Check the bans a user adds to their own settings
///
/// Users may add only `Generic` bans, and each ban id must be new across all of the user's bans,
/// since bans and their notifications are found by id alone.
///
/// # Arguments
///
/// * `settings` - The user's model settings
/// * `added` - The bans to add
fn check_user_bans(settings: &UserModelSettings, added: &[ModelBan]) -> Result<(), ApiError> {
    // the ids already used anywhere in the user's settings
    let mut used = settings.ban_index().into_keys().collect::<HashSet<Uuid>>();
    for ban in added {
        // only Generic bans can be set by hand on user settings
        if !matches!(ban.ban_kind, ModelBanKind::Generic(_)) {
            return bad!(format!(
                "only Generic bans can be added to your own model settings, not {}",
                ban.ban_kind.name()
            ));
        }
        // ban ids are unique across every endpoint, catalog, and credential
        if !used.insert(ban.id) {
            return bad!(format!(
                "A ban with id '{}' already exists. Bans cannot be updated, only added or removed.",
                ban.id
            ));
        }
    }
    Ok(())
}

/// Add some checked bans to a ban list
///
/// # Arguments
///
/// * `bans` - The ban list to add to
/// * `added` - The bans to add
fn add_bans(bans: &mut HashMap<Uuid, ModelBan>, added: Vec<ModelBan>) {
    // insert each ban by its id
    for ban in added {
        bans.insert(ban.id, ban);
    }
}

/// Check that a model id list holds no empty ids
///
/// # Arguments
///
/// * `field` - The name of the field, for errors
/// * `ids` - The model ids to check
fn check_model_ids(field: &str, ids: &BTreeSet<String>) -> Result<(), ApiError> {
    // a model id can't be empty
    if ids.iter().any(|id| id.trim().is_empty()) {
        return bad!(format!("{field} can't hold an empty model id"));
    }
    Ok(())
}

impl UserModelSettingsUpdate {
    /// Look up what checking this update needs from outside the user
    ///
    /// # Arguments
    ///
    /// * `owner` - The user whose settings are updated
    /// * `current` - The user's current model settings
    /// * `shared` - Shared Thorium objects
    pub(crate) async fn facts(
        &self,
        owner: &User,
        current: &UserModelSettings,
        shared: &Shared,
    ) -> Result<UserModelFacts, ApiError> {
        let mut facts = UserModelFacts::default();
        // endpoints are checked against the deployment's allowed list
        if !self.endpoints.is_empty() {
            facts.allowed = db::system::get_settings(shared).await?.models;
        }
        // credentials for endpoints that aren't the user's own name visible group endpoints
        for id in self.credentials.keys() {
            if current.endpoints.contains_key(id) || self.endpoints.contains_key(id) {
                continue;
            }
            if let Some(entity) = visible_entity(owner, *id, shared).await?
                && let EntityMetadata::ModelEndpoint(meta) = entity.metadata
            {
                facts.group_endpoints.insert(*id, meta.provider);
            }
        }
        // default profiles must be visible profiles
        for id in self.default_profiles.iter().flatten() {
            if let Some(entity) = visible_entity(owner, *id, shared).await?
                && entity.kind == EntityKinds::ModelProfile
            {
                facts.profiles.insert(*id);
            }
        }
        Ok(facts)
    }

    /// Apply this update to a user's model settings, changing nothing unless every check passes
    ///
    /// # Arguments
    ///
    /// * `settings` - The user's model settings
    /// * `facts` - What the checks need from outside the user
    #[allow(clippy::too_many_lines)]
    pub(crate) fn apply(
        self,
        settings: &mut UserModelSettings,
        facts: &UserModelFacts,
    ) -> Result<(), ApiError> {
        // change a copy so nothing is applied unless every check passes
        let mut next = settings.clone();
        // removed endpoints take their credentials with them
        for id in &self.remove_endpoints {
            if next.endpoints.remove(id).is_none() {
                return not_found!(format!("{id} isn't one of your model endpoints"));
            }
            next.credentials.remove(id);
        }
        // keep which endpoints are replaced, since their kept credentials are checked later
        let replaced = self.endpoints.keys().copied().collect::<Vec<Uuid>>();
        // added or replaced endpoints must be valid and allowed
        for (id, mut endpoint) in self.endpoints {
            // an endpoint needs a name
            if endpoint.name.trim().is_empty() {
                return bad!(format!("model endpoint {id} needs a name"));
            }
            // its provider must be valid and on the deployment's allowed list
            bad_request(endpoint.provider.validate())?;
            bad_request(facts.allowed.check(&endpoint.name, &endpoint.provider))?;
            // its catalog can't repeat a model id
            let mut seen = HashSet::new();
            for entry in &endpoint.catalog {
                if entry.model_id.trim().is_empty() {
                    return bad!("a catalog entry's model_id can't be empty".to_owned());
                }
                if !seen.insert(entry.model_id.clone()) {
                    return bad!(format!(
                        "model_id `{}` is listed more than once",
                        entry.model_id
                    ));
                }
            }
            endpoint
                .catalog
                .sort_by(|left, right| left.model_id.cmp(&right.model_id));
            endpoint.mark_manual();
            check_model_ids("disabled_models", &endpoint.disabled_models)?;
            // bans and refresh state are kept by Thorium, never submitted
            match next.endpoints.get(&id) {
                Some(existing) => {
                    // an endpoint that passed the whitelist loses its whitelist ban
                    endpoint.bans.clone_from(&existing.bans);
                    sync_whitelist_ban(&mut endpoint.bans, true);
                    endpoint.refresh = existing.refresh.clone();
                    for entry in &mut endpoint.catalog {
                        entry.bans = existing
                            .catalog
                            .iter()
                            .find(|old| old.model_id == entry.model_id)
                            .map_or_default(|old| old.bans.clone());
                    }
                }
                None => {
                    endpoint.bans.clear();
                    endpoint.refresh = ModelRefreshStatus::default();
                    for entry in &mut endpoint.catalog {
                        entry.bans.clear();
                    }
                }
            }
            next.endpoints.insert(id, endpoint);
        }
        // removed credentials must exist
        for id in &self.remove_credentials {
            if next.credentials.remove(id).is_none() {
                return not_found!(format!("you have no credential for model endpoint {id}"));
            }
        }
        // added or rotated credentials must fit their endpoint's provider
        let touched = self.credentials.keys().copied().collect::<HashSet<Uuid>>();
        for (endpoint, credential) in self.credentials {
            let provider = match next.endpoints.get(&endpoint) {
                Some(own) => &own.provider,
                None => match facts.group_endpoints.get(&endpoint) {
                    Some(provider) => provider,
                    None => {
                        return bad!(format!(
                            "{endpoint} isn't one of your model endpoints or a model endpoint you can see"
                        ));
                    }
                },
            };
            bad_request(provider.check_credential(&credential.kind))?;
            let existing = next.credentials.get(&endpoint).cloned();
            let stored = build_user_credential(credential, existing)?;
            next.credentials.insert(endpoint, stored);
        }
        // a replaced endpoint's kept credential must fit its provider
        for id in replaced.iter().filter(|id| !touched.contains(id)) {
            if let (Some(own), Some(credential)) =
                (next.endpoints.get(id), next.credentials.get(id))
            {
                bad_request(own.provider.check_credential(&credential.kind))?;
            }
        }
        // credential ids are unique
        let mut types = HashMap::new();
        for credential in next.credentials.values() {
            if types
                .insert(credential.id, credential.kind.cred_type())
                .is_some()
            {
                return bad!(format!(
                    "credential id {} is used more than once",
                    credential.id
                ));
            }
        }
        // assumed roles must name another of the user's access keys
        for (endpoint, credential) in &mut next.credentials {
            let Some(source) = credential.kind.role_source() else {
                continue;
            };
            let resolves = source != credential.id
                && types.get(&source) == Some(&CredentialType::AwsAccessKey);
            if touched.contains(endpoint) && !resolves {
                return bad!(format!(
                    "AwsAssumeRole.source {source} must name another of your AwsAccessKey credentials"
                ));
            }
            // a role whose source went away is banned until it's repaired
            sync_broken_reference(&mut credential.bans, Some(source), !resolves);
        }
        // default profiles must be visible profiles named once
        if let Some(profiles) = self.default_profiles {
            let mut seen = HashSet::new();
            for id in &profiles {
                if !facts.profiles.contains(id) {
                    return bad!(format!("{id} isn't a model profile you can see"));
                }
                if !seen.insert(*id) {
                    return bad!(format!("model profile {id} is listed more than once"));
                }
            }
            next.default_profiles = profiles;
        }
        // ban changes keyed by an endpoint id
        for (endpoint, update) in self.bans {
            apply_keyed_bans(&mut next, endpoint, update)?;
        }
        // ban changes on catalog models of own endpoints
        for (endpoint, models) in self.model_bans {
            for (model_id, update) in models {
                check_user_bans(&next, &update.bans_added)?;
                let Some(own) = next.endpoints.get_mut(&endpoint) else {
                    return bad!(format!("{endpoint} isn't one of your model endpoints"));
                };
                let Some(entry) = own
                    .catalog
                    .iter_mut()
                    .find(|entry| entry.model_id == model_id)
                else {
                    return not_found!(format!(
                        "model_id `{model_id}` isn't in the catalog of your model endpoint `{}`",
                        own.name
                    ));
                };
                remove_bans(&mut entry.bans, &update.bans_removed)?;
                add_bans(&mut entry.bans, update.bans_added);
            }
        }
        // ban changes on credentials
        for (endpoint, update) in self.credential_bans {
            check_user_bans(&next, &update.bans_added)?;
            let Some(credential) = next.credentials.get_mut(&endpoint) else {
                return not_found!(format!(
                    "you have no credential for model endpoint {endpoint}"
                ));
            };
            remove_bans(&mut credential.bans, &update.bans_removed)?;
            add_bans(&mut credential.bans, update.bans_added);
        }
        // disabled model ids change on own endpoints only
        let ids = self
            .disable_models
            .keys()
            .chain(self.enable_models.keys())
            .copied()
            .collect::<BTreeSet<Uuid>>();
        for endpoint in ids {
            let disable = self
                .disable_models
                .get(&endpoint)
                .cloned()
                .unwrap_or_default();
            let enable = self
                .enable_models
                .get(&endpoint)
                .cloned()
                .unwrap_or_default();
            check_model_ids("disable_models", &disable)?;
            check_model_ids("enable_models", &enable)?;
            if let Some(both) = disable.intersection(&enable).next() {
                return bad!(format!(
                    "model id `{both}` can't be disabled and enabled at once"
                ));
            }
            let Some(own) = next.endpoints.get_mut(&endpoint) else {
                return bad!(format!(
                    "{endpoint} isn't one of your model endpoints; only your own endpoints disable models here"
                ));
            };
            own.disabled_models.extend(disable);
            own.disabled_models.retain(|id| !enable.contains(id));
        }
        // refresh reports merge into own endpoints and the credentials the refreshes used
        let UserModelSettings {
            endpoints,
            credentials,
            ..
        } = &mut next;
        for (endpoint, report) in &self.refresh_reports {
            check_report(report)?;
            let own = endpoints.get_mut(endpoint);
            let credential = credentials.get_mut(endpoint);
            // a report for an endpoint the user has nothing for is refused
            if own.is_none() && credential.is_none() {
                return not_found!(format!(
                    "{endpoint} isn't one of your model endpoints and you have no credential for it"
                ));
            }
            if let Some(own) = own {
                merge_refresh_report(
                    &mut RefreshedEndpoint {
                        catalog: &mut own.catalog,
                        bans: &mut own.bans,
                        refresh: &mut own.refresh,
                    },
                    report,
                );
            }
            // only a report naming this credential bans or clears it
            if let Some(credential) = credential
                && report.credential == Some(ModelConfigRef::User(credential.id))
            {
                apply_credential_effect(&mut credential.bans, &credential_effect(report));
            }
        }
        // every check passed
        *settings = next;
        Ok(())
    }

    /// Get the endpoints this update sets a user's own endpoint or credential for, each once
    ///
    /// Each one needs a refresh once the update is saved; removals and reports need none.
    #[must_use]
    pub(crate) fn refreshed_endpoints(&self) -> BTreeMap<Uuid, bool> {
        // note each endpoint, and whether its credential was set
        let mut refreshed = BTreeMap::new();
        for id in self.endpoints.keys() {
            refreshed.insert(*id, false);
        }
        for id in self.credentials.keys() {
            refreshed.insert(*id, true);
        }
        refreshed
    }
}

/// Remove bans by id from a ban list, refusing ids it doesn't hold
///
/// # Arguments
///
/// * `bans` - The ban list
/// * `removed` - The ids of the bans to remove
fn remove_bans(bans: &mut HashMap<Uuid, ModelBan>, removed: &[Uuid]) -> Result<(), ApiError> {
    // every ban to remove must exist
    for id in removed {
        if bans.remove(id).is_none() {
            return not_found!(format!("The ban with id '{id}' doesn't exist"));
        }
    }
    Ok(())
}

/// Apply ban changes keyed by an endpoint id
///
/// Added bans go to the own endpoint, or to the credential when the id names a group endpoint;
/// removed ids are looked up on the endpoint, its catalog, and the credential.
///
/// # Arguments
///
/// * `settings` - The user's model settings
/// * `endpoint` - The endpoint id
/// * `update` - The ban changes
fn apply_keyed_bans(
    settings: &mut UserModelSettings,
    endpoint: Uuid,
    update: ModelBanUpdate,
) -> Result<(), ApiError> {
    // check the added bans first
    check_user_bans(settings, &update.bans_added)?;
    // remove each ban from whichever item holds it
    for id in &update.bans_removed {
        let mut removed = false;
        if let Some(own) = settings.endpoints.get_mut(&endpoint) {
            removed = own.bans.remove(id).is_some()
                || own
                    .catalog
                    .iter_mut()
                    .any(|entry| entry.bans.remove(id).is_some());
        }
        if !removed && let Some(credential) = settings.credentials.get_mut(&endpoint) {
            removed = credential.bans.remove(id).is_some();
        }
        if !removed {
            return not_found!(format!("The ban with id '{id}' doesn't exist"));
        }
    }
    // add the bans to the own endpoint or the credential
    if update.bans_added.is_empty() {
        return Ok(());
    }
    if let Some(own) = settings.endpoints.get_mut(&endpoint) {
        add_bans(&mut own.bans, update.bans_added);
    } else if let Some(credential) = settings.credentials.get_mut(&endpoint) {
        add_bans(&mut credential.bans, update.bans_added);
    } else {
        return bad!(format!(
            "{endpoint} isn't one of your model endpoints and you have no credential for it"
        ));
    }
    Ok(())
}

/// Get every ban in a user's model settings by id
///
/// # Arguments
///
/// * `settings` - The user's settings
fn model_ban_index(settings: &UserSettings) -> HashMap<Uuid, ScopedModelBan> {
    // users without AI settings have no bans
    settings
        .ai
        .as_ref()
        .map_or_default(|ai| ai.models.ban_index())
}

/// Write a notification for each added user ban and delete the one of each removed ban
///
/// # Arguments
///
/// * `user` - The user the bans belong to
/// * `before` - The user's bans before the change
/// * `shared` - Shared Thorium objects
async fn notify_model_bans(
    user: &User,
    before: &HashMap<Uuid, ScopedModelBan>,
    shared: &Shared,
) -> Result<(), ApiError> {
    // find the bans that appeared and the ones that disappeared
    let after = model_ban_index(&user.settings);
    let added = after
        .iter()
        .filter(|(id, _)| !before.contains_key(id))
        .map(|(_, ban)| ban.clone())
        .collect::<Vec<ScopedModelBan>>();
    let removed = before
        .keys()
        .filter(|id| !after.contains_key(id))
        .copied()
        .collect::<Vec<Uuid>>();
    // write the user's notifications under their username
    if !added.is_empty() || !removed.is_empty() {
        user.update_ban_notifications(&user.username, &added, &removed, shared)
            .await?;
    }
    Ok(())
}

/// Delete the ban notifications just written for a user whose stored settings no longer hold
/// those bans
///
/// The user can be deleted, or deleted and registered again under the same username, between
/// the scan's re-read and its notification write, and a notification left behind would be shown
/// to the next account with that username.
///
/// # Arguments
///
/// * `user` - The user the notifications were written for, with the settings the scan wrote
/// * `before` - The user's bans before the scan changed them
/// * `shared` - Shared Thorium objects
async fn clear_orphaned_ban_notifications(
    user: &User,
    before: &HashMap<Uuid, ScopedModelBan>,
    shared: &Shared,
) -> Result<(), ApiError> {
    // find the bans the scan added, which are the ones it wrote notifications for
    let added = model_ban_index(&user.settings)
        .into_keys()
        .filter(|id| !before.contains_key(id))
        .collect::<Vec<Uuid>>();
    // there is nothing to check without new notifications
    if added.is_empty() {
        return Ok(());
    }
    // read the bans stored under this username now; a deleted user has none
    let stored = match db::users::get_raw_settings(&user.username, shared).await? {
        Some(raw) => model_ban_index(&db::users::parse_stored_settings(&raw)?),
        None => HashMap::new(),
    };
    // delete the notification of every added ban the stored user doesn't have
    let orphaned = added
        .into_iter()
        .filter(|id| !stored.contains_key(id))
        .collect::<Vec<Uuid>>();
    if !orphaned.is_empty() {
        user.update_ban_notifications::<ScopedModelBan>(&user.username, &[], &orphaned, shared)
            .await?;
    }
    Ok(())
}

/// Ban every user's own model endpoints outside the allowed endpoint list and unban those inside
///
/// # Arguments
///
/// * `conf` - The allowed endpoint list
/// * `shared` - Shared Thorium objects
#[instrument(name = "users::scan_whitelist", skip_all, err(Debug))]
pub(crate) async fn scan_whitelist(conf: &ModelsConf, shared: &Shared) -> Result<(), ApiError> {
    // check every user's own endpoints
    for username in db::users::list(shared).await? {
        // read and write each user until no other change lands between the two
        loop {
            // read the user's settings as stored, skipping users deleted since the list
            let Some(raw) = db::users::get_raw_settings(&username, shared).await? else {
                break;
            };
            // a user whose settings don't parse can't be loaded or use its endpoints, so it is
            // skipped rather than failing the scan, and the API's startup, for every other user
            let mut settings = match db::users::parse_stored_settings(&raw) {
                Ok(settings) => settings,
                Err(error) => {
                    event!(
                        Level::ERROR,
                        user = username,
                        error = ?error,
                        msg = "Skipping a user whose settings don't parse in the model whitelist scan"
                    );
                    break;
                }
            };
            // sync the whitelist ban of each of the user's own endpoints
            let before = model_ban_index(&settings);
            let mut changed = false;
            if let Some(ai) = &mut settings.ai {
                for endpoint in ai.models.endpoints.values_mut() {
                    changed |=
                        sync_whitelist_ban(&mut endpoint.bans, conf.allows(&endpoint.provider));
                }
            }
            // only users whose bans changed are written and notified
            if !changed {
                break;
            }
            // let a test change the user between the read and the write
            #[cfg(feature = "test-utilities")]
            crate::test_utilities::faults::wait_if_scan_paused(&username).await;
            // write only the settings, and only if nothing changed them since the read
            if !db::users::swap_settings(&username, &raw, &settings, shared).await? {
                continue;
            }
            // notify the user about the bans that changed, unless they were just deleted
            match db::users::get(&username, shared).await {
                Ok(mut user) => {
                    // let a test delete the user between the re-read and the notification write
                    #[cfg(feature = "test-utilities")]
                    crate::test_utilities::faults::wait_if_scan_notify_paused(&username).await;
                    user.settings = settings;
                    notify_model_bans(&user, &before, shared).await?;
                    // drop the notifications just written if the user was deleted or replaced
                    // since the re-read
                    clear_orphaned_ban_notifications(&user, &before, shared).await?;
                }
                Err(err) if err.status() == StatusCode::NOT_FOUND => (),
                Err(err) => return Err(err),
            }
            break;
        }
    }
    Ok(())
}

impl AiSettingsUpdate {
    /// Apply any updated AI settings
    ///
    /// # Arguments
    ///
    /// * `existing` - Any existing settings to update
    /// * `facts` - What checking a model settings update needs from outside the user
    pub(crate) fn apply(
        mut self,
        existing: &mut Option<AiSettings>,
        facts: &UserModelFacts,
    ) -> Result<(), ApiError> {
        // the model settings update is applied after the legacy endpoints
        let models = self.models.take();
        // an update of only model settings needs no legacy default endpoint
        let models_only = models.is_some()
            && self.default_endpoint.is_none()
            && self.endpoints.is_empty()
            && self.remove_endpoints.is_empty();
        // check if we have existing existing to update
        let mut settings = match existing.take() {
            Some(settings) => settings,
            None if models_only => AiSettings::default(),
            None => {
                // we don't have any existing settings so well need to make new ones
                // We have to make sure that users provide us enough info to build our new settings
                let Some(default_endpoint) = self.default_endpoint.take() else {
                    return bad!(
                        "A default endpoint must be provided if no previous AI settings were set"
                            .to_owned()
                    );
                };
                // make sure they provided us settings for our default endpoint
                let endpoint = match self.endpoints.remove(&default_endpoint) {
                    // turn this endpoint update into an endpoint
                    Some(endpoint_update) => endpoint_update.to_endpoint()?,
                    None => {
                        return bad!(format!(
                            "Settings for {default_endpoint} must be specified to set it as a default"
                        ));
                    }
                };
                // insert our default endpoints settings into a map
                let mut endpoints = HashMap::with_capacity(self.endpoints.len());
                // add our new default endpoint
                endpoints.insert(default_endpoint.clone(), endpoint);
                // build the base ai settings object
                AiSettings {
                    endpoints,
                    default_endpoint,
                    models: UserModelSettings::default(),
                }
            }
        };
        // apply the rest of our updates to our settings
        update!(settings.default_endpoint, self.default_endpoint);
        // apply our endpoint updates
        for (name, endpoint_update) in self.endpoints {
            // get and update this endpoint if it already exists or create a new one
            let endpoint = match settings.endpoints.remove(&name) {
                Some(endpoint) => endpoint_update.apply(endpoint),
                // this endpoint does not yet exist so create it
                None => endpoint_update.to_endpoint()?,
            };
            // reinsert our endpoint
            settings.endpoints.insert(name, endpoint);
        }
        // drop any endpoints that we want to delete
        settings
            .endpoints
            .retain(|name, _| !self.remove_endpoints.contains(name));
        // apply the model settings update
        if let Some(models) = models {
            models.apply(&mut settings.models, facts)?;
        }
        // if we have no more ai endpoints then keep only model settings, if any
        if settings.endpoints.is_empty() {
            // clear our ai settings when there are no model settings either
            *existing = if settings.models.is_empty() {
                None
            } else {
                Some(settings)
            };
        } else {
            // make sure we still have a default endpoint
            if !settings.endpoints.contains_key(&settings.default_endpoint) {
                return bad!(format!(
                    "Please change your default endpoint from {} before removing its config!",
                    settings.default_endpoint
                ));
            }
            // set our new ai settings for this user
            *existing = Some(settings);
        }
        Ok(())
    }
}

impl UserSettingsUpdate {
    /// Look up what checking this update's model settings needs from outside the user
    ///
    /// # Arguments
    ///
    /// * `owner` - The user whose settings are updated
    /// * `shared` - Shared Thorium objects
    pub(crate) async fn model_facts(
        &self,
        owner: &User,
        shared: &Shared,
    ) -> Result<UserModelFacts, ApiError> {
        // only model settings updates need outside facts
        match self.ai.as_ref().and_then(|ai| ai.models.as_ref()) {
            Some(models) => {
                let current = owner
                    .settings
                    .ai
                    .as_ref()
                    .map_or_default(|ai| ai.models.clone());
                models.facts(owner, &current, shared).await
            }
            None => Ok(UserModelFacts::default()),
        }
    }

    /// Apply any updated settings to this user
    ///
    /// # Arguments
    ///
    /// * `settings` - The user settings to update
    /// * `facts` - What checking a model settings update needs from outside the user
    pub(crate) fn apply(
        mut self,
        settings: &mut UserSettings,
        facts: &UserModelFacts,
    ) -> Result<(), ApiError> {
        // update our theme if an update was set
        update!(settings.theme, self.theme);
        // apply any AI settings updates
        if let Some(ai_update) = self.ai {
            ai_update.apply(&mut settings.ai, facts)?;
        }
        Ok(())
    }
}

/// What applying a settings update to a user needs done once the user is saved
pub(crate) struct AppliedSettings {
    /// The user's model bans before the update
    before: HashMap<Uuid, ScopedModelBan>,
    /// The endpoints to refresh, and whether each one's credential was set
    refreshed: BTreeMap<Uuid, bool>,
}

impl User {
    /// Apply a settings update to this user and get what's needed once it's saved
    ///
    /// # Arguments
    ///
    /// * `update` - The settings update to apply
    /// * `shared` - Shared Thorium objects
    async fn apply_settings(
        &mut self,
        update: UserSettingsUpdate,
        shared: &Shared,
    ) -> Result<AppliedSettings, ApiError> {
        // look up what the model checks need, then apply the update
        let facts = update.model_facts(self, shared).await?;
        let before = model_ban_index(&self.settings);
        let refreshed = update
            .ai
            .as_ref()
            .and_then(|ai| ai.models.as_ref())
            .map_or_default(UserModelSettingsUpdate::refreshed_endpoints);
        update.apply(&mut self.settings, &facts)?;
        Ok(AppliedSettings { before, refreshed })
    }

    /// Notify a saved user of their changed model bans and queue the refreshes their update needs
    ///
    /// # Arguments
    ///
    /// * `caller` - The user who sent the update
    /// * `applied` - What applying the update needs done
    /// * `shared` - Shared Thorium objects
    async fn finish_settings(
        &self,
        caller: &User,
        applied: AppliedSettings,
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // write the notifications of any model bans the update changed
        notify_model_bans(self, &applied.before, shared).await?;
        // refresh each endpoint whose own endpoint or credential was set
        let models = self.settings.ai.as_ref().map(|ai| &ai.models);
        let credentials = models.map(|models| &models.credentials);
        let conf = if applied.refreshed.is_empty() {
            None
        } else {
            Some(db::system::get_settings(shared).await?.models)
        };
        for (endpoint, credential_set) in applied.refreshed {
            // skip an endpoint the deployment no longer allows
            let allowed = match (
                conf.as_ref(),
                models.and_then(|models| models.endpoints.get(&endpoint)),
            ) {
                (Some(conf), Some(own)) => refresh_allowed(conf, &own.provider, &own.bans),
                (Some(conf), None) => refreshable_endpoint(endpoint, conf, shared)
                    .await?
                    .is_some(),
                (None, _) => false,
            };
            if !allowed {
                continue;
            }
            let reason = match credentials.and_then(|creds| creds.get(&endpoint)) {
                Some(credential) if credential_set => ModelRefreshReason::CredentialChanged {
                    credential: ModelConfigRef::User(credential.id),
                },
                _ => ModelRefreshReason::EndpointChanged,
            };
            let target = ModelRefreshTarget::UserEndpoint {
                user: self.username.clone(),
                endpoint,
            };
            Event::queue_model_refresh(caller, target, reason, shared).await?;
        }
        Ok(())
    }
}

/// Check whether a presented secret key matches the configured secret key
///
/// The secret key gates admin/analyst self-registration and other privileged bootstrap paths, so
/// the comparison must not leak the key one byte at a time through its timing. This helper
/// centralizes the comparison so the constant-time compare (which always inspects every byte
/// regardless of where the first mismatch is) lives in one place.
///
/// # Arguments
///
/// * `configured` - The secret key from the Thorium config
/// * `presented` - The secret key presented in the request
fn secret_key_matches(configured: &str, presented: &str) -> bool {
    // compare in constant time so we don't leak the key one byte at a time through timing
    constant_time_eq::constant_time_eq(configured.as_bytes(), presented.as_bytes())
}

impl User {
    /// Creates a new user in the backend
    ///
    /// # Arguments
    ///
    /// * `req` - The user registration request
    /// * `key` - The secret key from the request
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "User::create", skip_all, fields(user = req.username, key_set = key.is_some()), err(Debug))]
    pub async fn create(
        mut req: UserCreate,
        key: Option<Key>,
        shared: &Shared,
    ) -> Result<User, ApiError> {
        // ensure username is alphanumeric
        bounder::string_lower(&req.username, "username", 1, 50)?;
        // make sure this isn't using any reserved usernames
        if req.username == "external" {
            return bad!("external is a reserved username".to_owned());
        }
        // creating an already disabled user makes no sense so reject it
        if req.role == UserRole::Disabled {
            return bad!("Users cannot be created with the Disabled role".to_owned());
        }
        // make sure this user doesn't already exist
        if User::exists(&req.username, shared).await? {
            return conflict!(format!("User {} already exists", req.username));
        }
        // only allow users with the secret key to create admins/analysts/developers/disabled accounts
        if req.role != UserRole::User || req.local || req.skip_verification {
            // bounce users without the key
            if let Some(key) = key {
                // return unauthorized for users with invalid keys
                if !secret_key_matches(&shared.config.thorium.secret_key, &key.key) {
                    return unauthorized!();
                }
            } else {
                return unauthorized!();
            }
        }
        // if ldap is configured then authenticated against ldap and pull unix info
        let (password, unix) = match (&shared.config.thorium.auth.ldap, req.local) {
            // ldap config is setup and a local account was not requested
            (Some(conf), false) => {
                // authenticate against ldap
                let mut ldap = basic_auth_ldap(&req.username, &req.password, shared).await?;
                // get unix info for this user
                let unix = get_unix_info(&req.username, conf, &mut ldap).await?;
                // unbind our ldap socket
                ldap.unbind().await?;
                // sync this users data
                (None, Some(unix))
            }
            // ldap is not configured or a local account was requested
            (_, _) => {
                // get password from request and replace it with an empty str
                let pw = std::mem::take(&mut req.password);
                // get our secret key
                let key = &shared.config.thorium.secret_key;
                // hash password
                (Some(hash_pw!(pw, key)), None)
            }
        };
        // create user object
        let mut cast = User {
            username: req.username,
            password,
            email: req.email,
            groups: Vec::default(),
            actual_groups: Vec::default(),
            role: req.role,
            token: token!(),
            unix,
            token_expiration: token_expire!(shared),
            settings: req.settings,
            verified: false,
            verification_token: None,
            verification_sent: None,
            aliases: HashMap::default(),
            image: None,
        };
        // send a verification email if needed
        match (req.skip_verification, &shared.email) {
            (true, _) | (_, None) => cast.verified = true,
            (false, Some(client)) => {
                // send our verification email
                cast.send_verification_email(client, shared).await?;
            }
        }
        // let a test hold this create between its existing-user check and the user write
        #[cfg(feature = "test-utilities")]
        crate::test_utilities::faults::wait_if_user_create_paused(&cast.username).await;
        // inject user into the backend
        let user = db::users::create(cast, shared).await?;
        // sync all groups in ldap if ldap is enabled
        if shared.config.thorium.auth.ldap.is_some() {
            // sync ldap data for all groups
            Group::sync_ldap(shared).await?;
        }
        Ok(user)
    }

    /// Send a verification email to an unverified user
    ///
    /// # Arguments
    ///
    /// *
    pub async fn send_verification_email(
        &mut self,
        client: &EmailClient,
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // check if this user has an approved email
        if let Some(approved) = &client.approved
            && !approved.is_match(&self.email)
        {
            // reject any emails that are not approved
            return unauthorized!(format!("{} is not an approved email", self.email));
        }
        // if they are not yet verified then send the verification email
        if self.verified {
            // this user is already verified so return a conflict
            return conflict!(format!(
                "{} has already verified their email",
                &self.username
            ));
        }
        // make sure we have email verification settings
        let Some(email_conf) = &shared.config.thorium.auth.email else {
            return unavailable!("Email verification is not enabled".to_owned());
        };
        // make sure this user has not recently sent a verification email
        // this helps prevent using Thorium to spam unverified users
        email_conf.can_send_verification(self)?;
        // generate a special token for our email verification
        let verification_token = token!();
        // save this verification token to redis
        db::users::set_verification_token(&self.username, &verification_token, shared).await?;
        // build our verification link to embed in the email
        let link = format!(
            "{}/users/verify/{}/email/{}",
            email_conf.base_url, self.username, verification_token
        );
        // update our user object
        self.verification_token = Some(verification_token);
        self.verification_sent = Some(Utc::now());
        // build the subject for email verification email
        let subject = "🦀🎉 Welcome to Thorium 🎉🦀".to_owned();
        // build a body with our verification email
        let body = format!(
            "Please verify your Thorium account by clicking on the following link:\n\n{link}"
        );
        // send our verification email
        client.send(&self.email, subject, body).await
    }

    /// Verify an email for a user
    ///
    /// # Arguments
    ///
    /// * `verification_token` - The verification token to check
    pub async fn verify_email(
        &mut self,
        verification_token: &String,
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // check if our verification token matches
        if Some(verification_token) == self.verification_token.as_ref() {
            // update our user object
            self.verified = true;
            self.verification_token = None;
            // clear this users verification token and set them as verified in redis
            db::users::clear_verification_token(&self.username, shared).await?;
            Ok(())
        } else {
            // this is the wrong verification token
            unauthorized!()
        }
    }

    /// Get a [`User`] by username
    ///
    /// This should only be used when absolutely neccasary as it bypasses all authentication checks
    ///
    /// # Arguments
    ///
    /// * `username` - The username of the user to get
    /// * `shared` - Shared Thorium objects
    pub async fn force_get(username: &str, shared: &Shared) -> Result<User, ApiError> {
        // get user
        db::users::get(username, shared).await
    }

    /// Lists usernames
    ///
    /// # Arguments
    ///
    /// * `shared` - Shared Thorium objects
    pub async fn list(&self, shared: &Shared) -> Result<Vec<String>, ApiError> {
        // list all usernames
        db::users::list(shared).await
    }

    /// Lists users with details
    ///
    /// # Arguments
    ///
    /// * `shared` - Shared Thorium objects
    pub async fn list_details(&self, shared: &Shared) -> Result<Vec<ScrubbedUser>, ApiError> {
        // only admins can list user details
        is_admin!(self);
        // just get a backup since we want all users anyways
        let (users, problems) = db::users::backup(shared).await?;
        // a user that doesn't parse fails the listing
        if let Some(problem) = problems.first() {
            return bad!(format!("{}: {}", problem.key, problem.error));
        }
        // cast these users to scrubbed users
        let scrubbed = users.into_iter().map(ScrubbedUser::from).collect();
        Ok(scrubbed)
    }

    /// Checks if a vector of users exist
    ///
    /// # Arguments
    ///
    /// * `username` - the names of the users to check
    /// * `shared` - Shared Thorium objects
    pub async fn exists_many(usernames: &HashSet<String>, shared: &Shared) -> Result<(), ApiError> {
        // check if a set of users exists
        db::users::exists_many(usernames, shared).await
    }

    /// Checks if a user exists
    ///
    /// Returns true if this user exists.
    ///
    /// # Arguments
    ///
    /// * `username` - the name of a user to check
    /// * `shared` - Shared Thorium objects
    pub async fn exists(username: &str, shared: &Shared) -> Result<bool, ApiError> {
        // check if this user exists
        db::users::exists(username, shared).await
    }

    /// Deletes a user from Thorium
    ///
    /// # Arguments
    ///
    /// * `user` - The user requesting this delete
    /// * `delete` - The username to delete
    /// * `shared` - Shared Thorium objects
    pub async fn delete(user: User, delete: &str, shared: &Shared) -> Result<(), ApiError> {
        // only admins or the user in question can delete their user
        // get the users data to delete
        let target = match (user.is_admin(), user.username == delete) {
            // were an admin deleting ourselves or we aren't and admin and were deleting ourselves
            (true | false, true) => user,
            // were an admin deleting another user
            (true, false) => Self::force_get(delete, shared).await?,
            // we aren't an admin and were aren't deleting ourselves
            _ => return unauthorized!(),
        };
        // grab any profile icon path so we can clean it out of S3 after the user is gone
        let image = target.image.clone();
        // clear the user's notifications while the username is still theirs, so a new user of
        // the same name created once it's free never loses notifications to this delete
        target
            .delete_all_notifications(&target.username, shared)
            .await?;
        // delete user in the background
        db::users::delete(&target, shared).await?;
        // let a test hold this delete once the user is removed
        #[cfg(feature = "test-utilities")]
        crate::test_utilities::faults::wait_if_user_delete_paused(&target.username).await;
        // remove this user's profile icon from S3 if they had one
        if let Some(image_path) = image {
            Self::delete_graphic(&image_path, shared).await?;
        }
        Ok(())
    }

    /// Upload (or replace) this user's profile icon
    ///
    /// Streams the image to the shared graphics S3 bucket via [`GraphicSupport`], stores the
    /// resulting path on the user, and removes any previously-stored icon at a different path.
    ///
    /// # Arguments
    ///
    /// * `field` - The multipart field containing the image data
    /// * `shared` - Shared Thorium objects
    pub async fn set_image(&mut self, field: Field<'_>, shared: &Shared) -> Result<(), ApiError> {
        // build the base S3 path for this user's icon (keyed by namespace and username)
        let base_path = self.build_graphic_base_path_from_self(shared);
        // stream the image to S3 under a fixed name; the extension comes from the content type
        let new_path =
            Self::upload_graphic(base_path, field, Some("icon".to_owned()), shared).await?;
        // swap in the new path, keeping the old one (if any) so we can clean it up
        let old = self.image.replace(new_path.clone());
        // persist the new icon path on the user
        db::users::save(self, shared).await?;
        // delete the previous icon only if it lived at a different path (e.g. a different extension)
        if let Some(old_path) = old
            && old_path != new_path
        {
            Self::delete_graphic(&old_path, shared).await?;
        }
        Ok(())
    }

    /// Remove this user's profile icon, if one is set
    ///
    /// # Arguments
    ///
    /// * `shared` - Shared Thorium objects
    pub async fn delete_image(&mut self, shared: &Shared) -> Result<(), ApiError> {
        // take the existing path; nothing to do if there is no icon
        if let Some(old_path) = self.image.take() {
            // persist the cleared icon first, then remove the bytes from S3
            db::users::save(self, shared).await?;
            Self::delete_graphic(&old_path, shared).await?;
        }
        Ok(())
    }

    /// Checks if a user is an admin
    #[must_use]
    pub fn is_admin(&self) -> bool {
        self.role == UserRole::Admin
    }

    /// Check if this user is an admin or analyst
    #[must_use]
    pub fn is_admin_or_analyst(&self) -> bool {
        self.role == UserRole::Admin || self.role == UserRole::Analyst
    }

    /// Checks if a user is a developer
    ///
    /// # Arguments
    ///
    /// * `scaler` - The image scaler to check that we can develop for
    #[must_use]
    pub fn is_developer(&self, scaler: ImageScaler) -> bool {
        // make sure this user
        match self.role {
            // admins can develop for anything
            UserRole::Admin => true,
            // analysts can develop for anything
            UserRole::Analyst => true,
            // check if this developer can develop on all scalers
            UserRole::Developer {
                k8s,
                bare_metal,
                windows,
                external,
                kvm,
            } => {
                // check if this user can develop for this scaler
                match scaler {
                    ImageScaler::K8s => k8s,
                    ImageScaler::BareMetal => bare_metal,
                    ImageScaler::Windows => windows,
                    ImageScaler::External => external,
                    ImageScaler::Kvm => kvm,
                }
            }
            // this user cannot develop images/pipelines
            UserRole::User => false,
            // disabled users cannot do anything
            UserRole::Disabled => false,
        }
    }

    /// Checks if a user is a developer
    ///
    /// # Arguments
    ///
    /// * `scaler` - The image scaler to check that we can develop for
    #[must_use]
    pub fn is_developer_many(&self, scalers: &[ImageScaler]) -> bool {
        // make sure this user
        match self.role {
            // admins can develop for anything
            UserRole::Admin => true,
            // this user can develop for anything
            UserRole::Analyst => true,
            // check if this developer can develop on all scalers
            UserRole::Developer {
                k8s,
                bare_metal,
                windows,
                external,
                kvm,
            } => {
                // check if this user can develop for this scaler
                for scaler in scalers {
                    // get the correct scaler permission
                    let permission = match scaler {
                        ImageScaler::K8s => k8s,
                        ImageScaler::BareMetal => bare_metal,
                        ImageScaler::Windows => windows,
                        ImageScaler::External => external,
                        ImageScaler::Kvm => kvm,
                    };
                    // if we don't have permission then short circuit
                    if !permission {
                        return false;
                    }
                }
                true
            }
            // this user cannot develop images/pipelines
            UserRole::User => false,
            // disabled users cannot do anything
            UserRole::Disabled => false,
        }
    }

    /// generates a random token
    ///
    /// # Arguments
    ///
    /// * `shared` - Shared Thorium objects
    fn gen_token(&mut self, shared: &Shared) {
        // update token and its expiration
        self.token = token!();
        self.token_expiration = token_expire!(shared);
    }

    /// Saves a users token into the backend
    ///
    /// # Arguments
    ///
    /// * `old` - The old token for this user
    /// * `shared` - Shared Thorium objects
    async fn save_token(&self, old: &str, shared: &Shared) -> Result<(), ApiError> {
        db::users::save_token(self, old, shared).await
    }

    /// Generate and save a new token for a user
    pub async fn regen_token(&mut self, shared: &Shared) -> Result<(), ApiError> {
        // get our old token
        let old = self.token.clone();
        // generate a new token
        self.gen_token(shared);
        // save our new token
        self.save_token(&old, shared).await?;
        Ok(())
    }

    /// Updates a user
    ///
    /// This will invalidate the user's current token if the
    /// password is updated.
    ///
    /// # Arguments
    ///
    /// * `update` - The update to apply to this user
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "User::update", skip_all, err(Debug))]
    pub async fn update(
        mut self,
        mut update: UserUpdate,
        shared: &Shared,
    ) -> Result<Self, ApiError> {
        // if we are updating our role make sure we are an admin
        if update.role.is_some() {
            // only admins can update roles
            is_admin!(self);
            // never allow a user to disable their own account
            if matches!(update.role, Some(UserRole::Disabled)) {
                return bad!("You cannot disable your own account".to_owned());
            }
            // update our role
            crate::update!(self.role, update.role);
        }
        // check if we are updating their password
        if let Some(password) = &update.password {
            // disallow password updates for non local accounts
            if shared.config.thorium.auth.ldap.is_none() || self.password.is_some() {
                event!(Level::INFO, msg = "Updating password");
                // get our secret key
                let key = &shared.config.thorium.secret_key;
                // hash password and set a new token
                self.password = Some(hash_pw!(password, key));
                // get our old token
                let old_token = self.token.clone();
                // generate a new token
                self.gen_token(shared);
                // save this users token to the db
                db::users::save_token(&self, &old_token, shared).await?;
            } else {
                return unavailable!("Cannot update password when ldap is enabled".to_string());
            }
        }
        // apply any settings updates
        let applied = match update.settings {
            Some(settings) => Some(self.apply_settings(settings, shared).await?),
            None => None,
        };
        // save update user to the backend
        db::users::save(&self, shared).await?;
        // notify about changed model bans and queue the refreshes the update needs
        if let Some(applied) = applied {
            self.finish_settings(&self, applied, shared).await?;
        }
        Ok(self)
    }

    /// Updates a logged-in user without further authentication
    ///
    /// This will invalidate the user's current token if the
    /// password is updated.
    ///
    /// # Arguments
    ///
    /// * `update` - The update to apply to this user
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "User::update_user", skip_all, err(Debug))]
    pub async fn update_user(
        &self,
        username: &str,
        mut update: UserUpdate,
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // only admins can update other users
        is_admin!(self);
        // make sure admins cannot disable their own account through this route either
        if username == self.username && matches!(update.role, Some(UserRole::Disabled)) {
            return bad!("You cannot disable your own account".to_owned());
        }
        // get info on the target user
        let mut target = User::force_get(username, shared).await?;
        // check if we are updating their password
        if let Some(password) = &update.password {
            // disallow password updates for non local accounts
            if shared.config.thorium.auth.ldap.is_none() || target.password.is_some() {
                event!(Level::INFO, msg = "Updating password");
                // clone key so it will live throughout async closure
                let key = &shared.config.thorium.secret_key;
                // hash password and set a new token
                target.password = Some(hash_pw!(password, key));
                // get our old token
                let old_token = target.token.clone();
                // generate a new token
                target.gen_token(shared);
                // save this users token to the db
                db::users::save_token(&target, &old_token, shared).await?;
            } else {
                return unavailable!("Cannot update password when ldap is enabled".to_string());
            }
        }
        // update our role
        crate::update!(target.role, update.role);
        // apply any settings updates
        let applied = match update.settings {
            Some(settings) => Some(target.apply_settings(settings, shared).await?),
            None => None,
        };
        // save update user to the backend
        db::users::save(&target, shared).await?;
        // notify about changed model bans and queue the refreshes the update needs
        if let Some(applied) = applied {
            target.finish_settings(self, applied, shared).await?;
        }
        Ok(())
    }

    /// Authorize this user can access some groups or gets the groups they can
    ///
    /// For admins this will get all groups in the cluster
    ///
    /// # Arguments
    ///
    /// * `groups` - The groups to restrict this authorization check too
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "Users::authorize_groups", skip_all, err(Debug))]
    pub async fn authorize_groups(
        &self,
        groups: &mut Vec<String>,
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // for users we can search their groups but for admins we need to search all groups
        // if this user is not an admin then check if any groups were requested otherwise return
        // all the groups they are in
        if self.role == UserRole::Admin {
            // this user is an admin so they can access all groups
            // if they provided groups we just need to make sure they exist
            if !groups.is_empty() {
                // TODO: make sure groups exist
                return Ok(());
            }
            // no groups were provided so get 10000 groups
            // this is a bit of a hack and means if theres more then 10000 groups we silently miss things
            // TODO: Fix this
            let mut list = db::groups::list(0, 10000, shared).await?;
            // extend our groups list
            groups.append(&mut list.names);
            Ok(())
        } else {
            // this user is not an admin so make sure they can access any requested groups
            if groups.is_empty() {
                // no groups were provided so just default to the groups this user is a part of
                Group::authorize_all(self, &self.groups, shared).await?;
                // add our users groups
                groups.append(&mut self.groups.clone());
                Ok(())
            } else {
                Group::authorize_all(self, groups, shared).await?;
                Ok(())
            }
        }
    }

    /// Sync all ldap users info
    ///
    /// # Arguments
    ///
    /// * `shared` - Shared Thorium objects
    pub async fn sync_all_unix_info(&self, shared: &Shared) -> Result<(), ApiError> {
        // only admins can sync unix info
        is_admin!(self);
        // get our ldap conf
        if let Some(conf) = &shared.config.thorium.auth.ldap {
            //  build an ldap connection
            let (conn, mut ldap) = ldap!(conf).await?;
            // drive this connection to completion
            ldap3::drive!(conn);
            // bind to our system user
            bind_to_ldap_system_user(&mut ldap, conf).await?;
            // get a list of all users
            for user in self.list_details(shared).await? {
                // if we don't have a password set then assume ldap and update our unix info
                let unix = if user.local {
                    // this is a local user so just use our default unix ids
                    shared.config.thorium.auth.local_user_ids.clone()
                } else {
                    // this is an ldap based user so get updated info instead
                    get_unix_info(&user.username, conf, &mut ldap).await?
                };
                // update this users unix info
                db::users::update_unix_info(&user.username, &unix, shared).await?;
            }
            // unbind our ldap socket
            ldap.unbind().await?;
            Ok(())
        } else {
            bad!("LDAP is not configured!".to_owned())
        }
    }
}

impl AuthedUser {
    /// Authenticate a user with the correct authentication method
    ///
    /// This gets the authorization data from the authorization header.
    ///
    /// # Arguments
    ///
    /// * `auth_header` - The auth header value to pull creds from
    /// * `verify_email` - Whether to require a verified email or not
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "AuthedUser::auth", skip_all, err(Debug))]
    async fn auth(
        auth_header: &str,
        verify_email: bool,
        shared: &Shared,
    ) -> Result<Self, ApiError> {
        // get our auth method
        let method = check_unauth!(AuthMethods::from_str(auth_header));
        // try to authenticate our user
        match method.authenticate(verify_email, shared).await {
            Ok(user) => {
                event!(Level::INFO, user = &user.username);
                Ok(user)
            }
            Err(error) => {
                // we failed to auth this user due to an error
                event!(Level::ERROR, error = true, error_msg = error.to_string());
                Err(error)
            }
        }
    }

    /// Require this user authenticated with their primary token or password
    ///
    /// This rejects scoped tokens and is used to guard sensitive routes like
    /// account updates and scoped token management.
    #[instrument(name = "AuthedUser::require_full", skip_all, err(Debug))]
    pub fn require_full(self) -> Result<User, ApiError> {
        // make sure this user did not authenticate with a scoped token
        match self {
            // this user is fully authenticated
            AuthedUser::Full(user) => Ok(user),
            // this user authenticated with a scoped token so reject them
            AuthedUser::Scoped(scoped) => {
                // log that a scoped token tried to access a restricted route
                event!(
                    Level::ERROR,
                    user = &scoped.user.username,
                    token = &scoped.name,
                    msg = "Scoped tokens cannot access this route",
                );
                unauthorized!()
            }
        }
    }
}

/// Check if a user has the permisisons to gived a scoped token powers to deploy tools
/// to a target environment
///
/// # Arguments
///
/// * `scoped_role` - The scoped role that we want to set
/// * `user_role` - The role for this environment that this unscoped user has
/// * `environment` - The name of the environment this role is for
macro_rules! validate_developer_role {
    ($scoped_role:expr, $user_role:expr, $environment:expr) => {
        if *$scoped_role && *$user_role == false {
            return unauthorized!(format!(
                "Developer roles cannot be given for the {} environment",
                $environment
            ));
        }
    };
}

impl ScopedTokenRole {
    /// Make sure this scoped token role can be set by this user
    ///
    /// # Arguments
    ///
    /// * `user` - The user that wants to set this role
    pub fn validate(&self, user: &User) -> Result<(), ApiError> {
        match (self, &user.role) {
            // a user scoped token is always okay
            (ScopedTokenRole::User, _) => Ok(()),
            // a user with the user role cannot make a scoped token with the developer role
            (ScopedTokenRole::Developer { .. }, UserRole::User) => {
                unauthorized!(
                    "Users with the User role cannot create a scoped token with developer roles"
                        .to_owned()
                )
            }
            // a developer token for developers is only okay if it has the
            // same or lesser permissions as its user
            (
                ScopedTokenRole::Developer {
                    k8s: scoped_k8s,
                    bare_metal: scoped_bare_metal,
                    windows: scoped_windows,
                    external: scoped_external,
                    kvm: scoped_kvm,
                },
                UserRole::Developer {
                    k8s: user_k8s,
                    bare_metal: user_bare_metal,
                    windows: user_windows,
                    external: user_external,
                    kvm: user_kvm,
                },
            ) => {
                // check all of the different environments for perms
                validate_developer_role!(scoped_k8s, user_k8s, "K8s");
                validate_developer_role!(scoped_bare_metal, user_bare_metal, "Bare Metal");
                validate_developer_role!(scoped_windows, user_windows, "Windows");
                validate_developer_role!(scoped_external, user_external, "External");
                validate_developer_role!(scoped_kvm, user_kvm, "Kvm");
                Ok(())
            }
            // Admins and Analysts can deploy tools to all clusters
            (ScopedTokenRole::Developer { .. }, UserRole::Admin | UserRole::Analyst) => Ok(()),
            // disabled users cannot create scoped tokens
            // this is unreachable in practice since disabled users fail auth first
            (ScopedTokenRole::Developer { .. }, UserRole::Disabled) => {
                unauthorized!("This user has been disabled".to_owned())
            }
        }
    }

    /// Get the role a scoped token acts with given its owner's current role
    ///
    /// The token's role is capped at the owner's role: a scoped token never acts as an
    /// admin or analyst, a developer token keeps only the scaler flags both it and its
    /// owner have (admins and analysts count as every scaler) and acts as a user when no
    /// flag is left, and a disabled owner stays disabled so authentication still rejects
    /// them.
    ///
    /// # Arguments
    ///
    /// * `owner` - The current role of the user that owns this scoped token
    #[must_use]
    pub fn effective_role(&self, owner: &UserRole) -> UserRole {
        match (self, owner) {
            // disabled users stay disabled no matter the token's role
            (_, UserRole::Disabled) => UserRole::Disabled,
            // user tokens and developer tokens of users lowered to the user role act as users
            (ScopedTokenRole::User, _) | (ScopedTokenRole::Developer { .. }, UserRole::User) => {
                UserRole::User
            }
            // admins and analysts count as every scaler so the token's flags are kept
            (
                ScopedTokenRole::Developer {
                    k8s,
                    bare_metal,
                    windows,
                    external,
                    kvm,
                },
                UserRole::Admin | UserRole::Analyst,
            ) => Self::developer_or_user(*k8s, *bare_metal, *windows, *external, *kvm),
            // developers keep only the flags both the token and the owner have
            (
                ScopedTokenRole::Developer {
                    k8s: scoped_k8s,
                    bare_metal: scoped_bare_metal,
                    windows: scoped_windows,
                    external: scoped_external,
                    kvm: scoped_kvm,
                },
                UserRole::Developer {
                    k8s: user_k8s,
                    bare_metal: user_bare_metal,
                    windows: user_windows,
                    external: user_external,
                    kvm: user_kvm,
                },
            ) => Self::developer_or_user(
                *scoped_k8s && *user_k8s,
                *scoped_bare_metal && *user_bare_metal,
                *scoped_windows && *user_windows,
                *scoped_external && *user_external,
                *scoped_kvm && *user_kvm,
            ),
        }
    }

    /// Build a developer role with the given scaler flags, or a user role if no flag is set
    ///
    /// A developer role with every flag false can't develop for any scaler, so it is
    /// reported as the user role it acts as.
    ///
    /// # Arguments
    ///
    /// * `k8s` - Whether this role can develop for K8s
    /// * `bare_metal` - Whether this role can develop for bare metal
    /// * `windows` - Whether this role can develop for Windows
    /// * `external` - Whether this role can develop for external jobs
    /// * `kvm` - Whether this role can develop for Kvm
    #[allow(clippy::fn_params_excessive_bools)]
    fn developer_or_user(
        k8s: bool,
        bare_metal: bool,
        windows: bool,
        external: bool,
        kvm: bool,
    ) -> UserRole {
        // a role with no scaler flag left acts as a user
        if !(k8s || bare_metal || windows || external || kvm) {
            return UserRole::User;
        }
        // otherwise keep the developer role with the flags that are left
        UserRole::Developer {
            k8s,
            bare_metal,
            windows,
            external,
            kvm,
        }
    }
}

impl ScopedToken {
    /// Create a new scoped token for a user
    ///
    /// # Arguments
    ///
    /// * `user` - The user to create a scoped token for
    /// * `req` - The scoped token request to build a scoped token from
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "ScopedToken::create", skip_all, fields(user = user.username, name = req.name), err(Debug))]
    pub async fn create(
        user: &User,
        req: ScopedTokenRequest,
        shared: &Shared,
    ) -> Result<Self, ApiError> {
        // bounds check this scoped tokens name
        bounder::string_lower(&req.name, "name", 1, 50)?;
        // make sure this scoped token has at least one group in scope
        if req.groups.is_empty() {
            return bad!("Scoped tokens must be scoped to at least one group".to_owned());
        }
        // since scoped tokens are never admins we have to check against their actual groups
        let user_groups = if user.role == UserRole::Admin {
            // this user is an admin so check their actual groups
            // not all groups in Thorium
            &user.actual_groups
        } else {
            &user.groups
        };
        // make sure this scope is a subset of this users groups
        for group in &req.groups {
            if !user_groups.contains(group) {
                // if we are an admin then give a more helpful log
                if user.role == UserRole::Admin {
                    return unauthorized!(format!(
                        "Admins must actually be in groups to scope a token to that group. You are not in {group}."
                    ));
                }
                // we are not an admin so just use the standard unauthorized message
                return unauthorized!(format!("{} is not one of your groups", group));
            }
        }
        // if an expiration was set then make sure it is in the future
        if let Some(expires) = req.expires
            && expires < Utc::now()
        {
            return bad!("Scoped tokens cannot expire in the past".to_owned());
        }
        // validate that this user can create a scoped token with this role
        req.role.validate(user)?;
        // build our new scoped token
        let scoped = ScopedToken {
            name: req.name,
            owner: user.username.clone(),
            role: req.role,
            groups: req.groups,
            token: token!(),
            token_expiration: token_expire!(shared),
            expires: req.expires,
        };
        // save this scoped token to the backend
        db::users::create_scoped_token(&scoped, shared).await?;
        Ok(scoped)
    }

    /// Get one of a users scoped tokens by name
    ///
    /// If this scoped tokens value has expired then it will be rotated so
    /// owners never retrieve an already expired value.
    ///
    /// # Arguments
    ///
    /// * `user` - The user to get a scoped token for
    /// * `name` - The name of the scoped token to get
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "ScopedToken::get", skip(user, shared), fields(user = user.username), err(Debug))]
    pub async fn get(user: &User, name: &str, shared: &Shared) -> Result<Self, ApiError> {
        // get this scoped tokens data
        let mut scoped = db::users::get_scoped_token(&user.username, name, shared).await?;
        // if this scoped token is ephemeral then check if it has permanently expired
        if let Some(expires) = scoped.expires
            && expires < Utc::now()
        {
            // this scoped token has permanently expired so remove it from redis
            db::users::delete_scoped_token(&scoped, shared).await?;
            return not_found!(format!("Scoped token {name} not found"));
        }
        // rotate this scoped tokens value if it has expired
        if scoped.token_expiration < Utc::now() {
            scoped.rotate(shared).await?;
        }
        Ok(scoped)
    }

    /// Update one of a users scoped tokens by name
    ///
    /// Updates never change a scoped tokens value so activated tokens keep
    /// working after an update.
    ///
    /// # Arguments
    ///
    /// * `user` - The user to update a scoped token for
    /// * `name` - The name of the scoped token to update
    /// * `update` - The update to apply to this scoped token
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "ScopedToken::update", skip(user, update, shared), fields(user = user.username), err(Debug))]
    pub async fn update(
        user: &User,
        name: &str,
        update: ScopedTokenUpdate,
        shared: &Shared,
    ) -> Result<Self, ApiError> {
        // make sure this update isn't empty
        if update.is_empty() {
            return bad!("Scoped token updates cannot be empty".to_owned());
        }
        // make sure this update doesn't both set and clear an expiration
        if update.expires.is_some() && update.clear_expires {
            return bad!("Scoped token updates cannot both set and clear an expiration".to_owned());
        }
        // make sure any added groups are a subset of this users groups
        for group in &update.add_groups {
            if !user.groups.contains(group) {
                return unauthorized!(format!("{} is not one of your groups", group));
            }
        }
        // if a new expiration was set then make sure it is in the future
        if let Some(expires) = update.expires
            && expires < Utc::now()
        {
            return bad!("Scoped tokens cannot expire in the past".to_owned());
        }
        // get this scoped tokens data
        let mut scoped = db::users::get_scoped_token(&user.username, name, shared).await?;
        // if this scoped token is ephemeral then check if it has permanently expired
        if let Some(expires) = scoped.expires
            && expires < Utc::now()
        {
            // this scoped token has permanently expired so remove it from redis
            db::users::delete_scoped_token(&scoped, shared).await?;
            return not_found!(format!("Scoped token {name} not found"));
        }
        // if a new role was set then validate and update that
        if let Some(role) = update.role {
            // validate that this user can create a scoped token with this role
            role.validate(user)?;
            // set our updated role
            scoped.role = role;
        }
        // add any new groups to this scoped tokens scope skipping duplicates
        for group in update.add_groups {
            if !scoped.groups.contains(&group) {
                scoped.groups.push(group);
            }
        }
        // remove any groups from this scoped tokens scope
        scoped
            .groups
            .retain(|group| !update.remove_groups.contains(group));
        // make sure this scoped token still has at least one group in scope
        if scoped.groups.is_empty() {
            return bad!("Scoped tokens must be scoped to at least one group".to_owned());
        }
        // apply any expiration updates
        if update.clear_expires {
            // clear this scoped tokens expiration date
            scoped.expires = None;
        } else if update.expires.is_some() {
            // set this scoped tokens new expiration date
            scoped.expires = update.expires;
        }
        // save this scoped tokens updated data to the backend
        db::users::update_scoped_token(&scoped, shared).await?;
        Ok(scoped)
    }

    /// List all of a users scoped tokens
    ///
    /// Any ephemeral scoped tokens that have permanently expired will be
    /// deleted instead of listed.
    ///
    /// # Arguments
    ///
    /// * `user` - The user to list scoped tokens for
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "ScopedToken::list", skip_all, fields(user = user.username), err(Debug))]
    pub async fn list(user: &User, shared: &Shared) -> Result<Vec<Self>, ApiError> {
        // get all of this users scoped tokens
        let tokens = db::users::list_scoped_tokens(&user.username, shared).await?;
        // get the current timestamp to check for expired tokens
        let now = Utc::now();
        // crawl over these tokens and prune any that have permanently expired
        let mut live = Vec::with_capacity(tokens.len());
        for token in tokens {
            match token.expires {
                // this scoped token has permanently expired so remove it from redis
                Some(expires) if expires < now => {
                    db::users::delete_scoped_token(&token, shared).await?;
                }
                // this scoped token is still valid so list it
                _ => live.push(token),
            }
        }
        Ok(live)
    }

    /// Delete one of a users scoped tokens by name
    ///
    /// # Arguments
    ///
    /// * `user` - The user to delete a scoped token for
    /// * `name` - The name of the scoped token to delete
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "ScopedToken::delete", skip(user, shared), fields(user = user.username), err(Debug))]
    pub async fn delete(user: &User, name: &str, shared: &Shared) -> Result<(), ApiError> {
        // get this scoped tokens data so we can remove it from the token map
        let scoped = db::users::get_scoped_token(&user.username, name, shared).await?;
        // delete this scoped token
        db::users::delete_scoped_token(&scoped, shared).await?;
        Ok(())
    }

    /// Generate and save a new value for this scoped token
    ///
    /// # Arguments
    ///
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "ScopedToken::rotate", skip_all, err(Debug))]
    pub async fn rotate(&mut self, shared: &Shared) -> Result<(), ApiError> {
        // get our old token value
        let old = std::mem::replace(&mut self.token, token!());
        // update when this new value expires
        self.token_expiration = token_expire!(shared);
        // save our new token value
        db::users::save_scoped_token(self, &old, shared).await?;
        Ok(())
    }
}

impl ScopedUser {
    /// Logout a user that authenticated with a scoped token
    ///
    /// This rotates only the scoped tokens value and leaves the owning users
    /// primary token untouched.
    ///
    /// # Arguments
    ///
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "ScopedUser::logout", skip_all, err(Debug))]
    pub async fn logout(&self, shared: &Shared) -> Result<(), ApiError> {
        // get this scoped tokens data
        let mut scoped =
            db::users::get_scoped_token(&self.user.username, &self.name, shared).await?;
        // rotate this scoped tokens value
        scoped.rotate(shared).await?;
        Ok(())
    }
}

/// Base64 decode a string
///
/// # Arguments
///
/// * `encoded` - A base64 encoded string to decode
fn b64_decode(encoded: &str) -> Result<String, ApiError> {
    // decode our base64'd bytes
    let decoded = base64::engine::general_purpose::STANDARD.decode(encoded)?;
    // convert our decoded bytes to a string
    let decoded_string = str::from_utf8(&decoded[..])?.to_owned();
    Ok(decoded_string)
}

/// A rejection from the auth extractors
///
/// Optionally carries an [`ApiError`] so post-authentication failures (e.g. a
/// disabled account) can surface their message in the 401 body. Credential
/// failures and lookup errors stay a bare 401 so auth never leaks whether a
/// user exists.
pub struct AuthReject(Option<ApiError>);

impl AuthReject {
    /// Build a bare 401 rejection with no message
    fn bare() -> Self {
        AuthReject(None)
    }

    /// Build a rejection from an auth chain error
    ///
    /// Only 401 errors that carry a message are surfaced; any other error
    /// (404s from user lookups, 500s, ...) is masked as a bare 401.
    ///
    /// # Arguments
    ///
    /// * `error` - The auth chain error to build a rejection from
    fn from_error(error: ApiError) -> Self {
        // only surface public messages from post-credential 401s so internal
        // auth errors like OAuth failures stay masked
        if error.status() == StatusCode::UNAUTHORIZED && error.public_msg().is_some() {
            AuthReject(Some(error))
        } else {
            AuthReject(None)
        }
    }
}

impl IntoResponse for AuthReject {
    /// Build a response for this auth rejection
    fn into_response(self) -> Response {
        // surface the carried error body if we have one
        match self.0 {
            Some(error) => error.into_response(),
            None => StatusCode::UNAUTHORIZED.into_response(),
        }
    }
}

impl<S> FromRequestParts<S> for AuthedUser
where
    AppState: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = AuthReject;
    /// Gets an authenticated user from a request
    ///
    /// # Arguments
    ///
    /// * `parts` - The request parts to extract our secret key from
    /// * `state` - Shared Thorium objects
    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        // get the shared app state
        let state = AppState::from_ref(state);
        // extract the authorization headers for this user
        if let Some(header_val) = parts.headers.get("authorization") {
            // try to cast our authorization header value to a str
            if let Ok(header_str) = header_val.to_str() {
                // authenticate this user and make sure they have verified their email
                return match AuthedUser::auth(header_str, true, &state.shared).await {
                    // this user authenticated successfully
                    Ok(user) => Ok(user),
                    // carry surfaceable auth errors into our rejection
                    Err(error) => Err(AuthReject::from_error(error)),
                };
            }
        }
        // we failed to extract our auth info from our headers
        Err(AuthReject::bare())
    }
}

impl From<User> for ScrubbedUser {
    /// Drop a user's password and clear every secret in their settings
    ///
    /// # Arguments
    ///
    /// * `user` - The user to scrub
    fn from(mut user: User) -> Self {
        // no model secret or legacy AI key is ever returned
        user.settings.redact_secrets();
        ScrubbedUser {
            username: user.username,
            email: user.email,
            role: user.role,
            groups: user.groups,
            actual_groups: user.actual_groups,
            token: user.token,
            token_expiration: user.token_expiration,
            unix: user.unix,
            settings: user.settings,
            local: user.password.is_some(),
            verified: user.verified,
            has_image: user.image.is_some(),
        }
    }
}

// Add graphic support for users so profile icons reuse the shared S3 graphic pipeline
impl GraphicSupport for User {
    /// Users key their icon off their Thorium namespace and their (immutable) username
    type GraphicKey<'a> = (&'a str, &'a str);

    /// Build the base path for this user's graphic
    ///
    /// Icons live at `users/<namespace>/<username>/`, so Thorium namespaces sharing a graphics
    /// bucket keep separate icons for users with the same name. Icons are read and deleted at the
    /// path stored on the user, so icons stored at `users/<username>/` keep working.
    ///
    /// # Arguments
    ///
    /// * `key` - The Thorium namespace and the username
    fn build_graphic_base_path((namespace, username): Self::GraphicKey<'_>) -> PathBuf {
        // keep user icons under `users/` so a username that happens to be a UUID can't collide
        // with an entity graphic (entities key off their UUID at the graphics-bucket root)
        PathBuf::from("users").join(namespace).join(username)
    }

    /// Build the base path for this user's graphic
    ///
    /// # Arguments
    ///
    /// * `shared` - Shared Thorium objects
    fn build_graphic_base_path_from_self(&self, shared: &Shared) -> PathBuf {
        // call our base path builder with this deployment's namespace
        Self::build_graphic_base_path((&shared.config.thorium.namespace, &self.username))
    }
}

impl From<User> for AuthResponse {
    /// Build an `AuthResponse` from a User
    ///
    /// # Arguments
    ///
    /// * `user` - The user to build an `AuthResponse` from
    fn from(user: User) -> Self {
        // check if this user has verified their email
        if user.verified {
            // this user has
            AuthResponse::Authed {
                token: user.token,
                expires: user.token_expiration,
            }
        } else {
            // This user has not yet verified their email
            AuthResponse::VerifyEmail(user.email)
        }
    }
}

impl<S> FromRequestParts<S> for AuthResponse
where
    AppState: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = AuthReject;
    /// Gets an authenticated user from a request
    ///
    /// # Arguments
    ///
    /// * `parts` - The request parts to extract our secret key from
    /// * `state` - Shared Thorium objects
    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        // get the shared app state
        let state = AppState::from_ref(state);
        // extract the authorization headers for this user
        if let Some(header_val) = parts.headers.get("authorization") {
            // try to cast our authorization header value to a str
            if let Ok(header_str) = header_val.to_str() {
                // authenticate this user but don't require a verified email
                return match AuthedUser::auth(header_str, false, &state.shared).await {
                    // this user authenticated successfully
                    Ok(authed) => {
                        // get the effective user for this authed user
                        // for scoped users this carries the scoped tokens value/expiration
                        let user = authed.into_user();
                        // return the correct auth response based on if we have a verified email or not
                        let resp = if user.verified {
                            // this user has a verified email and has authenticated so return their token info
                            AuthResponse::Authed {
                                token: user.token,
                                expires: user.token_expiration,
                            }
                        } else {
                            // this user still needs to verify their email
                            AuthResponse::VerifyEmail(user.email)
                        };
                        Ok(resp)
                    }
                    // carry surfaceable auth errors into our rejection
                    Err(error) => Err(AuthReject::from_error(error)),
                };
            }
        }
        // we failed to extract our auth info from our headers
        Err(AuthReject::bare())
    }
}

#[cfg(test)]
mod model_tests;

#[cfg(test)]
mod tests {
    use super::{Key, build_ldap_user_filter, reject_empty_ldap_password, secret_key_matches};
    use crate::models::{ScopedTokenRole, UserRole};
    use headers::{Header, HeaderValue};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::sync::Semaphore;

    /// Build a developer user role with the given scaler flags
    ///
    /// # Arguments
    ///
    /// * `k8s` - Whether this role can develop for K8s
    /// * `bare_metal` - Whether this role can develop for bare metal
    /// * `windows` - Whether this role can develop for Windows
    fn dev_user(k8s: bool, bare_metal: bool, windows: bool) -> UserRole {
        UserRole::Developer {
            k8s,
            bare_metal,
            windows,
            external: false,
            kvm: false,
        }
    }

    /// Build a developer scoped token role with the given scaler flags
    ///
    /// # Arguments
    ///
    /// * `k8s` - Whether this token can develop for K8s
    /// * `bare_metal` - Whether this token can develop for bare metal
    /// * `windows` - Whether this token can develop for Windows
    fn dev_token(k8s: bool, bare_metal: bool, windows: bool) -> ScopedTokenRole {
        ScopedTokenRole::Developer {
            k8s,
            bare_metal,
            windows,
            external: false,
            kvm: false,
        }
    }

    /// A developer token only keeps the flags both it and its developer owner have
    #[test]
    fn effective_role_table() {
        // a developer token only keeps the flags both it and its developer owner have
        assert_eq!(
            dev_token(true, false, true).effective_role(&dev_user(true, true, false)),
            dev_user(true, false, false)
        );
        // admins and analysts count as every scaler so a developer token keeps its own flags
        assert_eq!(
            dev_token(true, false, false).effective_role(&UserRole::Admin),
            dev_user(true, false, false)
        );
        assert_eq!(
            dev_token(true, false, false).effective_role(&UserRole::Analyst),
            dev_user(true, false, false)
        );
        // a developer token that shares no flag with its developer owner acts as a user
        assert_eq!(
            dev_token(true, false, true).effective_role(&dev_user(false, true, false)),
            UserRole::User
        );
        // a developer token with no flag acts as a user for admin and analyst owners too
        assert_eq!(
            dev_token(false, false, false).effective_role(&UserRole::Admin),
            UserRole::User
        );
        assert_eq!(
            dev_token(false, false, false).effective_role(&UserRole::Analyst),
            UserRole::User
        );
        // a developer token of an owner lowered to the user role acts as a user
        assert_eq!(
            dev_token(true, true, true).effective_role(&UserRole::User),
            UserRole::User
        );
        // a user token always acts as a user for every enabled owner role
        for owner in [
            UserRole::Admin,
            UserRole::Analyst,
            dev_user(true, true, true),
            UserRole::User,
        ] {
            assert_eq!(ScopedTokenRole::User.effective_role(&owner), UserRole::User);
        }
        // a disabled owner keeps every token disabled
        assert_eq!(
            ScopedTokenRole::User.effective_role(&UserRole::Disabled),
            UserRole::Disabled
        );
        assert_eq!(
            dev_token(true, true, true).effective_role(&UserRole::Disabled),
            UserRole::Disabled
        );
    }

    #[test]
    fn secret_key_matches_is_correct() {
        // the constant-time comparison must still be functionally correct — equal keys
        // match and any difference (value, length, or a prefix) does not
        assert!(secret_key_matches("super-secret-key", "super-secret-key"));
        assert!(!secret_key_matches("super-secret-key", "wrong-key"));
        assert!(!secret_key_matches("super-secret-key", "super-secret-ke"));
        assert!(!secret_key_matches(
            "super-secret-key",
            "super-secret-key-extra"
        ));
        assert!(!secret_key_matches("super-secret-key", ""));
    }

    #[test]
    fn key_header_rejects_non_utf8_value() {
        // craft a secret-key header value from raw non-UTF8 bytes, which an attacker can send
        let value = HeaderValue::from_bytes(&[0xFF, 0xFE]).expect("failed to build header");
        let values = [value];
        // decoding the header must return an error rather than panicking on
        // `to_str().unwrap()`
        let decoded = <Key as Header>::decode(&mut values.iter());
        assert!(
            decoded.is_err(),
            "Key header decode accepted a non-UTF8 value instead of erroring"
        );
    }

    #[test]
    fn key_header_decodes_valid_value() {
        // a normal ascii secret key must still decode successfully
        let value = HeaderValue::from_static("super-secret-key");
        let values = [value];
        let decoded = <Key as Header>::decode(&mut values.iter()).expect("failed to decode key");
        assert_eq!(decoded.key, "super-secret-key");
    }

    #[test]
    fn ldap_bind_rejects_empty_password() {
        // an empty password at the LDAP bind site must be rejected so a simple bind cannot
        // become an unauthenticated bind that authenticates as any LDAP-backed user
        assert!(
            reject_empty_ldap_password("").is_err(),
            "LDAP bind accepted an empty password (unauthenticated bind)"
        );
    }

    #[test]
    fn ldap_bind_allows_non_empty_password() {
        // a real password must still be allowed through to the bind
        assert!(
            reject_empty_ldap_password("hunter2").is_ok(),
            "LDAP bind rejected a normal password"
        );
    }

    #[test]
    fn ldap_filter_escapes_metacharacters() {
        // build a filter for a username carrying LDAP filter metacharacters an attacker could send
        let filter = build_ldap_user_filter("uid=", "*)(uid=*");
        // the raw injection payload must NOT appear verbatim; the metacharacters must be
        // escaped (e.g. `*` -> `\2a`, `(` -> `\28`, `)` -> `\29`) before reaching the directory
        assert!(
            !filter.contains("*)(uid=*"),
            "LDAP filter embedded an unescaped username (filter injection): {filter}"
        );
    }

    #[test]
    fn ldap_filter_preserves_normal_usernames() {
        // a well behaved username must still appear in the constructed filter unchanged
        let filter = build_ldap_user_filter("uid=", "analyst1");
        assert!(
            filter.contains("uid=analyst1"),
            "LDAP filter dropped a normal username: {filter}"
        );
    }

    use super::{bounded_blocking, verify_permits};

    /// Password verifications get one permit per CPU, and always at least one
    #[test]
    fn verify_permits_follow_the_cpu_count() {
        let cpus = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
        assert_eq!(verify_permits(), cpus);
        assert!(verify_permits() >= 1);
    }

    /// No more blocking tasks run at once than there are permits, however many are started
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn bounded_blocking_limits_concurrency() {
        let permits = Arc::new(Semaphore::new(3));
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        // start far more tasks than there are permits
        let mut handles = Vec::new();
        for _ in 0..64 {
            let permits = Arc::clone(&permits);
            let running = Arc::clone(&running);
            let peak = Arc::clone(&peak);
            handles.push(tokio::spawn(async move {
                bounded_blocking(&permits, move || {
                    // count this task as running and track the most seen at once
                    let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(10));
                    running.fetch_sub(1, Ordering::SeqCst);
                })
                .await
            }));
        }
        // wait for every task to finish
        for handle in handles {
            handle.await.unwrap().unwrap();
        }
        let peak = peak.load(Ordering::SeqCst);
        assert!(peak <= 3, "{peak} tasks ran at once with 3 permits");
        assert!(peak >= 1, "no task ran");
    }
}
