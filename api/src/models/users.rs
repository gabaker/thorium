//! Wrappers for interacting with users within Thorium with different backends

use chrono::prelude::*;
use schemars::JsonSchema;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use uuid::Uuid;

use super::{
    CredentialKind, CredentialSecrets, ModelBan, ModelBanUpdate, ModelCatalogEntry,
    ModelCatalogEntryRequest, ModelProvider, ModelRefreshReport, ModelRefreshStatus, ModelSecret,
    RedactSecrets, ScopedModelBan,
};
use crate::{matches_vec, same};

/// The key used to bootstrap cluster when no admins are loaded
#[derive(Serialize, Deserialize, Debug)]
pub struct Key {
    /// The secret key used to bootstrap the cluster
    pub key: String,
}

/// The roles a user can have
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum UserRole {
    /// An admin can see all data in Thorium and perform any action
    Admin,
    /// An Analyst is given access to all samples in Thorium
    Analyst,
    /// A developer can create tools
    Developer {
        #[serde(default)]
        k8s: bool,
        #[serde(default)]
        bare_metal: bool,
        #[serde(default)]
        windows: bool,
        #[serde(default)]
        external: bool,
        #[serde(default)]
        kvm: bool,
    },
    /// A user can upload files and run jobs
    User,
    /// A disabled user that is rejected on any attempt to access Thorium
    Disabled,
}

impl Default for UserRole {
    /// Create a default user role of user
    fn default() -> Self {
        UserRole::User
    }
}

/// Data needed to register a user
#[derive(Serialize, Deserialize, Debug)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct UserCreate {
    /// The username of the user
    pub username: String,
    /// The password for the user
    pub password: String,
    /// This users email
    pub email: String,
    /// The role of this user
    #[serde(default)]
    pub role: UserRole,
    /// Whether this is a local account or not
    #[serde(default)]
    pub local: bool,
    /// The settings this user has set
    #[serde(default)]
    pub settings: UserSettings,
    /// Skip email verification (requires secret key)
    #[serde(default)]
    pub skip_verification: bool,
}

impl UserCreate {
    /// Create a [`UserCreate`] object
    ///
    /// # Arguments
    ///
    /// * `username` - The username of the user to create
    /// * `password` - The new users password
    /// * `email` - An email address for this user
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::models::UserCreate;
    ///
    /// UserCreate::new("mcarson", "Guest", "mcarson@sandia.gov");
    /// ```
    pub fn new<T: Into<String>, P: Into<String>, E: Into<String>>(
        username: T,
        password: P,
        email: E,
    ) -> Self {
        UserCreate {
            username: username.into(),
            password: password.into(),
            email: email.into(),
            role: UserRole::default(),
            local: false,
            settings: UserSettings::default(),
            skip_verification: false,
        }
    }

    /// Sets the users role
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::models::{UserCreate, UserRole};
    ///
    /// UserCreate::new("mcarson", "Guest", "email@email.com")
    ///     .role(
    ///        UserRole::Developer {
    ///            k8s: true,
    ///            bare_metal: false,
    ///            windows: false,
    ///            external: false,
    ///            kvm: false,
    ///        });
    /// ```
    #[must_use]
    pub fn role(mut self, role: UserRole) -> Self {
        self.role = role;
        self
    }

    /// Sets a [`UserCreate`] to create an admin instead of a user
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::models::UserCreate;
    ///
    /// UserCreate::new("mcarson", "Guest", "email@email.com")
    ///     .admin();
    /// ```
    #[must_use]
    pub fn admin(mut self) -> Self {
        self.role = UserRole::Admin;
        self
    }

    /// Set the settings for this new user
    ///
    /// # Arguments
    ///
    /// * `settings` - The settings to set for this user
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::models::{UserCreate, UserSettings, Theme};
    ///
    /// UserCreate::new("mcarson", "Guest", "email@email.com")
    ///    .settings(UserSettings::default().theme(Theme::Dark));
    /// ```
    #[must_use]
    pub fn settings(mut self, settings: UserSettings) -> Self {
        self.settings = settings;
        self
    }

    /// Skip email verification when creating this user
    ///
    /// # Arguments
    ///
    /// * `settings` - The settings to set for this user
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::models::{UserCreate, UserSettings, Theme};
    ///
    /// UserCreate::new("mcarson", "Guest", "email@email.com").skip_verification();
    /// ```
    #[must_use]
    pub fn skip_verification(mut self) -> Self {
        self.skip_verification = true;
        self
    }

    /// Force this user to be locally authed
    pub fn local(mut self) -> Self {
        self.local = true;
        self
    }
}

/// An update for this user
#[derive(Debug, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct UserUpdate {
    /// The updated password for a user
    pub password: Option<String>,
    /// The email address for this user
    pub email: Option<String>,
    /// The role to set for the user
    pub role: Option<UserRole>,
    /// The settings to set for this user
    pub settings: Option<UserSettingsUpdate>,
}

impl UserUpdate {
    /// Set the settings update for this user
    ///
    /// # Arguments
    ///
    /// * `settings` - The settings update to apply
    #[must_use]
    pub fn settings(mut self, settings: UserSettingsUpdate) -> Self {
        self.settings = Some(settings);
        self
    }
}

