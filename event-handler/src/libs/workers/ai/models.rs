//! Model refresh: list an endpoint's models and send the result to the API
//!
//! Each refresh reads its endpoint as the user who caused it, through that user's masqueraded
//! client, so it only sees what they can see. Secrets are read with the admin client, held only
//! for the listing, and never logged. The API merges the report, so the worker never edits a
//! catalog or a ban itself.

use chrono::{DateTime, Utc};
use futures::stream::{self, StreamExt};
use reqwest::StatusCode;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::time::Duration;
use thorium::models::{
    AiSettingsUpdate, CatalogEntrySource, EntityMetadata, EntityMetadataUpdate, EntityUpdate,
    Event, EventData, ModelBan, ModelBanKind, ModelCatalogEntry, ModelConfigRef, ModelProvider,
    ModelRefreshEvent, ModelRefreshReport, ModelRefreshTarget, ModelsConf, ScrubbedUser,
    UserModelSettings, UserModelSettingsUpdate, UserRole, UserSettingsUpdate, UserUpdate,
};
use thorium::{Error, Thorium};
use tracing::{Level, event};
use uuid::Uuid;

use super::AiWorkerStats;
use super::providers::{AuthHosts, Lister, ListingCredential, ListingLimits};

/// How many refreshes of a batch run at once
pub const REFRESH_CONCURRENCY: usize = 5;

/// The bounds model refreshes run under
#[derive(Debug, Clone, Copy)]
pub struct RefreshLimits {
    /// The bounds of each listing
    pub listing: ListingLimits,
    /// How long one pass over a batch's refreshes may run before the rest wait for a retry
    pub pass: Duration,
}

impl Default for RefreshLimits {
    /// The default listing bounds and three minutes per pass
    fn default() -> Self {
        RefreshLimits {
            listing: ListingLimits::default(),
            pass: Duration::from_secs(180),
        }
    }
}

/// What makes two refreshes in one batch the same refresh: the user it acts as, the endpoint,
/// and the user whose settings it reports into
type RefreshKey = (String, Option<Uuid>, Option<String>);

/// Get what makes a refresh the same as another in its batch
///
/// # Arguments
///
/// * `user` - The user the refresh acts as
/// * `target` - What it refreshes
fn refresh_key(user: &str, target: &ModelRefreshTarget) -> RefreshKey {
    // a group endpoint reports to the endpoint, the others into a user's settings
    match target {
        ModelRefreshTarget::GroupEndpoint { endpoint, .. } => {
            (user.to_owned(), Some(*endpoint), None)
        }
        ModelRefreshTarget::UserEndpoint {
            user: owner,
            endpoint,
        } => (user.to_owned(), Some(*endpoint), Some(owner.clone())),
        ModelRefreshTarget::User { user: owner } => (user.to_owned(), None, Some(owner.clone())),
    }
}

/// A refresh that should be retried later
#[derive(Debug)]
pub struct RetryLater;

/// Whether an API error says the item doesn't exist or can't be seen
///
/// # Arguments
///
/// * `error` - The error
fn is_missing(error: &Error) -> bool {
    error.status() == Some(StatusCode::NOT_FOUND)
}

/// Log a failure that leaves a refresh in flight and turn it into a retry
///
/// # Arguments
///
/// * `msg` - What failed
/// * `error` - The error
#[allow(clippy::needless_pass_by_value)]
fn retry(msg: &str, error: Error) -> RetryLater {
    // the error never holds a secret, since secrets are only sent to providers
    event!(Level::ERROR, msg, error = error.to_string());
    RetryLater
}

/// The per-user masqueraded clients refreshes read and report through
///
/// Clients for the users the event handler knew at startup are built from its user cache, and a
/// user it doesn't know gets a client built on first use. Disabled users get no client, so their
/// refreshes are skipped.
#[derive(Default)]
pub struct UserClients {
    /// A client masquerading as each enabled user, by username
    clients: HashMap<String, Thorium>,
    /// The users known to be disabled
    disabled: HashSet<String>,
    /// The users whose clients are rebuilt the next time they're used
    stale: HashSet<String>,
}

