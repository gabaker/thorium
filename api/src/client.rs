//! Asynchronous and synchronus clients for Thorium

use base64::Engine as _;
use chrono::prelude::*;
use std::path::{Path, PathBuf};

use crate::models::{self, AuthResponse, ScrubbedUser};

mod associations;
mod basic;
pub mod conf;
mod cursors;
mod entities;
mod error;
mod events;
mod files;
mod groups;
mod helpers;
mod images;
mod jobs;
mod keys;
mod network_policies;
mod pipelines;
mod reactions;
mod repos;
mod search;
mod streams;
mod system;
mod traits;
mod trees;
mod updates;
mod users;
mod utils;

pub use associations::Associations;
pub use basic::Basic;
pub use conf::{ClientSettings, ContainerRuntime, CtlConf};
pub use cursors::{Cursor, LogsCursor, SearchDate};
pub use entities::Entities;
pub use error::Error;
pub use events::Events;
pub use files::Files;
pub use groups::Groups;
pub use images::Images;
pub use jobs::Jobs;
pub use keys::Keys;
pub use network_policies::NetworkPolicies;
pub use pipelines::Pipelines;
pub use reactions::Reactions;
pub use repos::Repos;
pub use search::Search;
pub use search::events::results::ResultSearchEvents;
pub use search::events::tags::TagSearchEvents;
pub use search::events::{SearchEvents, SearchEventsClient};
pub use streams::Streams;
pub use system::System;
pub use traits::GrepClient;
pub use traits::ResultsClient;
pub use trees::Trees;
pub use updates::Updates;
pub use users::Users;

// if the blocking client is enabled then expose blocking subclients
cfg_if::cfg_if! {
    if #[cfg(feature = "sync")] {
        pub use basic::BasicBlocking;
        pub use files::FilesBlocking;
        pub use groups::GroupsBlocking;
        pub use images::ImagesBlocking;
        pub use jobs::JobsBlocking;
        pub use pipelines::PipelinesBlocking;
        pub use reactions::ReactionsBlocking;
        pub use repos::ReposBlocking;
        pub use search::SearchBlocking;
        pub use streams::StreamsBlocking;
        pub use system::SystemBlocking;
        pub use users::UsersBlocking;
        pub use events::EventsBlocking;
        pub use network_policies::NetworkPoliciesBlocking;
        pub use trees::TreesBlocking;
        pub use updates::UpdatesBlocking;
        pub use entities::EntitiesBlocking;
        pub use associations::AssociationsBlocking;

        // expose blocking traits
        pub use traits::GrepClientBlocking;
        pub use traits::ResultsClientBlocking;

        // get needed imports
        use std::sync::LazyLock;
        use tokio::runtime::Runtime;

        /// A global runtime the blocking client
        pub static RUNTIME: LazyLock<Runtime> = LazyLock::new(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap()
        });
    }
}

#[cfg(feature = "python")]
mod python;
#[cfg(feature = "python")]
use pyo3::pyclass;

/// Builds the Thorium client
#[derive(Debug, Clone)]
pub struct ThoriumClientBuilder {
    /// The host/domain the Thorium api can be found at
    host: String,
    /// The username to login with
    username: Option<String>,
    /// The password to login with
    password: Option<String>,
    /// A token to use instead of a username/password combo
    token: Option<String>,
    /// The settings for thorctls client
    pub settings: ClientSettings,
}

impl ThoriumClientBuilder {
    /// Sets a basic auth username/password combo to use for authentication
    ///
    /// # Arguments
    ///
    /// * `username` - The username to authetnicate with
    /// * `password` - The password to authenticate with
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::Thorium;
    ///
    /// Thorium::build("http://127.0.0.1")
    ///     .basic_auth("user", "password");
    /// ```
    #[must_use]
    pub fn basic_auth<T: Into<String>>(mut self, username: T, password: T) -> Self {
        // set a username/password combo for basic auth
        self.username = Some(username.into());
        self.password = Some(password.into());
        self
    }

    /// Sets a token to use for authentication instead of username/password
    ///
    /// # Arguments
    ///
    /// * `token` - The token to use to authenticate
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::Thorium;
    ///
    /// Thorium::build("http://127.0.0.1").token("token");
    /// ```
    #[must_use]
    pub fn token<T: Into<String>>(mut self, token: T) -> Self {
        // Set a token for token based auth
        self.token = Some(token.into());
        self
    }

    /// Allow insecure invalid certificates to be trusted
    #[must_use]
    pub fn danger_accept_invalid_certs(mut self) -> Self {
        self.settings.invalid_certs = true;
        self
    }

    /// Allow insecure invalid hostnames to be trusted
    #[must_use]
    pub fn danger_accept_invalid_hostnames(mut self) -> Self {
        self.settings.invalid_hostnames = true;
        self
    }

    /// Adds a custom CA to be trusted
    ///
    /// # Arguments
    ///
    /// * `path` - The path to the certificate authority to trust
    #[must_use]
    pub fn add_certificate_authority<P: Into<PathBuf>>(mut self, path: P) -> Self {
        self.settings.certificate_authorities.push(path.into());
        self
    }

    /// Disable all proxies and connect to hosts directly
    ///
    /// By default the client respects proxy settings from the environment
    /// (`HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY`, honoring `NO_PROXY`). This disables that and
    /// connects directly, which is useful for in-cluster traffic.
    #[must_use]
    pub fn disable_proxy(mut self) -> Self {
        self.settings.disable_proxy = true;
        self
    }