/// The info to inject about this user on a Unix/Linx system
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, JsonSchema)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct UnixInfo {
    /// The unix user id of this user
    pub user: u64,
    /// The unix group id of this user
    pub group: u64,
}

/// An AI endpoint configuration
#[derive(Serialize, Deserialize, Clone, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct AiEndpoint {
    /// The URL endpoint for the AI service
    pub url: String,
    /// The API key for the AI service (stored only; every API response returns it empty)
    pub api_key: String,
    /// The model to use from this AI service
    pub model: String,
}

impl fmt::Debug for AiEndpoint {
    /// Write this endpoint without its API key
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // never print the key itself
        f.debug_struct("AiEndpoint")
            .field("url", &self.url)
            .field("api_key", &"<redacted>")
            .field("model", &self.model)
            .finish()
    }
}

/// AI settings for a user
#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct AiSettings {
    /// Multiple named AI endpoints
    #[serde(default)]
    pub endpoints: HashMap<String, AiEndpoint>,
    /// The default endpoint name
    #[serde(default)]
    pub default_endpoint: String,
    /// This user's own model endpoints, credentials, and default profiles
    #[serde(default)]
    pub models: UserModelSettings,
}

/// A user's own model configuration
#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct UserModelSettings {
    /// This user's own endpoints, by id
    #[serde(default)]
    pub endpoints: BTreeMap<Uuid, UserModelEndpoint>,
    /// This user's credentials, by the id of the endpoint they authenticate with (a group
    /// endpoint entity or one of `endpoints`)
    #[serde(default)]
    pub credentials: BTreeMap<Uuid, UserCredentialRef>,
    /// The model profile entities to try in order when an agent accepts any model
    #[serde(default)]
    pub default_profiles: Vec<Uuid>,
}

impl UserModelSettings {
    /// Whether these settings hold nothing
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.endpoints.is_empty() && self.credentials.is_empty() && self.default_profiles.is_empty()
    }

    /// Get every ban in these settings by id, with the item each one is on
    #[must_use]
    pub fn ban_index(&self) -> HashMap<Uuid, ScopedModelBan> {
        let mut index = HashMap::new();
        // the bans on each own endpoint and its catalog models
        for (id, endpoint) in &self.endpoints {
            let scope = format!("your model endpoint `{}` ({id})", endpoint.name);
            for ban in endpoint.bans.values() {
                index.insert(ban.id, ScopedModelBan::scoped(scope.clone(), ban.clone()));
            }
            for entry in &endpoint.catalog {
                let scope = format!(
                    "model `{}` on your model endpoint `{}`",
                    entry.model_id, endpoint.name
                );
                for ban in entry.bans.values() {
                    index.insert(ban.id, ScopedModelBan::scoped(scope.clone(), ban.clone()));
                }
            }
        }
        // the bans on each credential, named by its endpoint
        for (endpoint, credential) in &self.credentials {
            let scope = match self.endpoints.get(endpoint) {
                Some(own) => format!("your credential for model endpoint `{}`", own.name),
                None => format!("your credential for model endpoint {endpoint}"),
            };
            for ban in credential.bans.values() {
                index.insert(ban.id, ScopedModelBan::scoped(scope.clone(), ban.clone()));
            }
        }
        index
    }
}

/// One of a user's own model endpoints
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct UserModelEndpoint {
    /// The name of this endpoint
    pub name: String,
    /// The provider and how to connect to it (must be on the deployment's allowed endpoint list)
    pub provider: ModelProvider,
    /// The models this endpoint serves, sorted by `model_id`
    #[serde(default)]
    pub catalog: Vec<ModelCatalogEntry>,
    /// The reasons this endpoint can't be used, by ban id
    #[serde(default)]
    pub bans: HashMap<Uuid, ModelBan>,
    /// The state of this endpoint's most recent refresh
    #[serde(default)]
    pub refresh: ModelRefreshStatus,
    /// Exact model ids this endpoint never resolves to, even when its catalog lists them
    #[serde(default)]
    pub disabled_models: BTreeSet<String>,
}

impl UserModelEndpoint {
    /// Create an endpoint with an empty catalog
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the endpoint
    /// * `provider` - The provider and how to connect to it
    #[must_use]
    pub fn new(name: impl Into<String>, provider: ModelProvider) -> Self {
        UserModelEndpoint {
            name: name.into(),
            provider,
            catalog: Vec::new(),
            bans: HashMap::new(),
            refresh: ModelRefreshStatus::default(),
            disabled_models: BTreeSet::new(),
        }
    }

    /// Add a model to this endpoint's catalog by hand, replacing one with the same id
    ///
    /// # Arguments
    ///
    /// * `model` - The model to add
    #[must_use]
    pub fn model(mut self, model: ModelCatalogEntryRequest) -> Self {
        // keep the catalog sorted by model id
        let entry = model.into_entry();
        match self
            .catalog
            .binary_search_by(|existing| existing.model_id.cmp(&entry.model_id))
        {
            Ok(index) => self.catalog[index] = entry,
            Err(index) => self.catalog.insert(index, entry),
        }
        self
    }