impl UserClients {
    /// Build or drop clients for a set of users
    ///
    /// # Arguments
    ///
    /// * `admin` - The admin client every user client is built from
    /// * `users` - The users to build clients for
    pub fn apply_users(&mut self, admin: &Thorium, users: HashMap<String, ScrubbedUser>) {
        // build a client for each enabled user and drop the disabled ones
        for (username, user) in users {
            self.track(admin, username, &user);
        }
    }

    /// Build or drop the client of one user
    ///
    /// # Arguments
    ///
    /// * `admin` - The admin client the user's client is built from
    /// * `username` - The user's name
    /// * `user` - The user
    fn track(&mut self, admin: &Thorium, username: String, user: &ScrubbedUser) {
        // a disabled user's client would only fail
        if user.role == UserRole::Disabled {
            self.clients.remove(&username);
            self.stale.remove(&username);
            self.disabled.insert(username);
            return;
        }
        // masquerade as an enabled user
        let mut client = admin.clone();
        client.masquerade(user);
        self.disabled.remove(&username);
        self.stale.remove(&username);
        self.clients.insert(username, client);
    }

    /// Rebuild a user's client the next time it's used
    ///
    /// # Arguments
    ///
    /// * `username` - The user
    pub fn mark_stale(&mut self, username: &str) {
        self.stale.insert(username.to_owned());
    }

    /// Whether a client is cached for a user
    ///
    /// # Arguments
    ///
    /// * `username` - The user
    #[must_use]
    pub fn has_client(&self, username: &str) -> bool {
        self.clients.contains_key(username)
    }