    /// Route all traffic through an explicit proxy
    ///
    /// This takes precedence over environment proxy settings but is ignored when
    /// [`Self::disable_proxy`] is set. The URL may embed credentials (e.g.
    /// `http://user:pass@host:port`).
    ///
    /// # Arguments
    ///
    /// * `proxy` - The proxy URL to route traffic through
    #[must_use]
    pub fn proxy<T: Into<String>>(mut self, proxy: T) -> Self {
        self.settings.proxy = Some(proxy.into());
        self
    }

    /// Set the hosts that should bypass an explicit proxy
    ///
    /// This mirrors the `NO_PROXY` environment variable and only applies when a proxy is set
    /// via [`Self::proxy`].
    ///
    /// # Arguments
    ///
    /// * `no_proxy` - A comma-separated list of hosts/domains/CIDRs that bypass the proxy
    #[must_use]
    pub fn no_proxy<T: Into<String>>(mut self, no_proxy: T) -> Self {
        self.settings.no_proxy = Some(no_proxy.into());
        self
    }

    /// Load auth info from a key file on disk
    ///
    /// # Arguments
    ///
    /// * `path` - The path to load auth info from
    pub fn from_keys(self, path: &str) -> Result<Self, Error> {
        // load auth keys
        let keys = Keys::new(path)?;
        // use the correct auth method based on what is defined in the config
        match (keys.username, keys.password, keys.token) {
            (Some(user), Some(pass), _) => Ok(self.basic_auth(user, pass)),
            (_, None, Some(token)) => Ok(self.token(token)),
            (_, _, _) => Err(Error::new("Either username/password or token must be set")),
        }
    }

    /// Load auth info and settings from a [`CtlConf`]
    ///
    /// # Arguments
    ///
    /// * `ctl_conf` - The `CtlConf` to build from
    pub fn from_ctl_conf(mut self, ctl_conf: CtlConf) -> Result<Self, Error> {
        // get our keys object from our ctl conf
        let keys = ctl_conf.keys;
        // save our settings from this ctl conf
        self.settings = ctl_conf.client;
        // use the correct auth method based on what is defined in the config
        match (keys.username, keys.password, keys.token) {
            (Some(user), Some(pass), _) => Ok(self.basic_auth(user, pass)),
            (_, None, Some(token)) => Ok(self.token(token)),
            (_, _, _) => Err(Error::new("Either username/password or token must be set")),
        }
    }

    /// Builds a client with the configured auth settings
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::Thorium;
    /// # use thorium::Error;
    ///
    /// # async fn exec() -> Result<(), Error> {
    /// let thorium = Thorium::build("http://127.0.0.1")
    ///     .basic_auth("user", "password")
    ///     .build()
    ///     .await?;
    /// # // allow test code to be compiled but don't unwrap as no API instance would be up
    /// # Ok(())
    /// # }
    /// # tokio_test::block_on(async {
    /// #    exec().await
    /// # });
    /// ```
    pub async fn build(self) -> Result<Thorium, Error> {
        // build a client
        let client = helpers::build_reqwest_client(&self.settings).await?;
        // get token if we have a username/password and no token
        let (token, expires) = match (self.token, self.username, self.password) {
            // we already have a token, so use the existing one
            (Some(token), _, _) => (token, None),
            // we need to get a new token with basic auth
            (None, Some(username), Some(password)) => {
                Thorium::auth(&self.host, &username, &password, &client).await?
            }
            // username and password were not given and we don't have a token, so error
            _ => {
                return Err(Error::new(
                    "Username and password must be provided if no auth token is set!",
                ));
            }
        };
        // convert our buffer into a Vec<u8> and base64 it
        let encoded = base64::engine::general_purpose::STANDARD.encode(token.as_bytes());
        // build token auth string
        let auth_str = format!("token {encoded}");
        // build every handler with our token
        Ok(Thorium::assemble(
            self.host,
            &auth_str,
            auth_str.clone(),
            expires,
            client,
        ))
    }
}

/// An asynchronous client for Thorium
#[derive(Clone)]
pub struct Thorium {
    /// Handles basic routes in Thorium
    pub basic: Basic,
    /// Handles jobs routes in Thorium
    pub jobs: Jobs,
    /// Handles reactions routes in Thorium
    pub reactions: Reactions,
    /// Handles pipelines routes in Thorium
    pub pipelines: Pipelines,
    /// Handles groups routes in Thorium
    pub groups: Groups,
    /// Handles images routes in Thorium
    pub images: Images,
    /// Handles streams routes in Thorium
    pub streams: Streams,
    /// Handles users routes in Thorium
    pub users: Users,
    /// Handles system routes in Thorium
    pub system: System,
    /// Handles search routes in Thorium
    pub search: Search,
    /// Handles files routes in Thorium
    pub files: Files,
    /// Handles repos routes in Thorium
    pub repos: Repos,
    /// Handles entities routes in Thorium
    pub entities: Entities,
    /// Handles associations routes in Thorium
    pub associations: Associations,
    /// Handles binary update routes in Thorium
    pub updates: Updates,
    /// Handles event routes in Thorium
    pub events: Events,
    /// Handles network policies routes in Thorium
    pub network_policies: NetworkPolicies,
    /// Handles tree routes in Thorium
    pub trees: Trees,
    /// The host/url to reach Thorium at
    pub host: String,
    /// The auth str to use when reverting from a masquerade
    auth_str: String,
    /// When our token expires if we have a token
    pub expires: Option<DateTime<Utc>>,
    // keep a copy of our client for faster masquerades and refreshes
    client: reqwest::Client,
}