    /// Never resolve this endpoint to a model id
    ///
    /// # Arguments
    ///
    /// * `model_id` - The model id to disable
    #[must_use]
    pub fn disable(mut self, model_id: impl Into<String>) -> Self {
        self.disabled_models.insert(model_id.into());
        self
    }

    /// Mark every catalog entry as added by hand, as entries a user submits are
    #[cfg(feature = "api")]
    pub(crate) fn mark_manual(&mut self) {
        // entries a user submits are manual with manual limits
        for entry in &mut self.catalog {
            entry.source = super::CatalogEntrySource::Manual;
            entry.limits.source = super::LimitsSource::Manual;
        }
    }
}

/// A user's own credential for one endpoint (its secrets are cleared from every response)
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct UserCredentialRef {
    /// The id of this credential (what an `AwsRoleSource::AccessKey` of the same user names)
    pub id: Uuid,
    /// What kind of credential this is and its non-secret metadata
    pub kind: CredentialKind,
    /// The secret values
    #[serde(default)]
    pub secrets: CredentialSecrets,
    /// When this credential stops working, if known
    pub expires: Option<DateTime<Utc>>,
    /// When the secret was last replaced
    pub rotated: Option<DateTime<Utc>>,
    /// The reasons this credential can't be used, by ban id
    #[serde(default)]
    pub bans: HashMap<Uuid, ModelBan>,
}

impl UserCredentialRef {
    /// Create a credential with a new id
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of credential and its metadata
    /// * `secret` - The primary secret, if the kind has one
    #[must_use]
    pub fn new(kind: CredentialKind, secret: Option<ModelSecret>) -> Self {
        UserCredentialRef {
            id: Uuid::new_v4(),
            kind,
            secrets: CredentialSecrets {
                primary: secret,
                session_token: None,
                fingerprint: None,
            },
            expires: None,
            rotated: None,
            bans: HashMap::new(),
        }
    }

    /// Set the AWS session token of this credential
    ///
    /// # Arguments
    ///
    /// * `token` - The session token
    #[must_use]
    pub fn session_token(mut self, token: ModelSecret) -> Self {
        self.secrets.session_token = Some(token);
        self
    }

    /// Set when this credential stops working
    ///
    /// # Arguments
    ///
    /// * `expires` - When the credential stops working
    #[must_use]
    pub fn expires(mut self, expires: DateTime<Utc>) -> Self {
        self.expires = Some(expires);
        self
    }
}

/// An update to a user's own model configuration
#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct UserModelSettingsUpdate {
    /// Endpoints to add or replace, by id
    #[serde(default)]
    pub endpoints: BTreeMap<Uuid, UserModelEndpoint>,
    /// Endpoints to remove by id, with their credentials
    #[serde(default)]
    pub remove_endpoints: Vec<Uuid>,
    /// Credentials to add or rotate, by endpoint id (an omitted secret keeps the stored one)
    #[serde(default)]
    pub credentials: BTreeMap<Uuid, UserCredentialRef>,
    /// Credentials to remove by endpoint id
    #[serde(default)]
    pub remove_credentials: Vec<Uuid>,
    /// The new ordered default profiles
    pub default_profiles: Option<Vec<Uuid>>,
    /// Bans to add to an own endpoint (or to the credential for a group endpoint), and ban ids
    /// to remove from the endpoint, its catalog models, or its credential, by endpoint id
    #[serde(default)]
    pub bans: BTreeMap<Uuid, ModelBanUpdate>,
    /// Bans to add or remove on the catalog models of own endpoints, by endpoint id and model id
    #[serde(default)]
    pub model_bans: BTreeMap<Uuid, BTreeMap<String, ModelBanUpdate>>,
    /// Bans to add or remove on credentials, by the id of the endpoint they authenticate with
    #[serde(default)]
    pub credential_bans: BTreeMap<Uuid, ModelBanUpdate>,
    /// Listing results to merge into this user's endpoint catalogs and credentials, by endpoint
    /// id (sent by the AI worker as the user)
    #[serde(default)]
    pub refresh_reports: BTreeMap<Uuid, ModelRefreshReport>,
    /// Model ids to add to the `disabled_models` of this user's own endpoints, by endpoint id
    #[serde(default)]
    pub disable_models: BTreeMap<Uuid, BTreeSet<String>>,
    /// Model ids to remove from the `disabled_models` of this user's own endpoints, by endpoint id
    #[serde(default)]
    pub enable_models: BTreeMap<Uuid, BTreeSet<String>>,
}

impl UserModelSettingsUpdate {
    /// Add or replace one of the user's own endpoints
    ///
    /// # Arguments
    ///
    /// * `id` - The endpoint's id
    /// * `endpoint` - The endpoint
    #[must_use]
    pub fn endpoint(mut self, id: Uuid, endpoint: UserModelEndpoint) -> Self {
        self.endpoints.insert(id, endpoint);
        self
    }

    /// Remove one of the user's own endpoints and its credential
    ///
    /// # Arguments
    ///
    /// * `id` - The endpoint's id
    #[must_use]
    pub fn remove_endpoint(mut self, id: Uuid) -> Self {
        self.remove_endpoints.push(id);
        self
    }