    /// Get a client masquerading as a user, building it on first use
    ///
    /// Returns nothing for a disabled or deleted user, whose refreshes are skipped.
    ///
    /// # Arguments
    ///
    /// * `admin` - The admin client the user's client is built from
    /// * `username` - The user
    /// * `stats` - The stats to count the user lookup in
    pub async fn resolve(
        &mut self,
        admin: &Thorium,
        username: &str,
        stats: &mut AiWorkerStats,
    ) -> Result<Option<Thorium>, Error> {
        // a known disabled user is skipped
        if self.disabled.contains(username) && !self.stale.contains(username) {
            return Ok(None);
        }
        // a cached client is reused unless it's stale
        if !self.stale.contains(username)
            && let Some(client) = self.clients.get(username)
        {
            return Ok(Some(client.clone()));
        }
        // read the user and build their client
        stats.user_lookups += 1;
        let user = match admin.users.get(username).await {
            Ok(user) => user,
            Err(error) if is_missing(&error) => {
                self.clients.remove(username);
                self.stale.remove(username);
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        self.track(admin, username.to_owned(), &user);
        Ok(self.clients.get(username).cloned())
    }
}

/// A credential read for a refresh
struct ReadCredential {
    /// Which credential this is, named in the report so only it is banned or cleared
    reference: ModelConfigRef,
    /// What the listing signs in with
    listing: ListingCredential,
    /// When the credential stops working, if known
    expires: Option<DateTime<Utc>>,
    /// The credential's bans, to count the bans a report adds or clears
    bans: HashMap<Uuid, ModelBan>,
}

/// The catalog and bans a refresh's report is merged into, as read before the listing
struct MergeTarget<'a> {
    /// The endpoint's catalog, when the report merges into it
    catalog: Option<&'a [ModelCatalogEntry]>,
    /// The endpoint's bans, when the report merges into them
    endpoint_bans: Option<&'a HashMap<Uuid, ModelBan>>,
    /// The bans of the credential the refresh used, when the report merges into them
    credential_bans: Option<&'a HashMap<Uuid, ModelBan>>,
}

/// Whether the deployment still allows contacting an endpoint
///
/// # Arguments
///
/// * `models` - The deployment's allowed endpoint list
/// * `provider` - The endpoint's provider
/// * `bans` - The endpoint's bans
fn allowed(models: &ModelsConf, provider: &ModelProvider, bans: &HashMap<Uuid, ModelBan>) -> bool {
    // an endpoint banned for leaving the list, or no longer on it, is never contacted
    !bans
        .values()
        .any(|ban| ban.ban_kind == ModelBanKind::NotAllowedByDeployment)
        && models.allows(provider)
}

/// Whether a ban list holds a ban of the same kind
///
/// # Arguments
///
/// * `bans` - The ban list
/// * `kind` - The kind of ban
fn holds(bans: &HashMap<Uuid, ModelBan>, kind: &ModelBanKind) -> bool {
    bans.values().any(|ban| ban.ban_kind.name() == kind.name())
}

/// Count what merging a report will change, from what the refresh read before listing
///
/// # Arguments
///
/// * `target` - What the report is merged into
/// * `report` - The report
/// * `stats` - The stats to count in
fn count_changes(target: &MergeTarget<'_>, report: &ModelRefreshReport, stats: &mut AiWorkerStats) {
    // a failure adds its ban unless one of its kind is already there
    if let Some(failure) = &report.failure {
        stats.listing_failures += 1;
        let bans = match failure {
            ModelBanKind::EndpointUnreachable(_) => target.endpoint_bans,
            _ => target.credential_bans,
        };
        if bans.is_some_and(|bans| !holds(bans, failure)) {
            stats.bans_added += 1;
        }
        return;
    }
    // a success adds new models, bans the refreshed ones it omits, and clears what reappeared
    if let Some(catalog) = target.catalog {
        let listed = report
            .models
            .iter()
            .map(|model| model.model_id.as_str())
            .collect::<HashSet<&str>>();
        let known = catalog
            .iter()
            .map(|entry| entry.model_id.as_str())
            .collect::<HashSet<&str>>();
        stats.models_added += listed.difference(&known).count();
        for entry in catalog {
            let removed = entry
                .bans
                .values()
                .filter(|ban| ban.ban_kind == ModelBanKind::ModelRemoved)
                .count();
            if listed.contains(entry.model_id.as_str()) {
                stats.bans_cleared += removed;
            } else if matches!(entry.source, CatalogEntrySource::Refreshed { .. }) && removed == 0 {
                stats.bans_added += 1;
            }
        }
    }
    // the refresh failure bans are cleared
    if let Some(bans) = target.endpoint_bans {
        stats.bans_cleared += bans
            .values()
            .filter(|ban| matches!(ban.ban_kind, ModelBanKind::EndpointUnreachable(_)))
            .count();
    }
    if let Some(bans) = target.credential_bans {
        stats.bans_cleared += bans
            .values()
            .filter(|ban| {
                matches!(
                    ban.ban_kind,
                    ModelBanKind::CredentialRejected { .. } | ModelBanKind::CredentialExpired
                )
            })
            .count();
    }
}

/// Wrap a model settings update in the user update that carries it
///
/// # Arguments
///
/// * `models` - The model settings update
fn models_update(models: UserModelSettingsUpdate) -> UserUpdate {
    // the model settings travel inside the AI settings
    let ai = AiSettingsUpdate::default().models(models);
    UserUpdate::default().settings(UserSettingsUpdate::default().ai(ai))
}

/// Get the user whose client a refresh reads and reports through
///
/// # Arguments
///
/// * `event` - The refresh event
/// * `refresh` - The refresh it carries
#[must_use]
pub fn acting_user<'a>(event: &'a Event, refresh: &'a ModelRefreshEvent) -> &'a str {
    // an endpoint refresh acts as the user who caused it, a user refresh as that user
    match &refresh.target {
        ModelRefreshTarget::GroupEndpoint { .. } => &event.user,
        ModelRefreshTarget::UserEndpoint { user, .. } | ModelRefreshTarget::User { user } => user,
    }
}

/// The events of a batch that run a refresh, and how the rest follow them
struct RefreshBatch {
    /// The events that run a refresh, with their refresh and the acting user's client
    work: Vec<(Event, ModelRefreshEvent, Thorium)>,
    /// The events that follow each running event's result
    followers: HashMap<Uuid, Vec<Uuid>>,
    /// The users whose client couldn't be read
    failed: HashSet<String>,
}

/// What refreshing a batch did
#[derive(Debug, Default)]
pub struct RefreshOutcome {
    /// The events that were finished and can be cleared
    pub cleared: Vec<Uuid>,
    /// The events left in flight to be retried
    pub in_flight: Vec<Uuid>,
    /// The stats for the batch
    pub stats: AiWorkerStats,
}

/// Refreshes model catalogs for a batch of model refresh events
pub struct ModelRefresher<'a> {
    /// The event handler's admin client, for the admin-only reads and endpoint reports
    admin: &'a Thorium,
    /// Lists models through each provider's API
    lister: Lister<'a>,
    /// How long one pass over a batch's refreshes may run
    pass: Duration,
}