// define the synchronous, blocking client
cfg_if::cfg_if! {
    // limit the blocking client to only the subclients that support python
    // if the python feature is active
    if #[cfg(feature = "python")] {
        /// A synchronous client for Thorium
        #[pyclass(from_py_object)]
        #[derive(Clone)]
        pub struct ThoriumBlocking {
            /// Handles basic routes in Thorium
            #[pyo3(get)]
            pub basic: BasicBlocking,
            /// Handles jobs routes in Thorium
            #[pyo3(get)]
            pub jobs: JobsBlocking,
            /// Handles reactions routes in Thorium
            #[pyo3(get)]
            pub reactions: ReactionsBlocking,
            /// Handles reactions routes in Thorium
            #[pyo3(get)]
            pub files: FilesBlocking,
            /// The host/url to reach Thorium at
            pub host: String,
            /// The auth str to use when reverting from a masquerade
            _auth_str: String,
            /// When our token expires if we have a token
            pub expires: Option<DateTime<Utc>>,
            // keep a copy of our client for faster masquerades and refreshes
            _client: reqwest::Client,
        }

        impl ThoriumClientBuilder {
            /// Builds a client with the configured auth settings
            ///
            /// # Examples
            ///
            /// ```
            /// use thorium::ThoriumBlocking;
            /// # use thorium::Error;
            ///
            /// # fn exec() -> Result<(), Error> {
            /// let thorium = ThoriumBlocking::build("http://127.0.0.1")
            ///     .basic_auth("user", "password")
            ///     .build_blocking()?;
            /// # // allow test code to be compiled but don't unwrap as no API instance would be up
            /// # Ok(())
            /// # }
            /// # exec();
            /// ```
            pub fn build_blocking(self) -> Result<ThoriumBlocking, Error> {
                // build a client
                let client = helpers::build_blocking_reqwest_client(&self.settings)?;
                // get token if we have a username/password and no token
                let (token, expires) = match (self.token, self.username, self.password) {
                    // we already have a token, so use the existing one
                    (Some(token), _, _) => (token, None),
                    (None, Some(username), Some(password)) => {
                        ThoriumBlocking::basic_auth(&self.host, &username, &password, &client)?
                    },
                    _ => {
                        return Err(Error::new(
                            "Username and password must be provided if no auth token is set!",
                        ));
                    }
                };
                // convert our buffer into a Vec<u8> and base64 it
                let encoded = base64::engine::general_purpose::STANDARD.encode(token.as_bytes());
                // build token auth string
                let auth_str = format!("token {encoded}");
                // build handlers
                let basic = BasicBlocking::new(&self.host, &client);
                let jobs = JobsBlocking::new(&self.host, &auth_str, &client);
                let reactions = ReactionsBlocking::new(&self.host, &auth_str, &client);
                let files = FilesBlocking::new(&self.host, &auth_str, &client);
                // build Thorium client
                let client = ThoriumBlocking {
                    basic,
                    jobs,
                    reactions,
                    files,
                    host: self.host,
                    _auth_str: auth_str,
                    expires,
                    _client: client,
                };
                Ok(client)
            }
        }
    } else if #[cfg(feature = "sync")] {
        // otherwise provide the whole blocking client

        /// A synchronous client for Thorium
        #[derive(Clone)]
        pub struct ThoriumBlocking {
            /// Handles basic routes in Thorium
            pub basic: BasicBlocking,
            /// Handles jobs routes in Thorium
            pub jobs: JobsBlocking,
            /// Handles reactions routes in Thorium
            pub reactions: ReactionsBlocking,
            /// Handles pipelines routes in Thorium
            pub pipelines: PipelinesBlocking,
            /// Handles groups routes in Thorium
            pub groups: GroupsBlocking,
            /// Handles images routes in Thorium
            pub images: ImagesBlocking,
            /// Handles streams routes in Thorium
            pub streams: StreamsBlocking,
            /// Handles users routes in Thorium
            pub users: UsersBlocking,
            /// Handles system routes in Thorium
            pub system: SystemBlocking,
            /// Handles search routes in Thorium
            pub search: SearchBlocking,
            /// Handles files routes in Thorium
            pub files: FilesBlocking,
            /// Handles repos routes in Thorium
            pub repos: ReposBlocking,
            /// Handles entities routes in Thorium
            pub entities: EntitiesBlocking,
            /// Handles associations routes in Thorium
            pub associations: AssociationsBlocking,
            /// Handles binary update routes in Thorium
            pub updates: UpdatesBlocking,
            /// Handles event routes in Thorium
            pub events: EventsBlocking,
            /// Handles network policies routes in Thorium
            pub network_policies: NetworkPoliciesBlocking,
            /// Handles tree routes in Thorium
            pub trees: TreesBlocking,
            /// The host/url to reach Thorium at
            pub host: String,
            /// The auth str to use when reverting from a masquerade
            _auth_str: String,
            /// When our token expires if we have a token
            pub expires: Option<DateTime<Utc>>,
            // keep a copy of our client for faster masquerades and refreshes
            _client: reqwest::Client,
        }

        impl ThoriumClientBuilder {
            /// Builds a client with the configured auth settings
            ///
            /// # Examples
            ///
            /// ```
            /// use thorium::ThoriumBlocking;
            /// # use thorium::Error;
            ///
            /// # fn exec() -> Result<(), Error> {
            /// let thorium = ThoriumBlocking::build("http://127.0.0.1")
            ///     .basic_auth("user", "password")
            ///     .build_blocking()?;
            /// # // allow test code to be compiled but don't unwrap as no API instance would be up
            /// # Ok(())
            /// # }
            /// # exec();
            /// ```
            pub fn build_blocking(self) -> Result<ThoriumBlocking, Error> {
                // build a client
                let client = helpers::build_blocking_reqwest_client(&self.settings)?;
                // get token if we have a username/password and no token
                let (token, expires) = match (self.token, self.username, self.password) {
                    // we already have a token, so use the existing one
                    (Some(token), _, _) => (token, None),
                    (None, Some(username), Some(password)) => {
                        ThoriumBlocking::basic_auth(&self.host, &username, &password, &client)?
                    },
                    _ => {
                        return Err(Error::new(
                            "Username and password must be provided if no auth token is set!",
                        ));
                    }
                };
                // convert our buffer into a Vec<u8> and base64 it
                let encoded = base64::engine::general_purpose::STANDARD.encode(token.as_bytes());
                // build token auth string
                let auth_str = format!("token {encoded}");
                // build handlers
                let basic = BasicBlocking::new(&self.host, &client);
                let jobs = JobsBlocking::new(&self.host, &auth_str, &client);
                let reactions = ReactionsBlocking::new(&self.host, &auth_str, &client);
                let pipelines = PipelinesBlocking::new(&self.host, &auth_str, &client);
                let groups = GroupsBlocking::new(&self.host, &auth_str, &client);
                let images = ImagesBlocking::new(&self.host, &auth_str, &client);
                let streams = StreamsBlocking::new(&self.host, &auth_str, &client);
                let users = UsersBlocking::new(&self.host, &auth_str, &client);
                let system = SystemBlocking::new(&self.host, &auth_str, &client);
                let search = SearchBlocking::new(&self.host, &auth_str, &client);
                let files = FilesBlocking::new(&self.host, &auth_str, &client);
                let repos = ReposBlocking::new(&self.host, &auth_str, &client);
                let entities = EntitiesBlocking::new(&self.host, &auth_str, &client);
                let associations = AssociationsBlocking::new(&self.host, &auth_str, &client);
                let updates = UpdatesBlocking::new(&self.host, &auth_str, &client);
                let events = EventsBlocking::new(&self.host, &auth_str, &client);
                let network_policies = NetworkPoliciesBlocking::new(&self.host, &auth_str, &client);
                let trees = TreesBlocking::new(&self.host, &auth_str, &client);
                // build Thorium client
                let client = ThoriumBlocking {
                    basic,
                    jobs,
                    reactions,
                    pipelines,
                    groups,
                    images,
                    streams,
                    users,
                    system,
                    search,
                    files,
                    repos,
                    entities,
                    associations,
                    updates,
                    events,
                    network_policies,
                    trees,
                    host: self.host,
                    _auth_str: auth_str,
                    expires,
                    _client: client,
                };
                Ok(client)
            }
        }
    }
}