    /// Add or rotate the user's credential for an endpoint
    ///
    /// # Arguments
    ///
    /// * `endpoint` - The id of the endpoint the credential authenticates with
    /// * `credential` - The credential
    #[must_use]
    pub fn credential(mut self, endpoint: Uuid, credential: UserCredentialRef) -> Self {
        self.credentials.insert(endpoint, credential);
        self
    }

    /// Remove the user's credential for an endpoint
    ///
    /// # Arguments
    ///
    /// * `endpoint` - The id of the endpoint the credential authenticates with
    #[must_use]
    pub fn remove_credential(mut self, endpoint: Uuid) -> Self {
        self.remove_credentials.push(endpoint);
        self
    }

    /// Replace the ordered default profiles
    ///
    /// # Arguments
    ///
    /// * `profiles` - The model profile entities to try in order
    #[must_use]
    pub fn default_profiles(mut self, profiles: Vec<Uuid>) -> Self {
        self.default_profiles = Some(profiles);
        self
    }

    /// Set the ban changes keyed by an endpoint
    ///
    /// # Arguments
    ///
    /// * `endpoint` - The endpoint's id
    /// * `update` - The ban changes
    #[must_use]
    pub fn bans(mut self, endpoint: Uuid, update: ModelBanUpdate) -> Self {
        self.bans.insert(endpoint, update);
        self
    }

    /// Set the ban changes of a catalog model on one of the user's own endpoints
    ///
    /// # Arguments
    ///
    /// * `endpoint` - The endpoint's id
    /// * `model_id` - The catalog model's id
    /// * `update` - The ban changes
    #[must_use]
    pub fn model_bans(
        mut self,
        endpoint: Uuid,
        model_id: impl Into<String>,
        update: ModelBanUpdate,
    ) -> Self {
        self.model_bans
            .entry(endpoint)
            .or_default()
            .insert(model_id.into(), update);
        self
    }

    /// Set the ban changes of the user's credential for an endpoint
    ///
    /// # Arguments
    ///
    /// * `endpoint` - The id of the endpoint the credential authenticates with
    /// * `update` - The ban changes
    #[must_use]
    pub fn credential_bans(mut self, endpoint: Uuid, update: ModelBanUpdate) -> Self {
        self.credential_bans.insert(endpoint, update);
        self
    }

    /// Set the listing result to merge for one of the user's endpoints or credentials
    ///
    /// # Arguments
    ///
    /// * `endpoint` - The id of the endpoint the listing was for
    /// * `report` - The listing result
    #[must_use]
    pub fn refresh_report(mut self, endpoint: Uuid, report: ModelRefreshReport) -> Self {
        self.refresh_reports.insert(endpoint, report);
        self
    }

    /// Disable a model id on one of the user's own endpoints
    ///
    /// # Arguments
    ///
    /// * `endpoint` - The endpoint's id
    /// * `model_id` - The model id to disable
    #[must_use]
    pub fn disable_model(mut self, endpoint: Uuid, model_id: impl Into<String>) -> Self {
        self.disable_models
            .entry(endpoint)
            .or_default()
            .insert(model_id.into());
        self
    }

    /// Enable a disabled model id on one of the user's own endpoints
    ///
    /// # Arguments
    ///
    /// * `endpoint` - The endpoint's id
    /// * `model_id` - The model id to enable
    #[must_use]
    pub fn enable_model(mut self, endpoint: Uuid, model_id: impl Into<String>) -> Self {
        self.enable_models
            .entry(endpoint)
            .or_default()
            .insert(model_id.into());
        self
    }
}

/// The different themes supported in the webUI
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum Theme {
    /// A dark theme
    Dark,
    /// A light theme
    Light,
    /// A crab theme
    Crab,
    /// A dark blue theme
    Ocean,
    /// Use the theme closest to browser settings
    Automatic,
}

impl Default for Theme {
    /// Set the default theme to Automatic
    fn default() -> Theme {
        Theme::Automatic
    }
}

/// Any user specific settings
#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct UserSettings {
    /// The theme this user uses in the webUI
    pub theme: Theme,
    /// The AI settings for this user
    #[serde(default)]
    pub ai: Option<AiSettings>,
}

impl RedactSecrets for UserSettings {
    /// Clear every model credential secret and legacy AI key, keeping fingerprints
    fn redact_secrets(&mut self) {
        // only AI settings hold secrets
        if let Some(ai) = &mut self.ai {
            // legacy keys are returned empty
            for endpoint in ai.endpoints.values_mut() {
                endpoint.api_key.clear();
            }
            // credential secrets are cleared
            for credential in ai.models.credentials.values_mut() {
                credential.secrets.redact_secrets();
            }
        }
    }
}

impl UserSettings {
    /// Set the theme
    ///
    /// # Arguments
    ///
    /// * `theme` - The theme to set
    pub fn theme(mut self, theme: Theme) -> Self {
        self.theme = theme;
        self
    }
}

/// An update to an AI endpoint configuration
#[derive(Serialize, Deserialize, Clone, Default, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct AiEndpointUpdate {
    /// The URL endpoint for the AI service
    pub url: Option<String>,
    /// The API key for the AI service
    pub api_key: Option<String>,
    /// The model to use from this AI service
    pub model: Option<String>,
}