impl<'a> ModelRefresher<'a> {
    /// Build a refresher
    ///
    /// # Arguments
    ///
    /// * `admin` - The event handler's admin client
    /// * `listing` - The client listing requests are sent with
    /// * `hosts` - Where sign-in requests go
    /// * `limits` - The bounds refreshes run under
    #[must_use]
    pub fn new(
        admin: &'a Thorium,
        listing: &'a reqwest::Client,
        hosts: &'a AuthHosts,
        limits: RefreshLimits,
    ) -> Self {
        ModelRefresher {
            admin,
            lister: Lister::new(listing, hosts).limits(limits.listing),
            pass: limits.pass,
        }
    }

    /// Refresh every model refresh event in a batch, up to five at a time
    ///
    /// An event is cleared once its report is sent, or when there's nothing to refresh (a
    /// disabled user, a missing endpoint, or no credential). Events that ask for the same
    /// refresh share one listing and are cleared or retried together. An event whose reads or
    /// report fail for a reason that may pass is left in flight, and its user's client is
    /// rebuilt when it's retried; so is every event whose refresh is still running when the
    /// pass's deadline arrives.
    ///
    /// # Arguments
    ///
    /// * `clients` - The per-user clients
    /// * `events` - The batch
    pub async fn process(&self, clients: &mut UserClients, events: &[Event]) -> RefreshOutcome {
        let mut outcome = RefreshOutcome::default();
        // read the allowed endpoint list every refresh is checked against, or retry them all
        let models = match self.admin.system.get_settings().await {
            Ok(settings) => settings.models,
            Err(error) => {
                event!(
                    Level::ERROR,
                    msg = "Failed to read the allowed model endpoints",
                    error = error.to_string()
                );
                outcome.in_flight = events.iter().map(|event| event.id).collect();
                return outcome;
            }
        };
        let models = &models;
        // find the user each refresh acts as and pick the event that runs each refresh
        let RefreshBatch {
            work,
            mut followers,
            failed,
        } = self.plan(clients, events, &mut outcome).await;
        // refresh up to five events at once until every refresh is done or the pass's deadline
        let mut unfinished = work
            .iter()
            .map(|(event, _, _)| event.id)
            .collect::<HashSet<Uuid>>();
        let results = self.run(work, models).await;
        // sort the events into finished and retried, and rebuild retried users' clients
        let mut stale = BTreeSet::new();
        for (id, user, result, stats) in results {
            unfinished.remove(&id);
            outcome.stats += stats;
            let followed = followers.remove(&id).unwrap_or_default();
            match result {
                Ok(()) => {
                    outcome.cleared.push(id);
                    outcome.cleared.extend(followed);
                }
                Err(RetryLater) => {
                    outcome.in_flight.push(id);
                    outcome.in_flight.extend(followed);
                    stale.insert(user);
                }
            }
        }
        // the refreshes still running at the deadline are retried later
        if !unfinished.is_empty() {
            event!(
                Level::WARN,
                msg = "Leaving refreshes that ran past the pass deadline for a retry",
                count = unfinished.len()
            );
        }
        for id in unfinished {
            outcome.in_flight.push(id);
            outcome
                .in_flight
                .extend(followers.remove(&id).unwrap_or_default());
        }
        stale.extend(failed);
        for user in stale {
            clients.mark_stale(&user);
        }
        outcome.stats.events_processed = outcome.cleared.len();
        outcome
    }