impl Thorium {
    /// Create a new Thorium client builder
    ///
    /// This can user either username/password or token. When using a token the client will not
    /// known when it expires.
    ///
    /// # Arguments
    ///
    /// * `host` - The host/url/ip the Thorium API can be reached at
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::Thorium;
    /// # use thorium::Error;
    ///
    /// # async fn exec() -> Result<(), Error> {
    /// let thorium = Thorium::build("http://127.0.0.1")
    ///     .basic_auth("user", "password")
    ///     .build()
    ///     .await?;
    /// # // allow test code to be compiled but don't unwrap as no API instance would be up
    /// # Ok(())
    /// # }
    /// # tokio_test::block_on(async {
    /// #    exec().await
    /// # });
    /// ```
    pub fn build<T: Into<String>>(host: T) -> ThoriumClientBuilder {
        ThoriumClientBuilder {
            host: host.into(),
            username: None,
            password: None,
            token: None,
            settings: ClientSettings::default(),
        }
    }

    /// Authenticate using a username/password to Thorium
    ///
    /// # Arguments
    ///
    /// * `host` - The host/url/ip the Thorium API can be reached at
    /// * `username` - The username of the user to login as
    /// * `password` - The password to authenticate with
    /// * `client` - The client to authenticate with
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::Thorium;
    /// # use thorium::Error;
    ///
    /// # async fn exec() -> Result<(), Error> {
    /// let client = reqwest::Client::new();
    /// let (token, expriation) =
    ///     Thorium::auth(
    ///         "http://127.0.0.1",
    ///         "user",
    ///         "pass",
    ///         &client
    ///     ).await?;
    /// # // allow test code to be compiled but don't unwrap as no API instance would be up
    /// # Ok(())
    /// # }
    /// # tokio_test::block_on(async {
    /// #    exec().await
    /// # });
    /// ```
    pub async fn auth(
        host: &str,
        username: &str,
        password: &str,
        client: &reqwest::Client,
    ) -> Result<(String, Option<DateTime<Utc>>), Error> {
        // create auth handler and get token
        match Users::auth_basic(host, username, password, client).await? {
            AuthResponse::Authed { token, expires } => Ok((token, Some(expires))),
            AuthResponse::VerifyEmail(email) => {
                // build an error message telling the user to verify their email
                let msg = format!("Email Verification required at {email}");
                Err(Error::new(msg))
            }
        }
    }