impl fmt::Debug for AiEndpointUpdate {
    /// Write this update without its API key
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // only say whether a key is set
        f.debug_struct("AiEndpointUpdate")
            .field("url", &self.url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("model", &self.model)
            .finish()
    }
}

impl AiEndpointUpdate {
    /// Set the URL for the AI endpoint
    ///
    /// # Arguments
    ///
    /// * `url` - The URL endpoint for the AI service
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::models::AiEndpointUpdate;
    ///
    /// AiEndpointUpdate::default().url("https://<llm-server>");
    /// ```
    #[must_use]
    pub fn url(mut self, url: impl Into<String>) -> Self {
        self.url = Some(url.into());
        self
    }

    /// Set the API key for the AI endpoint
    ///
    /// # Arguments
    ///
    /// * `api_key` - The API key for the AI service
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::models::AiEndpointUpdate;
    ///
    /// AiEndpointUpdate::default().api_key("<api_key>");
    /// ```
    #[must_use]
    pub fn api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }

    /// Set the model for the AI endpoint
    ///
    /// # Arguments
    ///
    /// * `model` - The model to use from this AI service
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::models::AiEndpointUpdate;
    ///
    /// AiEndpointUpdate::default().model("<model>");
    /// ```
    #[must_use]
    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Check if this update contains everything needed for a new endpoint
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.url.is_some() && self.api_key.is_some() && self.model.is_some()
    }
}

/// An update to AI settings for a user
#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct AiSettingsUpdate {
    /// Endpoints to add/update
    #[serde(default)]
    pub endpoints: HashMap<String, AiEndpointUpdate>,
    /// Endpoint names to remove
    #[serde(default)]
    pub remove_endpoints: Vec<String>,
    /// Set the default endpoint
    #[serde(default)]
    pub default_endpoint: Option<String>,
    /// An update to this user's own model configuration
    pub models: Option<UserModelSettingsUpdate>,
}

impl AiSettingsUpdate {
    /// Create a new AI settings update
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the endpoint to update
    /// * `update` - The endpoint update to set
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::models::{AiSettingsUpdate, AiEndpointUpdate};
    ///
    /// AiSettingsUpdate::default().endpoint("<endpoint>", AiEndpointUpdate::default().api_key("<api_key>"));
    /// ```
    #[must_use]
    pub fn endpoint(mut self, name: impl Into<String>, update: AiEndpointUpdate) -> Self {
        // insert or repalce this endpoints settings
        self.endpoints.insert(name.into(), update);
        self
    }

    /// Add an endpoint name to be removed from the AI settings
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the endpoint to remove
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::models::AiSettingsUpdate;
    ///
    /// AiSettingsUpdate::default().remove_endpoint("old_endpoint");
    /// ```
    #[must_use]
    pub fn remove_endpoint(mut self, name: impl Into<String>) -> Self {
        self.remove_endpoints.push(name.into());
        self
    }

    /// Set the default endpoint for AI operations
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the endpoint to set as default
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::models::AiSettingsUpdate;
    ///
    /// AiSettingsUpdate::default().default_endpoint("main_endpoint");
    /// ```
    #[must_use]
    pub fn default_endpoint(mut self, name: impl Into<String>) -> Self {
        self.default_endpoint = Some(name.into());
        self
    }

    /// Set the update to this user's own model configuration
    ///
    /// # Arguments
    ///
    /// * `models` - The model settings update to apply
    #[must_use]
    pub fn models(mut self, models: UserModelSettingsUpdate) -> Self {
        self.models = Some(models);
        self
    }
}

/// The update to apply to a users settings
#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct UserSettingsUpdate {
    /// The theme this user uses in the webUI
    pub theme: Option<Theme>,
    /// The AI settings update for this user
    pub ai: Option<AiSettingsUpdate>,
}

impl UserSettingsUpdate {
    /// Update the theme for this user
    ///
    /// # Arguments
    ///
    /// * `theme` - The theme to set
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::models::{UserSettingsUpdate, Theme};
    ///
    /// UserSettingsUpdate::default().theme(Theme::Dark);
    /// ```
    #[must_use]
    pub fn theme(mut self, theme: Theme) -> Self {
        self.theme = Some(theme);
        self
    }

    /// Update the AI settings for this user
    ///
    /// # Arguments
    ///
    /// * `ai` - The AI settings update to apply
    #[must_use]
    pub fn ai(mut self, ai: AiSettingsUpdate) -> Self {
        self.ai = Some(ai);
        self
    }
}

/// A user within Thorium
#[derive(Serialize, Deserialize, Clone)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct User {
    /// The username of this user
    pub username: String,
    /// This users password if ldap is not being used
    pub password: Option<String>,
    /// This users email
    pub email: String,
    /// The role for this user (admin || user || service)
    pub role: UserRole,
    /// The groups this user is in
    pub groups: Vec<String>,
    /// If this user is an admin this will be the groups they are actually in
    pub actual_groups: Vec<String>,
    /// The token for this user
    pub token: String,
    /// When this users token expires
    pub token_expiration: DateTime<Utc>,
    /// The info to inject about this user on unix/linux systems
    pub unix: Option<UnixInfo>,
    /// The settings this user has set
    pub settings: UserSettings,
    /// Whether this user has been verified already or not
    pub verified: bool,
    /// The verification token to check against if one has been set
    pub verification_token: Option<String>,
    /// When a verification email was last sent
    pub verification_sent: Option<DateTime<Utc>>,
    /// The different auth provider aliases by their provider
    pub aliases: HashMap<String, String>,
    /// The S3 path to this user's profile icon, if one has been set
    pub image: Option<String>,
}