    /// Find the user each refresh in a batch acts as and pick the event that runs each refresh
    ///
    /// Events for disabled or missing users are cleared and events whose user can't be read
    /// are left in flight in `outcome`.
    ///
    /// # Arguments
    ///
    /// * `clients` - The per-user clients
    /// * `events` - The batch
    /// * `outcome` - The outcome to record skipped and retried events in
    async fn plan(
        &self,
        clients: &mut UserClients,
        events: &[Event],
        outcome: &mut RefreshOutcome,
    ) -> RefreshBatch {
        // get the users' clients one at a time
        let mut work = Vec::with_capacity(events.len());
        let mut resolved: HashMap<String, Option<Thorium>> = HashMap::new();
        let mut failed: HashSet<String> = HashSet::new();
        // the first event asking for each refresh runs it, and the others follow its result
        let mut leaders: HashMap<RefreshKey, Uuid> = HashMap::new();
        let mut followers: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
        for event in events {
            let EventData::ModelRefresh(refresh) = &event.data else {
                continue;
            };
            let user = acting_user(event, refresh).to_owned();
            if !resolved.contains_key(&user) && !failed.contains(&user) {
                match clients.resolve(self.admin, &user, &mut outcome.stats).await {
                    Ok(client) => {
                        resolved.insert(user.clone(), client);
                    }
                    Err(error) => {
                        event!(
                            Level::ERROR,
                            msg = "Failed to read the user a refresh acts as",
                            user,
                            error = error.to_string()
                        );
                        failed.insert(user.clone());
                    }
                }
            }
            match resolved.get(&user) {
                // a disabled or deleted user's refresh is skipped
                Some(None) => {
                    event!(
                        Level::INFO,
                        msg = "Skipping a refresh for a disabled or missing user",
                        user
                    );
                    outcome.cleared.push(event.id);
                }
                // a refresh already asked for in this batch follows the first event asking
                Some(Some(client)) => {
                    let key = refresh_key(&user, &refresh.target);
                    match leaders.get(&key) {
                        Some(leader) => followers.entry(*leader).or_default().push(event.id),
                        None => {
                            leaders.insert(key, event.id);
                            work.push((event.clone(), refresh.clone(), client.clone()));
                        }
                    }
                }
                None => outcome.in_flight.push(event.id),
            }
        }
        RefreshBatch {
            work,
            followers,
            failed,
        }
    }

    /// Run a batch's refreshes up to five at a time until they're done or the pass's deadline
    ///
    /// # Arguments
    ///
    /// * `work` - The events that run a refresh, with their refresh and client
    /// * `models` - The deployment's allowed endpoint list
    async fn run(
        &self,
        work: Vec<(Event, ModelRefreshEvent, Thorium)>,
        models: &ModelsConf,
    ) -> Vec<(Uuid, String, Result<(), RetryLater>, AiWorkerStats)> {
        // start each refresh with its own stats
        let mut running = stream::iter(work)
            .map(|(event, refresh, client)| async move {
                let mut stats = AiWorkerStats::default();
                let result = self
                    .refresh(&event, &refresh, &client, models, &mut stats)
                    .await;
                let user = acting_user(&event, &refresh).to_owned();
                (event.id, user, result, stats)
            })
            .buffer_unordered(REFRESH_CONCURRENCY);
        // collect results until every refresh is done or the pass's deadline arrives
        let deadline = tokio::time::sleep(self.pass);
        tokio::pin!(deadline);
        let mut results = Vec::new();
        loop {
            tokio::select! {
                next = running.next() => match next {
                    Some(result) => results.push(result),
                    None => break,
                },
                () = &mut deadline => break,
            }
        }
        // drop the refreshes still running at the deadline
        drop(running);
        results
    }