    /// Adds a new admin to Thorium using the secret key
    ///
    /// This is primarily used for bootstrapping a new Thorium install.
    ///
    /// # Arguments
    ///
    /// * `host` - The host/url/ip the Thorium API can be reached at
    /// * `username` - The username of the user to create
    /// * `password` - The password to set for the new user
    /// * `key` - The secret key used to authenticate to the Thorium API
    /// # Examples
    ///
    /// ```
    /// use thorium::Thorium;
    /// use thorium::client::ClientSettings;
    /// # use thorium::Error;
    ///
    /// # async fn exec() -> Result<(), Error> {
    /// let settings = ClientSettings::default();
    /// let mut thorium = Thorium::bootstrap("http://127.0.0.1", "user", "password", "email@email.com", "secret", &settings).await?;
    /// # // allow test code to be compiled but don't unwrap as no API instance would be up
    /// # Ok(())
    /// # }
    /// # tokio_test::block_on(async {
    /// #    exec().await
    /// # });
    /// ```
    pub async fn bootstrap<T: Into<String>>(
        host: &str,
        username: T,
        password: T,
        email: T,
        key: &str,
        settings: &ClientSettings,
    ) -> Result<AuthResponse, Error> {
        // build an admin user create request
        let bp = models::UserCreate::new(username.into(), password.into(), email.into())
            .admin()
            .skip_verification();
        Users::create(host, bp, Some(key), settings).await
    }
}

impl Thorium {
    /// Create a Thorium client from a path on disk
    ///
    /// # Arguments
    ///
    /// * `path` - The path to load out Key from
    pub async fn from_key_file(path: &str) -> Result<Self, Error> {
        // load auth keys
        let keys = Keys::new(path)?;
        // build a Thorium client from keys
        Self::from_keys(keys).await
    }

    /// Create a `Thorium` client from a keys struct
    ///
    /// # Arguments
    ///
    /// * `keys` - The keys to create a client with
    pub async fn from_keys(keys: Keys) -> Result<Self, Error> {
        // create a Thorium client builder
        let builder = Self::build(keys.api);
        // use the correct auth method based on what is defined in the config
        let builder = match (keys.username, keys.password, keys.token) {
            (Some(user), Some(pass), None) => builder.basic_auth(user, pass),
            (_, _, Some(token)) => builder.token(token),
            (_, _, _) => return Err(Error::new("Either username/password or token must be set")),
        };
        // build a new Thorium client
        builder.build().await
    }

    /// Create a Thorium client from a [`CtlConf`] serialized to a file at the given path
    ///
    /// # Arguments
    ///
    /// * `path` - The path to load our `CtlConf` from
    pub async fn from_ctl_conf_file<P: AsRef<Path>>(path: P) -> Result<Self, Error> {
        // load ctl conf
        let ctl_conf = CtlConf::from_path(&path)?;
        // build a client from the ctl conf
        Self::from_ctl_conf(ctl_conf).await
    }

    /// Attempt to create a `Thorium` client from the given [`CtlConf`]
    ///
    /// # Arguments
    ///
    /// * `conf` - The `CtlConf` to create a `Thorium` client from
    pub async fn from_ctl_conf(ctl_conf: CtlConf) -> Result<Self, Error> {
        // create a builder from the ctl conf and build
        Self::build(ctl_conf.keys.api.clone())
            .from_ctl_conf(ctl_conf)?
            .build()
            .await
    }

    /// Refresh this clients token
    ///
    /// This will invalidate the client current token and create a new one causing any other
    /// clients using this token to fail. While masquerading, the client's own token is the one
    /// logged out, the masqueraded user's token stays valid, and every handler moves back to
    /// the new token, ending the masquerade.
    ///
    /// # Arguments
    ///
    /// * `username` - The username of the user to login as
    /// * `password` - The password to authenticate with
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::Thorium;
    /// # use thorium::Error;
    ///
    /// # async fn exec() -> Result<(), Error> {
    /// // get Thorium client
    /// let mut thorium = Thorium::build("http://127.0.0.1")
    ///     .token("token")
    ///     .build()
    ///     .await?;
    /// // refresh our token
    /// thorium.refresh("user", "password").await?;
    /// # // allow test code to be compiled but don't unwrap as no API instance would be up
    /// # Ok(())
    /// # }
    /// # tokio_test::block_on(async {
    /// #    exec().await
    /// # });
    /// ```
    pub async fn refresh<T: AsRef<str>>(&mut self, username: T, password: T) -> Result<(), Error> {
        // logout and invalidate our own token, which is the revert target rather than the token
        // of a user we may be masquerading as
        Users::new(&self.host, &self.auth_str, &self.client)
            .logout()
            .await?;
        // authenticate and get new token
        let (token, expiration) = Self::auth(
            &self.host,
            username.as_ref(),
            password.as_ref(),
            &self.client,
        )
        .await?;
        // update token expiration
        self.expires = expiration;
        // move every handler and our revert target to the new token
        self.set_token(&token);
        Ok(())
    }

    /// Masquerade as a another user
    ///
    /// # Arguments
    ///
    /// * `user` - The user to masquerade as
    pub fn masquerade(&mut self, user: &ScrubbedUser) {
        // convert our buffer into a Vec<u8> and base64 it
        let encoded = base64::engine::general_purpose::STANDARD.encode(user.token.as_bytes());
        // build token auth string
        let auth_str = format!("token {encoded}");
        // rebuild every handler with the user's auth string
        self.rebuild_handlers(&auth_str);
    }

    /// Revert back to our original user from a masquerade
    pub fn revert_masquerade(&mut self) {
        // rebuild every handler with our own auth string
        let auth_str = self.auth_str.clone();
        self.rebuild_handlers(&auth_str);
    }