impl fmt::Debug for User {
    /// Write this user without its password hash, its verification token, or its token, which
    /// is a scoped token's value when the user authenticated with one
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // never print the credentials themselves, only whether the optional ones are set
        f.debug_struct("User")
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("email", &self.email)
            .field("role", &self.role)
            .field("groups", &self.groups)
            .field("actual_groups", &self.actual_groups)
            .field("token", &"<redacted>")
            .field("token_expiration", &self.token_expiration)
            .field("unix", &self.unix)
            .field("settings", &self.settings)
            .field("verified", &self.verified)
            .field(
                "verification_token",
                &self.verification_token.as_ref().map(|_| "<redacted>"),
            )
            .field("verification_sent", &self.verification_sent)
            .field("aliases", &self.aliases)
            .field("image", &self.image)
            .finish()
    }
}

/// A user within Thorium that does not have its password
#[derive(Serialize, Deserialize, Clone)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ScrubbedUser {
    /// The username of this user
    pub username: String,
    /// The role for this user (admin || user)
    pub role: UserRole,
    /// This users email
    pub email: String,
    /// The groups this user is in
    pub groups: Vec<String>,
    /// If this user is an admin this will be the groups they are actually in
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actual_groups: Vec<String>,
    /// The token for this user
    pub token: String,
    /// When this users token expires
    pub token_expiration: DateTime<Utc>,
    /// The info to inject about this user on unix/linux systems
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unix: Option<UnixInfo>,
    /// The settings this user has set
    pub settings: UserSettings,
    /// Whether this user is a local user or not
    pub local: bool,
    /// Whether this user has been verified already or not
    pub verified: bool,
    /// Whether this user has a profile icon set
    ///
    /// The icon itself is fetched lazily from `GET /users/user/{username}/image` rather than
    /// embedded here, so cheap/frequent calls like `whoami` don't ship the image bytes.
    pub has_image: bool,
}

impl fmt::Debug for ScrubbedUser {
    /// Write this user without its token
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // never print the token itself
        f.debug_struct("ScrubbedUser")
            .field("username", &self.username)
            .field("role", &self.role)
            .field("email", &self.email)
            .field("groups", &self.groups)
            .field("actual_groups", &self.actual_groups)
            .field("token", &"<redacted>")
            .field("token_expiration", &self.token_expiration)
            .field("unix", &self.unix)
            .field("settings", &self.settings)
            .field("local", &self.local)
            .field("verified", &self.verified)
            .field("has_image", &self.has_image)
            .finish()
    }
}

impl PartialEq<ScrubbedUser> for ScrubbedUser {
    /// Check if a [`ScrubbedUser`] and a [`ScrubbedUser`] are equal
    ///
    /// # Arguments
    ///
    /// * `request` - The [`ScrubbedUser`] to compare against
    fn eq(&self, request: &ScrubbedUser) -> bool {
        // make sure the username is the same
        same!(self.username, request.username);
        // make sure the role is the same
        same!(self.role, request.role);
        // make sure the email is the same
        same!(self.email, request.email);
        // make sure the group list is the same
        matches_vec!(self.groups, request.groups);
        // make sure the token and its expriation are the same
        same!(self.token, request.token);
        same!(self.token_expiration, request.token_expiration);
        // make sure our settings are the same
        same!(self.settings, request.settings);
        // make sure our local user info is the same
        same!(self.local, request.local);
        // make sure our verification is the same
        same!(self.verified, request.verified);
        // make sure our profile icon presence is the same
        same!(self.has_image, request.has_image);
        true
    }
}

/// Response to a sucessful auth
///
/// If email support is configured then users may get a VerifyEmail variant if
/// their email has not yet been verified.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum AuthResponse {
    /// This user successfully authenticated and has a verified email
    Authed {
        /// The token to use to talk to Thorium
        token: String,
        /// The date/time this token expires
        expires: DateTime<Utc>,
    },
    /// This user successfully authenticated but needs to verify their email
    VerifyEmail(String),
}

/// The roles a scoped token can have
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum ScopedTokenRole {
    /// A developer can create tools
    Developer {
        #[serde(default)]
        k8s: bool,
        #[serde(default)]
        bare_metal: bool,
        #[serde(default)]
        windows: bool,
        #[serde(default)]
        external: bool,
        #[serde(default)]
        kvm: bool,
    },
    /// A user can upload files and run jobs
    User,
}

/// A request to create a scoped token limited to a subset of a users groups
#[derive(Serialize, Deserialize, Debug, Clone)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ScopedTokenRequest {
    /// The name of this scoped token
    pub name: String,
    /// The role for this scoped token
    pub role: ScopedTokenRole,
    /// The groups this scoped token is limited to
    pub groups: Vec<String>,
    /// When this scoped token permanently expires if it is ephemeral
    pub expires: Option<DateTime<Utc>>,
}