    /// Refresh one event's target
    ///
    /// # Arguments
    ///
    /// * `event` - The event
    /// * `refresh` - The refresh it carries
    /// * `client` - The client of the user it acts as
    /// * `models` - The deployment's allowed endpoint list
    /// * `stats` - The stats to count in
    async fn refresh(
        &self,
        event: &Event,
        refresh: &ModelRefreshEvent,
        client: &Thorium,
        models: &ModelsConf,
        stats: &mut AiWorkerStats,
    ) -> Result<(), RetryLater> {
        // each target refreshes differently
        match &refresh.target {
            ModelRefreshTarget::GroupEndpoint { endpoint, .. } => {
                self.group_endpoint(client, &event.user, *endpoint, models, stats)
                    .await
            }
            ModelRefreshTarget::UserEndpoint { user, endpoint } => {
                let settings = self.own_settings(client).await?;
                self.user_endpoint(client, user, &settings, *endpoint, models, stats)
                    .await
            }
            ModelRefreshTarget::User { user } => {
                // refresh every endpoint the user has a credential for, one report each
                let settings = self.own_settings(client).await?;
                for endpoint in settings.credentials.keys() {
                    self.user_endpoint(client, user, &settings, *endpoint, models, stats)
                        .await?;
                }
                Ok(())
            }
        }
    }

    /// Read a user's own model settings, without secrets, through their client
    ///
    /// # Arguments
    ///
    /// * `client` - The user's client
    async fn own_settings(&self, client: &Thorium) -> Result<UserModelSettings, RetryLater> {
        // read who the client is
        let user = client
            .users
            .info()
            .await
            .map_err(|error| retry("Failed to read a user's model settings", error))?;
        Ok(user.settings.ai.map_or_default(|ai| ai.models))
    }

    /// Read a group credential's secrets with the admin client, and its source's for a role
    ///
    /// Returns nothing when the credential or a role's source no longer exists.
    ///
    /// # Arguments
    ///
    /// * `id` - The credential's id
    async fn group_credential(&self, id: Uuid) -> Result<Option<ReadCredential>, RetryLater> {
        // read the credential
        let credential = match self.admin.entities.model_credential_secret(id).await {
            Ok(credential) => credential,
            Err(error) if is_missing(&error) => return Ok(None),
            Err(error) => return Err(retry("Failed to read a model credential", error)),
        };
        // an assumed role signs in with its source access key
        let source = match credential.kind.role_source() {
            Some(source) => match self.admin.entities.model_credential_secret(source).await {
                Ok(source) => Some(Box::new(ListingCredential {
                    kind: source.kind,
                    secrets: source.secrets,
                    source: None,
                })),
                Err(error) if is_missing(&error) => return Ok(None),
                Err(error) => {
                    return Err(retry("Failed to read a role's source credential", error));
                }
            },
            None => None,
        };
        Ok(Some(ReadCredential {
            reference: ModelConfigRef::Group(id),
            listing: ListingCredential {
                kind: credential.kind,
                secrets: credential.secrets,
                source,
            },
            expires: credential.expires,
            bans: credential.bans,
        }))
    }

    /// Read a user's own credential for an endpoint with the admin client
    ///
    /// Returns nothing when the user has no credential for the endpoint, or a role's source is
    /// gone.
    ///
    /// # Arguments
    ///
    /// * `user` - The user
    /// * `endpoint` - The endpoint the credential is for
    /// * `settings` - The user's settings without secrets, to find a role's source by id
    async fn user_credential(
        &self,
        user: &str,
        endpoint: Uuid,
        settings: Option<&UserModelSettings>,
    ) -> Result<Option<ReadCredential>, RetryLater> {
        // read the credential
        let credential = match self.admin.users.user_model_credential(user, endpoint).await {
            Ok(credential) => credential,
            Err(error) if is_missing(&error) => return Ok(None),
            Err(error) => return Err(retry("Failed to read a user's model credential", error)),
        };
        // an assumed role signs in with another of the user's credentials, found by its id
        let source = match credential.kind.role_source() {
            Some(source) => {
                let read;
                let settings = match settings {
                    Some(settings) => settings,
                    None => {
                        read = self
                            .admin
                            .users
                            .get(user)
                            .await
                            .map_err(|error| retry("Failed to read a user", error))?
                            .settings
                            .ai
                            .map_or_default(|ai| ai.models);
                        &read
                    }
                };
                let Some(key) = settings
                    .credentials
                    .iter()
                    .find(|(_, cred)| cred.id == source)
                    .map(|(key, _)| *key)
                else {
                    return Ok(None);
                };
                match self.admin.users.user_model_credential(user, key).await {
                    Ok(source) => Some(Box::new(ListingCredential {
                        kind: source.kind,
                        secrets: source.secrets,
                        source: None,
                    })),
                    Err(error) if is_missing(&error) => return Ok(None),
                    Err(error) => {
                        return Err(retry("Failed to read a role's source credential", error));
                    }
                }
            }
            None => None,
        };
        Ok(Some(ReadCredential {
            reference: ModelConfigRef::User(credential.id),
            listing: ListingCredential {
                kind: credential.kind,
                secrets: credential.secrets,
                source,
            },
            expires: credential.expires,
            bans: credential.bans,
        }))
    }