    /// Use a new token for every handler and as the auth to revert to after a masquerade
    ///
    /// # Arguments
    ///
    /// * `token` - The token to authenticate with
    fn set_token(&mut self, token: &str) {
        // convert our buffer into a Vec<u8> and base64 it
        let encoded = base64::engine::general_purpose::STANDARD.encode(token.as_bytes());
        // keep this token's auth string as the one to revert to
        self.auth_str = format!("token {encoded}");
        // rebuild every handler with the new auth string
        let auth_str = self.auth_str.clone();
        self.rebuild_handlers(&auth_str);
    }

    /// Rebuild every handler with an auth string, keeping the auth to revert to
    ///
    /// # Arguments
    ///
    /// * `handler_auth` - The auth string every handler should send
    fn rebuild_handlers(&mut self, handler_auth: &str) {
        // take our host and revert auth so they carry over into the rebuilt client
        let host = std::mem::take(&mut self.host);
        let auth_str = std::mem::take(&mut self.auth_str);
        // rebuild the client with every handler on the new auth string
        *self = Thorium::assemble(
            host,
            handler_auth,
            auth_str,
            self.expires,
            self.client.clone(),
        );
    }

    /// Build a client with every handler sending the same auth string
    ///
    /// Every way of building or re-authenticating a client goes through here, so the struct
    /// literal below is the one place a new handler has to be added.
    ///
    /// # Arguments
    ///
    /// * `host` - The host/url to reach Thorium at
    /// * `handler_auth` - The auth string every handler should send
    /// * `auth_str` - The auth string to revert to after a masquerade
    /// * `expires` - When our token expires if we know it
    /// * `client` - The reqwest client every handler shares
    fn assemble(
        host: String,
        handler_auth: &str,
        auth_str: String,
        expires: Option<DateTime<Utc>>,
        client: reqwest::Client,
    ) -> Thorium {
        // build every handler and the client that holds them
        Thorium {
            basic: Basic::new(&host, &client),
            jobs: Jobs::new(&host, handler_auth, &client),
            reactions: Reactions::new(&host, handler_auth, &client),
            pipelines: Pipelines::new(&host, handler_auth, &client),
            groups: Groups::new(host.as_str(), handler_auth, &client),
            images: Images::new(&host, handler_auth, &client),
            streams: Streams::new(&host, handler_auth, &client),
            users: Users::new(&host, handler_auth, &client),
            system: System::new(&host, handler_auth, &client),
            search: Search::new(&host, handler_auth, &client),
            files: Files::new(&host, handler_auth, &client),
            repos: Repos::new(&host, handler_auth, &client),
            entities: Entities::new(&host, handler_auth, &client),
            associations: Associations::new(&host, handler_auth, &client),
            updates: Updates::new(&host, handler_auth, &client),
            events: Events::new(&host, handler_auth, &client),
            network_policies: NetworkPolicies::new(&host, handler_auth, &client),
            trees: Trees::new(&host, handler_auth, &client),
            host,
            auth_str,
            expires,
            client,
        }
    }
}

#[cfg(feature = "sync")]
impl ThoriumBlocking {
    /// Create a new Thorium client builder
    ///
    /// This can user either username/password or token. When using a token the client will not
    /// known when it expires.
    ///
    /// # Arguments
    ///
    /// * `host` - The host/url/ip the Thorium API can be reached at
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::ThoriumBlocking;
    /// # use thorium::Error;
    ///
    /// # fn exec() -> Result<(), Error> {
    /// let thorium = ThoriumBlocking::build("http://127.0.0.1")
    ///     .basic_auth("user", "password")
    ///     .build_blocking()?;
    /// # // allow test code to be compiled but don't unwrap as no API instance would be up
    /// # Ok(())
    /// # }
    /// # exec();
    /// ```
    pub fn build<T: Into<String>>(host: T) -> ThoriumClientBuilder {
        ThoriumClientBuilder {
            host: host.into(),
            username: None,
            password: None,
            token: None,
            settings: ClientSettings::default(),
        }
    }

    /// Authenticate using a username/password to Thorium
    ///
    /// # Arguments
    ///
    /// * `host` - The host/url/ip the Thorium API can be reached at
    /// * `username` - The username of the user to login as
    /// * `password` - The password to authenticate with
    /// * `client` - The client to authenticate with
    fn basic_auth(
        host: &str,
        username: &str,
        password: &str,
        client: &reqwest::Client,
    ) -> Result<(String, Option<DateTime<Utc>>), Error> {
        // create auth handler and get token
        match UsersBlocking::auth_basic(host, username, password, client)? {
            AuthResponse::Authed { token, expires } => Ok((token, Some(expires))),
            AuthResponse::VerifyEmail(email) => {
                // build an error message telling the user to verify their email
                let msg = format!("Email Verification required at {email}");
                Err(Error::new(msg))
            }
        }
    }

    /// Create a blocking Thorium client from a path on disk
    ///
    /// # Arguments
    ///
    /// * `path` - The path to read keys from
    pub fn from_key_file(path: &str) -> Result<Self, Error> {
        // load auth keys
        let keys = Keys::new(path)?;
        // build a Thorium client from keys
        Self::from_keys(keys)
    }

    /// Create a blocking Thorium client from a [`Keys`] struct
    ///
    /// # Arguments
    ///
    /// * `keys` - The keys to create a client with
    pub fn from_keys(keys: Keys) -> Result<Self, Error> {
        // create a Thorium client builder
        let builder = Self::build(keys.api);
        // use the correct auth method based on what is defined in the config
        let builder = match (keys.username, keys.password, keys.token) {
            (Some(user), Some(pass), None) => builder.basic_auth(user, pass),
            (_, _, Some(token)) => builder.token(token),
            (_, _, _) => {
                return Err(Error::new(
                    "Either username/password or token must be set in the keys",
                ));
            }
        };
        // build a new Thorium client
        builder.build_blocking()
    }