impl ScopedTokenRequest {
    /// Creates a new scoped token request
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the scoped token to create
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::models::ScopedTokenRequest;
    ///
    /// ScopedTokenRequest::new("corn-harvester")
    ///     .group("CornPeeps");
    /// ```
    pub fn new<T: Into<String>>(name: T) -> Self {
        ScopedTokenRequest {
            name: name.into(),
            role: ScopedTokenRole::User,
            groups: Vec::default(),
            expires: None,
        }
    }

    /// Sets the role for this scoped token
    ///
    /// # Arguments
    ///
    /// * `role` - The role to set for this scoped tokens scope
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::models::{ScopedTokenRequest, ScopedTokenRole};
    ///
    /// ScopedTokenRequest::new("corn-harvester")
    ///     .role(ScopedTokenRole::User);
    /// ```
    pub fn role(mut self, role: ScopedTokenRole) -> Self {
        self.role = role;
        self
    }

    /// Adds a group to limit this scoped token to
    ///
    /// # Arguments
    ///
    /// * `group` - The group to add to this scoped tokens scope
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::models::ScopedTokenRequest;
    ///
    /// ScopedTokenRequest::new("corn-harvester")
    ///     .group("CornPeeps");
    /// ```
    #[must_use]
    pub fn group<T: Into<String>>(mut self, group: T) -> Self {
        // add this group to our scope
        self.groups.push(group.into());
        self
    }

    /// Adds multiple groups to limit this scoped token to
    ///
    /// # Arguments
    ///
    /// * `groups` - The groups to add to this scoped tokens scope
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::models::ScopedTokenRequest;
    ///
    /// ScopedTokenRequest::new("corn-harvester")
    ///     .groups(vec!("CornPeeps", "CornFans"));
    /// ```
    #[must_use]
    pub fn groups<T: Into<String>>(mut self, groups: Vec<T>) -> Self {
        // add these groups to our scope
        self.groups.extend(groups.into_iter().map(Into::into));
        self
    }

    /// Sets the date this scoped token permanently expires making it ephemeral
    ///
    /// # Arguments
    ///
    /// * `expires` - The date/time this scoped token permanently expires
    ///
    /// # Examples
    ///
    /// ```
    /// use chrono::{Utc, Duration};
    /// use thorium::models::ScopedTokenRequest;
    ///
    /// ScopedTokenRequest::new("corn-harvester")
    ///     .group("CornPeeps")
    ///     .expires(Utc::now() + Duration::days(7));
    /// ```
    #[must_use]
    pub fn expires(mut self, expires: DateTime<Utc>) -> Self {
        // set the date this token permanently expires
        self.expires = Some(expires);
        self
    }
}

/// An update to apply to a scoped token
///
/// Updates never change a scoped tokens value so activated tokens keep
/// working after an update.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ScopedTokenUpdate {
    /// The new role for this scoped token
    pub role: Option<ScopedTokenRole>,
    /// The groups to add to this scoped tokens scope
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub add_groups: Vec<String>,
    /// The groups to remove from this scoped tokens scope
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove_groups: Vec<String>,
    /// The new date this scoped token permanently expires
    ///
    /// Incompatible with `clear_expires`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires: Option<DateTime<Utc>>,
    /// Clear this scoped tokens expiration date making it no longer ephemeral
    ///
    /// Incompatible with `expires`
    #[serde(default)]
    pub clear_expires: bool,
}

impl ScopedTokenUpdate {
    /// Check to see if this update is empty
    #[must_use]
    pub fn is_empty(&self) -> bool {
        // an update is empty if it matches the default update
        *self == Self::default()
    }

    /// Sets the role for this scoped token
    ///
    /// # Arguments
    ///
    /// * `role` - The role to set for this scoped tokens scope
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::models::{ScopedTokenUpdate, ScopedTokenRole};
    ///
    /// ScopedTokenUpdate::default().role(ScopedTokenRole::User);
    /// ```
    pub fn role(mut self, role: ScopedTokenRole) -> Self {
        self.role = Some(role);
        self
    }

    /// Add a group to this scoped tokens scope
    ///
    /// # Arguments
    ///
    /// * `group` - The group to add to this scoped tokens scope
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::models::ScopedTokenUpdate;
    ///
    /// ScopedTokenUpdate::default().add_group("CornPeeps");
    /// ```
    #[must_use]
    pub fn add_group<T: Into<String>>(mut self, group: T) -> Self {
        // add this group to the groups to add
        self.add_groups.push(group.into());
        self
    }

    /// Add multiple groups to this scoped tokens scope
    ///
    /// # Arguments
    ///
    /// * `groups` - The groups to add to this scoped tokens scope
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::models::ScopedTokenUpdate;
    ///
    /// ScopedTokenUpdate::default().add_groups(vec!("CornPeeps", "CornFans"));
    /// ```
    #[must_use]
    pub fn add_groups<T: Into<String>>(mut self, groups: Vec<T>) -> Self {
        // add these groups to the groups to add
        self.add_groups.extend(groups.into_iter().map(Into::into));
        self
    }