    /// List an endpoint's models and build the report, checking the credential's expiry first
    ///
    /// # Arguments
    ///
    /// * `provider` - The endpoint's provider
    /// * `credential` - The credential to list with, if any
    async fn list(
        &self,
        provider: &ModelProvider,
        credential: Option<&ReadCredential>,
    ) -> Result<ModelRefreshReport, RetryLater> {
        // the report names the credential, so only it is banned or cleared
        let named = credential.map(|credential| credential.reference);
        // an expired credential is reported without calling the provider
        if credential
            .and_then(|credential| credential.expires)
            .is_some_and(|expires| expires <= Utc::now())
        {
            return Ok(
                ModelRefreshReport::failed(ModelBanKind::CredentialExpired).with_credential(named)
            );
        }
        // list the models, retrying failures that may pass
        match self
            .lister
            .list(provider, credential.map(|credential| &credential.listing))
            .await
        {
            Ok(models) => Ok(ModelRefreshReport::listed(models).with_credential(named)),
            Err(error) => match error.failure() {
                Some(failure) => Ok(ModelRefreshReport::failed(failure).with_credential(named)),
                None => {
                    event!(
                        Level::WARN,
                        msg = "Retrying a model listing later",
                        error = format!("{error:?}")
                    );
                    Err(RetryLater)
                }
            },
        }
    }

    /// Send a user's report through their own client
    ///
    /// Returns false when the API no longer has anything for the endpoint.
    ///
    /// # Arguments
    ///
    /// * `client` - The user's client
    /// * `endpoint` - The endpoint the report is for
    /// * `report` - The report
    async fn send_user_report(
        &self,
        client: &Thorium,
        endpoint: Uuid,
        report: ModelRefreshReport,
    ) -> Result<bool, RetryLater> {
        // the report merges into the user's own settings
        let update =
            models_update(UserModelSettingsUpdate::default().refresh_report(endpoint, report));
        match client.users.update_self(update).await {
            Ok(_) => Ok(true),
            Err(error) if is_missing(&error) => Ok(false),
            Err(error) => Err(retry("Failed to send a user's refresh report", error)),
        }
    }