    /// Create a Thorium client from a [`CtlConf`] serialized to a file at the given path
    ///
    /// # Arguments
    ///
    /// * `path` - The path to load our `CtlConf` from
    pub fn from_ctl_conf_file<P: AsRef<Path>>(path: P) -> Result<Self, Error> {
        // load ctl conf
        let ctl_conf = CtlConf::from_path(&path)?;
        // build a client from the ctl conf
        Self::from_ctl_conf(ctl_conf)
    }

    /// Attempt to create a `Thorium` client from the given [`CtlConf`]
    ///
    /// # Arguments
    ///
    /// * `conf` - The `CtlConf` to create a `Thorium` client from
    pub fn from_ctl_conf(ctl_conf: CtlConf) -> Result<Self, Error> {
        // create a builder from the ctl conf and build
        Self::build(ctl_conf.keys.api.clone())
            .from_ctl_conf(ctl_conf)?
            .build_blocking()
    }
}

#[cfg(test)]
mod masquerade_tests {
    use base64::Engine as _;
    use chrono::Utc;

    use super::Thorium;
    use crate::models::{ScrubbedUser, UserRole, UserSettings};

    /// The token the base client authenticates with
    const BASE_TOKEN: &str = "base-token";

    /// Build the auth string a handler sends for a token
    ///
    /// # Arguments
    ///
    /// * `token` - The token to build an auth string for
    fn auth_for(token: &str) -> String {
        // base64 the token the same way the client does
        let encoded = base64::engine::general_purpose::STANDARD.encode(token.as_bytes());
        format!("token {encoded}")
    }

    /// Build a client authenticated with the base token without contacting an API
    async fn base_client() -> Thorium {
        // a token-authenticated build makes no requests
        Thorium::build("http://127.0.0.1:1")
            .token(BASE_TOKEN)
            .build()
            .await
            .expect("building a token client needs no API")
    }

    /// Build a user to masquerade as
    ///
    /// # Arguments
    ///
    /// * `token` - The token this user authenticates with
    fn user(token: &str) -> ScrubbedUser {
        // build a minimal user carrying the given token
        ScrubbedUser {
            username: "masked".to_owned(),
            role: UserRole::User,
            email: "masked@example.com".to_owned(),
            groups: Vec::default(),
            actual_groups: Vec::default(),
            token: token.to_owned(),
            token_expiration: Utc::now(),
            unix: None,
            settings: UserSettings::default(),
            local: true,
            verified: true,
            has_image: false,
        }
    }

    /// Get the auth string of every handler on a client, by handler name
    ///
    /// The exhaustive destructure stops compiling when a field is added to `Thorium`, so a new
    /// handler can't be left out of this check.
    ///
    /// # Arguments
    ///
    /// * `thorium` - The client to read handler auth strings from
    fn handler_auths(thorium: &Thorium) -> Vec<(&'static str, String)> {
        // name every field so a new one fails to compile here
        let Thorium {
            basic: _,
            jobs,
            reactions,
            pipelines,
            groups,
            images,
            streams,
            users,
            system,
            search,
            files,
            repos,
            entities,
            associations,
            updates,
            events,
            network_policies,
            trees,
            host: _,
            auth_str: _,
            expires: _,
            client: _,
        } = thorium;
        // the basic handler sends no auth so only the authenticated handlers are listed
        vec![
            ("jobs", jobs.auth_token().to_owned()),
            ("reactions", reactions.auth_token().to_owned()),
            ("pipelines", pipelines.auth_token().to_owned()),
            ("groups", groups.auth_token().to_owned()),
            ("images", images.auth_token().to_owned()),
            ("streams", streams.auth_token().to_owned()),
            ("users", users.auth_token().to_owned()),
            ("system", system.auth_token().to_owned()),
            ("search", search.auth_token().to_owned()),
            ("files", files.auth_token().to_owned()),
            ("repos", repos.auth_token().to_owned()),
            ("entities", entities.auth_token().to_owned()),
            ("associations", associations.auth_token().to_owned()),
            ("updates", updates.auth_token().to_owned()),
            ("events", events.auth_token().to_owned()),
            ("network_policies", network_policies.auth_token().to_owned()),
            ("trees", trees.auth_token().to_owned()),
        ]
    }

    /// Assert every handler on a client sends the expected auth string
    ///
    /// # Arguments
    ///
    /// * `thorium` - The client to check
    /// * `expected` - The auth string every handler should send
    /// * `context` - What the client just did, for the failure message
    fn assert_all_handlers(thorium: &Thorium, expected: &str, context: &str) {
        // collect any handler that sends a different auth string
        let wrong = handler_auths(thorium)
            .into_iter()
            .filter(|(_, auth)| auth != expected)
            .map(|(name, _)| name)
            .collect::<Vec<_>>();
        assert!(
            wrong.is_empty(),
            "{context}: handlers {wrong:?} kept another auth string"
        );
    }

    /// After a masquerade every handler uses the user's auth string
    #[tokio::test]
    async fn masquerade_covers_every_handler() {
        // build a base client and masquerade as a user
        let mut thorium = base_client().await;
        thorium.masquerade(&user("user-token"));
        // every handler must now send the user's auth string
        assert_all_handlers(&thorium, &auth_for("user-token"), "after masquerade");
    }