    /// Remove a group from this scoped tokens scope
    ///
    /// # Arguments
    ///
    /// * `group` - The group to remove from this scoped tokens scope
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::models::ScopedTokenUpdate;
    ///
    /// ScopedTokenUpdate::default().remove_group("CornPeeps");
    /// ```
    #[must_use]
    pub fn remove_group<T: Into<String>>(mut self, group: T) -> Self {
        // add this group to the groups to remove
        self.remove_groups.push(group.into());
        self
    }

    /// Remove multiple groups from this scoped tokens scope
    ///
    /// # Arguments
    ///
    /// * `groups` - The groups to remove from this scoped tokens scope
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::models::ScopedTokenUpdate;
    ///
    /// ScopedTokenUpdate::default().remove_groups(vec!("CornPeeps", "CornFans"));
    /// ```
    #[must_use]
    pub fn remove_groups<T: Into<String>>(mut self, groups: Vec<T>) -> Self {
        // add these groups to the groups to remove
        self.remove_groups
            .extend(groups.into_iter().map(Into::into));
        self
    }

    /// Set a new date this scoped token permanently expires
    ///
    /// # Arguments
    ///
    /// * `expires` - The new date/time this scoped token permanently expires
    ///
    /// # Examples
    ///
    /// ```
    /// use chrono::{Utc, Duration};
    /// use thorium::models::ScopedTokenUpdate;
    ///
    /// ScopedTokenUpdate::default().expires(Utc::now() + Duration::days(7));
    /// ```
    #[must_use]
    pub fn expires(mut self, expires: DateTime<Utc>) -> Self {
        // set the new date this token permanently expires
        self.expires = Some(expires);
        self
    }

    /// Clear this scoped tokens expiration date making it no longer ephemeral
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::models::ScopedTokenUpdate;
    ///
    /// ScopedTokenUpdate::default().clear_expires();
    /// ```
    #[must_use]
    pub fn clear_expires(mut self) -> Self {
        // clear this tokens expiration date
        self.clear_expires = true;
        self
    }
}

/// A token tied to a user that is limited to a subset of that users groups
#[derive(Serialize, Deserialize, Debug, Clone)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ScopedToken {
    /// The name of this scoped token
    pub name: String,
    /// The username of the user this scoped token is tied to
    pub owner: String,
    /// The role for this scoped token
    pub role: ScopedTokenRole,
    /// The groups this scoped token is limited to
    pub groups: Vec<String>,
    /// The current value of this scoped token
    pub token: String,
    /// When this scoped tokens current value expires and gets rotated
    pub token_expiration: DateTime<Utc>,
    /// When this scoped token permanently expires if it is ephemeral
    pub expires: Option<DateTime<Utc>>,
}

/// A user that authenticated with a scoped token
///
/// The wrapped user has its groups limited to this scoped tokens scope and
/// carries the scoped tokens value/expiration instead of the users primary
/// token.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ScopedUser {
    /// The user this scoped token is tied to with its groups limited to scope
    pub user: User,
    /// The name of the scoped token used to authenticate
    pub name: String,
}

/// An authenticated user in Thorium
///
/// Users are either fully authenticated with their primary token/password or
/// authenticated with a scoped token that limits them to a subset of their
/// groups.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum AuthedUser {
    /// A user authenticated with their primary token or password
    Full(User),
    /// A user authenticated with a scoped token limited to a subset of groups
    Scoped(ScopedUser),
}

impl AuthedUser {
    /// Convert this authenticated user into its effective [`User`]
    ///
    /// For scoped users this returns the user with its groups limited to the
    /// scoped tokens scope.
    #[must_use]
    pub fn into_user(self) -> User {
        // get the effective user for this authed user
        match self {
            AuthedUser::Full(user) => user,
            AuthedUser::Scoped(scoped) => scoped.user,
        }
    }

    /// Whether this user authenticated with a scoped token or not
    #[must_use]
    pub fn is_scoped(&self) -> bool {
        // check if we are the scoped variant
        matches!(self, AuthedUser::Scoped(_))
    }
}

impl std::ops::Deref for AuthedUser {
    type Target = User;

    /// Deref to the effective [`User`] for this authenticated user
    fn deref(&self) -> &Self::Target {
        // get a reference to the effective user for this authed user
        match self {
            AuthedUser::Full(user) => user,
            AuthedUser::Scoped(scoped) => &scoped.user,
        }
    }
}

cfg_if::cfg_if! {
    if #[cfg(any(feature = "api", feature = "client"))] {
        use crate::models::backends::NotificationSupport;
        use crate::models::{KeySupport, NotificationType};

        impl NotificationSupport for User {
            /// Provide the user notification type
            fn notification_type() -> NotificationType {
                NotificationType::Users
            }
        }

        impl KeySupport for User {
            /// The user's username
            type Key = String;

            /// Users have no extra components for their keys
            type ExtraKey = ();

            /// Build the key for this user
            ///
            /// # Arguments
            ///
            /// * `key` - The username
            /// * `_extra` - Unused; these keys have no extra components
            fn build_key(key: Self::Key, _extra: &Self::ExtraKey) -> String {
                key
            }

            /// Build a URL component for this user
            ///
            /// # Arguments
            ///
            /// * `key` - The username
            /// * `_extra` - Unused; these keys have no extra components
            fn key_url(key: &Self::Key, _extra: Option<&Self::ExtraKey>) -> String {
                key.clone()
            }
        }
    }
}