    /// Refresh a group endpoint
    ///
    /// It's listed with its group credential, or with the triggering user's own credential when
    /// it has none; with neither, nothing is recorded unless the provider takes no key.
    ///
    /// # Arguments
    ///
    /// * `client` - The client of the user who caused the refresh
    /// * `user` - The user who caused the refresh
    /// * `id` - The endpoint's id
    /// * `models` - The deployment's allowed endpoint list
    /// * `stats` - The stats to count in
    async fn group_endpoint(
        &self,
        client: &Thorium,
        user: &str,
        id: Uuid,
        models: &ModelsConf,
        stats: &mut AiWorkerStats,
    ) -> Result<(), RetryLater> {
        // read the endpoint as the user, so a hidden or deleted one is refreshed by no one
        let entity = match client.entities.get(id).await {
            Ok(entity) => entity,
            Err(error) if is_missing(&error) => return Ok(()),
            Err(error) => return Err(retry("Failed to read a model endpoint", error)),
        };
        let EntityMetadata::ModelEndpoint(meta) = entity.metadata else {
            return Ok(());
        };
        // an endpoint the deployment no longer allows is never contacted
        if !allowed(models, &meta.provider, &meta.bans) {
            event!(
                Level::INFO,
                msg = "Skipping a refresh of an endpoint the deployment doesn't allow",
                endpoint = id.to_string()
            );
            return Ok(());
        }
        // find the credential to list with
        let (credential, own) = match meta.credential {
            Some(credential) => match self.group_credential(credential).await? {
                Some(read) => (Some(read), false),
                None => return Ok(()),
            },
            None => match self.user_credential(user, id, None).await? {
                Some(read) => (Some(read), true),
                None if meta.provider.allows_keyless() => (None, false),
                None => return Ok(()),
            },
        };
        // list the models
        let report = self.list(&meta.provider, credential.as_ref()).await?;
        // count what the merge will change
        let target = MergeTarget {
            catalog: Some(&meta.catalog),
            endpoint_bans: Some(&meta.bans),
            credential_bans: credential.as_ref().map(|credential| &credential.bans),
        };
        count_changes(&target, &report, stats);
        // only admins send an endpoint's report
        let update = EntityUpdate::default()
            .metadata(EntityMetadataUpdate::model_endpoint().refresh_report(report.clone()));
        match self.admin.entities.update(id, update).await {
            Ok(_) => (),
            Err(error) if is_missing(&error) => return Ok(()),
            Err(error) => return Err(retry("Failed to send an endpoint's refresh report", error)),
        }
        // a user's own credential takes the report in their settings too
        if own {
            self.send_user_report(client, id, report).await?;
        }
        Ok(())
    }

    /// Refresh one of a user's own endpoints, or a group endpoint with the user's credential
    ///
    /// # Arguments
    ///
    /// * `client` - The user's client
    /// * `user` - The user
    /// * `settings` - The user's model settings without secrets
    /// * `id` - The endpoint's id
    /// * `models` - The deployment's allowed endpoint list
    /// * `stats` - The stats to count in
    async fn user_endpoint(
        &self,
        client: &Thorium,
        user: &str,
        settings: &UserModelSettings,
        id: Uuid,
        models: &ModelsConf,
        stats: &mut AiWorkerStats,
    ) -> Result<(), RetryLater> {
        // find the endpoint among the user's own, or as a group endpoint they can see
        let own = settings.endpoints.get(&id);
        let group = match own {
            Some(_) => None,
            None => match client.entities.get(id).await {
                Ok(entity) => match entity.metadata {
                    EntityMetadata::ModelEndpoint(meta) => Some(meta),
                    _ => return Ok(()),
                },
                Err(error) if is_missing(&error) => return Ok(()),
                Err(error) => return Err(retry("Failed to read a model endpoint", error)),
            },
        };
        let (provider, bans) = match (own, &group) {
            (Some(own), _) => (&own.provider, &own.bans),
            (None, Some(group)) => (&group.provider, &group.bans),
            (None, None) => return Ok(()),
        };
        // an endpoint the deployment no longer allows is never contacted
        if !allowed(models, provider, bans) {
            event!(
                Level::INFO,
                msg = "Skipping a refresh of an endpoint the deployment doesn't allow",
                endpoint = id.to_string()
            );
            return Ok(());
        }
        // list with the user's credential, or without one for their own keyless endpoint
        let credential = match self.user_credential(user, id, Some(settings)).await? {
            Some(read) => Some(read),
            None if own.is_some() && provider.allows_keyless() => None,
            None => return Ok(()),
        };
        let report = self.list(provider, credential.as_ref()).await?;
        // count what the merge will change in the user's settings
        let target = MergeTarget {
            catalog: own.map(|own| own.catalog.as_slice()),
            endpoint_bans: own.map(|own| &own.bans),
            credential_bans: credential.as_ref().map(|credential| &credential.bans),
        };
        count_changes(&target, &report, stats);
        // send the report as the user
        self.send_user_report(client, id, report).await?;
        Ok(())
    }
}