    /// Reverting a masquerade puts every handler back on the original auth string
    #[tokio::test]
    async fn revert_masquerade_restores_every_handler() {
        // build a base client and masquerade as two users in a row
        let mut thorium = base_client().await;
        thorium.masquerade(&user("first-token"));
        thorium.masquerade(&user("second-token"));
        // revert back to the base user
        thorium.revert_masquerade();
        // every handler must be back on the base auth string
        assert_all_handlers(&thorium, &auth_for(BASE_TOKEN), "after revert_masquerade");
    }

    /// Adopting a refreshed token moves every handler and the revert target to it
    #[tokio::test]
    async fn refreshed_token_covers_every_handler() {
        // build a base client that is masquerading as a user
        let mut thorium = base_client().await;
        thorium.masquerade(&user("user-token"));
        // adopt a refreshed token the way refresh does after authenticating
        thorium.set_token("refreshed-token");
        // every handler must send the refreshed auth string
        assert_all_handlers(&thorium, &auth_for("refreshed-token"), "after refresh");
        // reverting must return to the refreshed token rather than the stale one
        thorium.revert_masquerade();
        assert_all_handlers(
            &thorium,
            &auth_for("refreshed-token"),
            "after refresh and revert",
        );
    }

    /// The requests a mock server received, as (request line, authorization header) pairs
    type Recorded = std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>;

    /// Start a mock Thorium server that records each request's line and authorization header,
    /// answers `/api/users/auth` with a token for `refreshed-token`, and anything else with an
    /// empty `200`
    ///
    /// Returns the server's host and the requests it has received
    async fn auth_server() -> (String, Recorded) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // bind to a random local port
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // track the requests we receive
        let recorded: Recorded = std::sync::Arc::default();
        let inner = recorded.clone();
        // answer requests in the background
        tokio::spawn(async move {
            loop {
                // accept the next connection
                let (mut stream, _) = listener.accept().await.unwrap();
                // read until the end of the request head; these requests have no body
                let mut buf = Vec::new();
                let mut chunk = [0_u8; 4096];
                while !String::from_utf8_lossy(&buf).contains("\r\n\r\n") {
                    // read the next chunk of this request and stop if the client closed it
                    let read = stream.read(&mut chunk).await.unwrap();
                    if read == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..read]);
                }
                // record the request line and its authorization header
                let text = String::from_utf8_lossy(&buf).to_string();
                let line = text.lines().next().unwrap_or_default().to_owned();
                let auth = text
                    .lines()
                    .find_map(|header| {
                        header
                            .split_once(':')
                            .filter(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                            .map(|(_, value)| value.trim().to_owned())
                    })
                    .unwrap_or_default();
                inner.lock().unwrap().push((line.clone(), auth));
                // a login returns a new token and anything else returns nothing
                let body = if line.contains("/api/users/auth") {
                    r#"{"Authed":{"token":"refreshed-token","expires":"2099-01-01T00:00:00Z"}}"#
                } else {
                    ""
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                // answer and close the connection
                stream.write_all(resp.as_bytes()).await.unwrap();
                stream.shutdown().await.unwrap();
            }
        });
        // return the server's address and the requests it records
        (format!("http://{addr}"), recorded)
    }

    /// Get the authorization header the mock server saw on each logout
    ///
    /// # Arguments
    ///
    /// * `recorded` - The requests the mock server recorded
    fn logout_auths(recorded: &Recorded) -> Vec<String> {
        // keep only the logout requests
        recorded
            .lock()
            .unwrap()
            .iter()
            .filter(|(line, _)| line.contains("/api/users/logout"))
            .map(|(_, auth)| auth.clone())
            .collect()
    }

    /// `refresh` logs out the client's own token, then moves every handler and the
    /// revert target to the new token
    #[tokio::test]
    async fn refresh_logs_out_its_own_token() {
        // build a client for a mock server and refresh it
        let (host, recorded) = auth_server().await;
        let mut thorium = Thorium::build(&host)
            .token(BASE_TOKEN)
            .build()
            .await
            .unwrap();
        thorium.refresh("admin", "password").await.unwrap();
        // the one logout went out with the base token
        assert_eq!(logout_auths(&recorded), vec![auth_for(BASE_TOKEN)]);
        // every handler is on the new token
        assert_all_handlers(&thorium, &auth_for("refreshed-token"), "after refresh");
    }

    /// `refresh` while masquerading logs out the client's own token, not the
    /// masqueraded user's, and ends the masquerade on the new token
    #[tokio::test]
    async fn refresh_while_masquerading_keeps_the_user_token() {
        // build a client for a mock server and masquerade as a user
        let (host, recorded) = auth_server().await;
        let mut thorium = Thorium::build(&host)
            .token(BASE_TOKEN)
            .build()
            .await
            .unwrap();
        thorium.masquerade(&user("user-token"));
        // refresh our own token
        thorium.refresh("admin", "password").await.unwrap();
        // the logout went out with the base token, never the masqueraded user's
        assert_eq!(logout_auths(&recorded), vec![auth_for(BASE_TOKEN)]);
        // every handler is on the new token, and reverting stays there
        assert_all_handlers(&thorium, &auth_for("refreshed-token"), "after refresh");
        thorium.revert_masquerade();
        assert_all_handlers(
            &thorium,
            &auth_for("refreshed-token"),
            "after refresh and revert",
        );
        // a second refresh logs out the refreshed token
        thorium.refresh("admin", "password").await.unwrap();
        assert_eq!(
            logout_auths(&recorded),
            vec![auth_for(BASE_TOKEN), auth_for("refreshed-token")]
        );
    }
}
