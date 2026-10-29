//! The client for entities in Thorium

#[cfg(feature = "trace")]
use tracing::instrument;
use uuid::Uuid;

use super::Error;
use super::traits::{GenericClient, GrepClient, GrepClientHelper, NotificationsClient};
use crate::models::backends::NotificationSupport;
use crate::models::{
    Association, Cursor, Entity, EntityKinds, EntityListLine, EntityListOpts, EntityRequest,
    EntityResponse, EntityUpdate, GrepMatches, GrepOpts, GrepPage, HuntCheckpoint,
    HuntCheckpointResponse, HuntCreateRequest, HuntRuleLine, HuntRuleListOpts, HuntStatusOpts,
    HuntSystemStatus, InstancedAgentToolset, ModelCredentialEntity, Notification,
    NotificationParams, NotificationRequest, TagDeleteRequest, TagRequest, ToolInstanceSummary,
    YaraHitWriteRequest, YaraHitWriteResponse,
};
use crate::{
    add_date, add_query, add_query_bool, add_query_list, add_query_list_clone, send, send_build,
};

// sync client imports
cfg_if::cfg_if! {
    if #[cfg(feature = "sync")] {
        use super::RUNTIME;
        use super::traits::GrepClientBlocking;
    }
}

/// A handler for the entities routes in Thorium
#[cfg_attr(feature = "sync", thorium_derive::blocking_struct)]
#[derive(Clone)]
pub struct Entities {
    /// The host/url that Thorium can be reached at
    host: String,
    /// token to use for auth
    token: String,
    /// A reqwest client for reqwests
    client: reqwest::Client,
}

#[cfg(test)]
mod tests;

#[cfg(test)]
impl Entities {
    /// The auth string this handler sends with its requests
    pub(crate) fn auth_token(&self) -> &str {
        &self.token
    }
}

#[cfg_attr(feature = "sync", thorium_derive::blocking_struct)]
impl Entities {
    /// Creates a new entities handler
    ///
    /// Instead of directly creating this handler you likely want to simply create a
    /// `thorium::Thorium` and use the handler within that instead.
    ///
    /// # Arguments
    ///
    /// * `host` - url/ip of the Thorium api
    /// * `token` - The token used for authentication
    /// * `client` - The reqwest client to use
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::client::Entities;
    ///
    /// let client = reqwest::Client::new();
    /// let entities = Entities::new("http://127.0.0.1", "token", &client);
    /// ```
    #[must_use]
    pub fn new(host: &str, token: &str, client: &reqwest::Client) -> Self {
        // build basic route handler
        Entities {
            host: host.to_owned(),
            token: token.to_owned(),
            client: client.clone(),
        }
    }

    /// Creates an [`Entity`] in Thorium
    ///
    /// # Arguments
    ///
    /// * `entity_req` - The entity request to use to add an entity to Thorium
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::Thorium;
    /// # use thorium::Error;
    /// use thorium::models::{EntityRequest, EntityMetadataRequest};
    ///
    /// # async fn exec() -> Result<(), Error> {
    /// // create a Thorium client
    /// let thorium = Thorium::build("http://127.0.0.1").token("<token>").build().await?;
    /// // have metadata for the entity to create
    /// let meta = EntityMetadataRequest::Other;
    /// // build the entity request
    /// let entity_req = EntityRequest::new("sponge", meta, vec!("bob".to_owned()));
    /// // try to create an entity in Thorium
    /// thorium.entities.create(entity_req).await?;
    /// # // allow test code to be compiled but don't unwrap as no API instance would be up
    /// # Ok(())
    /// # }
    /// # tokio_test::block_on(async {
    /// #    exec().await
    /// # });
    /// ```
    #[cfg_attr(
        feature = "trace",
        instrument(name = "Thorium::Entities::create", skip_all, err(Debug))
    )]
    pub async fn create(&self, entity_req: EntityRequest) -> Result<EntityResponse, Error> {
        // build url for claiming a job
        let url = format!("{base}/api/entities/", base = self.host);
        // build request
        let req = self
            .client
            .post(&url)
            .multipart(entity_req.to_form()?)
            .header("authorization", &self.token);
        // send this request
        send_build!(self.client, req, EntityResponse)
    }

    /// Writes a yara rule hit through the server side hit write route
    ///
    /// The server recomputes the hits write groups from authoritative state
    /// and performs the whole partitioned write (entity, associations, depth
    /// stamped tags, and auto flag actions) in one audited idempotent call,
    /// reporting the per cause split of the requested event groups back. A
    /// request carrying a hunt id additionally decorates the written hit with
    /// a `HuntHit` association and a `HuntId` entity tag.
    ///
    /// # Arguments
    ///
    /// * `hit_req` - The yara hit write request to send
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::Thorium;
    /// use thorium::models::YaraHitWriteRequest;
    /// use chrono::Utc;
    /// use uuid::Uuid;
    /// # use thorium::Error;
    ///
    /// # async fn exec() -> Result<(), Error> {
    /// // create a Thorium client
    /// let thorium = Thorium::build("http://127.0.0.1").token("<token>").build().await?;
    /// // build the hit write request (use a real rule id and sha256 though)
    /// let hit_req = YaraHitWriteRequest {
    ///     rule_id: Uuid::new_v4(),
    ///     sha256: "aaaa".repeat(16),
    ///     rule_hash: "feed".repeat(16),
    ///     ruleset_hash: "beef".repeat(16),
    ///     engine_version: "yara-x 1.0.0".to_owned(),
    ///     scanned_at: Utc::now(),
    ///     event_groups: vec!["corn".to_owned()],
    ///     event_depth: 0,
    ///     matched: Vec::default(),
    ///     hunt: None,
    /// };
    /// // write this hit through the server side route
    /// thorium.entities.create_yara_hit(&hit_req).await?;
    /// # // allow test code to be compiled but don't unwrap as no API instance would be up
    /// # Ok(())
    /// # }
    /// # tokio_test::block_on(async {
    /// #    exec().await
    /// # });
    /// ```
    #[cfg_attr(
        feature = "trace",
        instrument(name = "Thorium::Entities::create_yara_hit", skip_all, err(Debug))
    )]
    pub async fn create_yara_hit(
        &self,
        hit_req: &YaraHitWriteRequest,
    ) -> Result<YaraHitWriteResponse, Error> {
        // build url for writing a yara hit
        let url = format!("{base}/api/entities/yara/hit/", base = self.host);
        // build request
        let req = self
            .client
            .post(&url)
            .json(hit_req)
            .header("authorization", &self.token);
        // send this request
        send_build!(self.client, req, YaraHitWriteResponse)
    }

    /// Creates a hunt in Thorium
    ///
    /// The server resolves and freezes the hunt's rule selector under the
    /// creator's visibility, splits its corpus into lanes, and starts the lane
    /// chunk chains; the returned entity carries the frozen snapshot and the
    /// exclusion counters.
    ///
    /// # Arguments
    ///
    /// * `hunt_req` - The hunt create request to send
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::Thorium;
    /// use thorium::models::{HuntCreateRequest, HuntEngine, HuntSelector};
    /// # use thorium::Error;
    /// # use uuid::Uuid;
    ///
    /// # async fn exec() -> Result<(), Error> {
    /// // create Thorium client
    /// let thorium = Thorium::build("http://127.0.0.1").token("<token>").build().await?;
    /// // build a request for a hunt over one explicitly chosen rule
    /// let hunt_req = HuntCreateRequest {
    ///     name: "corn-hunt".to_owned(),
    ///     groups: vec!["corn".to_owned()],
    ///     engine: HuntEngine::Yara,
    ///     selector: HuntSelector::Rules(vec![Uuid::new_v4()]),
    ///     corpus: None,
    ///     description: Some("Hunt historical corn".to_owned()),
    /// };
    /// // create this hunt
    /// thorium.entities.create_hunt(&hunt_req).await?;
    /// # // allow test code to be compiled but don't unwrap as no API instance would be up
    /// # Ok(())
    /// # }
    /// # tokio_test::block_on(async {
    /// #    exec().await
    /// # });
    /// ```
    #[cfg_attr(
        feature = "trace",
        instrument(name = "Thorium::Entities::create_hunt", skip_all, err(Debug))
    )]
    pub async fn create_hunt(&self, hunt_req: &HuntCreateRequest) -> Result<Entity, Error> {
        // build url for creating a hunt
        let url = format!("{base}/api/entities/hunt/", base = self.host);
        // build request
        let req = self
            .client
            .post(&url)
            .json(hunt_req)
            .header("authorization", &self.token);
        // send this request and build the created hunt entity
        send_build!(self.client, req, Entity)
    }

    /// Cancels a hunt in Thorium
    ///
    /// Only the hunt's submitter or an admin may cancel it; running chunks
    /// observe the cancel lazily within one chunk.
    ///
    /// # Arguments
    ///
    /// * `id` - The uuid of the hunt to cancel
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::Thorium;
    /// # use thorium::Error;
    /// # use uuid::Uuid;
    ///
    /// # async fn exec() -> Result<(), Error> {
    /// // create Thorium client
    /// let thorium = Thorium::build("http://127.0.0.1").token("<token>").build().await?;
    /// // cancel this hunt by id (use a real hunt id though)
    /// thorium.entities.cancel_hunt(Uuid::new_v4()).await?;
    /// # // allow test code to be compiled but don't unwrap as no API instance would be up
    /// # Ok(())
    /// # }
    /// # tokio_test::block_on(async {
    /// #    exec().await
    /// # });
    /// ```
    #[cfg_attr(
        feature = "trace",
        instrument(name = "Thorium::Entities::cancel_hunt", skip(self), err(Debug))
    )]
    pub async fn cancel_hunt(&self, id: Uuid) -> Result<reqwest::Response, Error> {
        // build url for cancelling a hunt
        let url = format!("{base}/api/entities/hunt/{id}/cancel", base = self.host);
        // build request
        let req = self.client.post(&url).header("authorization", &self.token);
        // send this request
        send!(self.client, req)
    }

    /// Lists one page of a hunt's frozen rule snapshot in Thorium
    ///
    /// Rules that were deleted after the freeze, or that the caller cannot
    /// see, come back as id only lines. The cursor never expires since the
    /// frozen snapshot is immutable.
    ///
    /// # Arguments
    ///
    /// * `id` - The uuid of the hunt to list frozen rules for
    /// * `opts` - The options for this frozen rule listing
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::Thorium;
    /// use thorium::models::HuntRuleListOpts;
    /// # use thorium::Error;
    /// # use uuid::Uuid;
    ///
    /// # async fn exec() -> Result<(), Error> {
    /// // create Thorium client
    /// let thorium = Thorium::build("http://127.0.0.1").token("<token>").build().await?;
    /// // list this hunts frozen rules (use a real hunt id though)
    /// let opts = HuntRuleListOpts::default().page_size(100);
    /// let cursor = thorium.entities.list_hunt_rules(Uuid::new_v4(), &opts).await?;
    /// # // allow test code to be compiled but don't unwrap as no API instance would be up
    /// # Ok(())
    /// # }
    /// # tokio_test::block_on(async {
    /// #    exec().await
    /// # });
    /// ```
    #[cfg_attr(
        feature = "trace",
        instrument(
            name = "Thorium::Entities::list_hunt_rules",
            skip(self, opts),
            err(Debug)
        )
    )]
    pub async fn list_hunt_rules(
        &self,
        id: Uuid,
        opts: &HuntRuleListOpts,
    ) -> Result<Cursor<HuntRuleLine>, Error> {
        // build url for listing a hunts frozen rules
        let url = format!("{base}/api/entities/hunt/{id}/rules", base = self.host);
        // get the correct page size if our limit is smaller then our page_size
        let page_size = opts.limit.map_or_else(
            || opts.page_size,
            |limit| std::cmp::min(opts.page_size, limit),
        );
        // build our query params
        let mut query = vec![("limit".to_owned(), page_size.to_string())];
        add_query!(query, "cursor".to_owned(), opts.cursor);
        // get the data for this request and create our cursor
        Cursor::new(
            &url,
            opts.page_size,
            opts.limit,
            &self.token,
            &query,
            &self.client,
        )
        .await
    }

    /// Get the admin hunt census for the status page
    ///
    /// Admin only: the census carries cross-group hunt names, submitters, and
    /// per-user usage.
    ///
    /// # Arguments
    ///
    /// * `opts` - The options bounding this census read
    #[cfg_attr(
        feature = "trace",
        instrument(name = "Thorium::Entities::hunt_system_status", skip(self), err(Debug))
    )]
    pub async fn hunt_system_status(
        &self,
        opts: &HuntStatusOpts,
    ) -> Result<HuntSystemStatus, Error> {
        // build url for reading the admin hunt census
        let url = format!("{base}/api/entities/hunt/status/", base = self.host);
        // build our query params
        let mut query: Vec<(String, String)> = Vec::with_capacity(2);
        add_query!(query, "recent".to_owned(), opts.recent);
        add_query!(query, "window_days".to_owned(), opts.window_days);
        // build our request
        let req = self
            .client
            .get(&url)
            .query(&query)
            .header("authorization", &self.token);
        // send this request and build the census from the response
        send_build!(self.client, req, HuntSystemStatus)
    }

    /// Exempts a yara rule from noisy/slow quarantine in Thorium
    ///
    /// Only admins may manage exemptions. Setting an exemption also deletes
    /// any Noisy/Slow quarantine status so the rule rejoins scanning in the
    /// same action.
    ///
    /// # Arguments
    ///
    /// * `id` - The uuid of the yara rule to exempt from quarantine
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::Thorium;
    /// # use thorium::Error;
    /// # use uuid::Uuid;
    ///
    /// # async fn exec() -> Result<(), Error> {
    /// // create Thorium client
    /// let thorium = Thorium::build("http://127.0.0.1").token("<token>").build().await?;
    /// // exempt this rule from quarantine (use a real rule id though)
    /// thorium.entities.set_quarantine_exemption(Uuid::new_v4()).await?;
    /// # // allow test code to be compiled but don't unwrap as no API instance would be up
    /// # Ok(())
    /// # }
    /// # tokio_test::block_on(async {
    /// #    exec().await
    /// # });
    /// ```
    #[cfg_attr(
        feature = "trace",
        instrument(
            name = "Thorium::Entities::set_quarantine_exemption",
            skip(self),
            err(Debug)
        )
    )]
    pub async fn set_quarantine_exemption(&self, id: Uuid) -> Result<reqwest::Response, Error> {
        // build url for exempting a yara rule from quarantine
        let url = format!(
            "{base}/api/entities/rules/{id}/quarantine-exemption",
            base = self.host
        );
        // build request
        let req = self.client.post(&url).header("authorization", &self.token);
        // send this request
        send!(self.client, req)
    }

    /// Clears a yara rules quarantine exemption in Thorium
    ///
    /// Only admins may manage exemptions. Clearing an exemption only removes
    /// the rule's protection from future quarantine; it never adds or removes
    /// quarantine status values.
    ///
    /// # Arguments
    ///
    /// * `id` - The uuid of the yara rule to clear the exemption from
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::Thorium;
    /// # use thorium::Error;
    /// # use uuid::Uuid;
    ///
    /// # async fn exec() -> Result<(), Error> {
    /// // create Thorium client
    /// let thorium = Thorium::build("http://127.0.0.1").token("<token>").build().await?;
    /// // clear this rules exemption (use a real rule id though)
    /// thorium.entities.clear_quarantine_exemption(Uuid::new_v4()).await?;
    /// # // allow test code to be compiled but don't unwrap as no API instance would be up
    /// # Ok(())
    /// # }
    /// # tokio_test::block_on(async {
    /// #    exec().await
    /// # });
    /// ```
    #[cfg_attr(
        feature = "trace",
        instrument(
            name = "Thorium::Entities::clear_quarantine_exemption",
            skip(self),
            err(Debug)
        )
    )]
    pub async fn clear_quarantine_exemption(&self, id: Uuid) -> Result<reqwest::Response, Error> {
        // build url for clearing a yara rules quarantine exemption
        let url = format!(
            "{base}/api/entities/rules/{id}/quarantine-exemption",
            base = self.host
        );
        // build request
        let req = self
            .client
            .delete(&url)
            .header("authorization", &self.token);
        // send this request
        send!(self.client, req)
    }

    /// Commits one processed hunt chunk in Thorium
    ///
    /// Only the admin service account's hunt workers call this. A 409 means
    /// this checkpoint lost its sequence compare-and-set to a duplicate
    /// delivery and the caller should drop its stale chain.
    ///
    /// # Arguments
    ///
    /// * `id` - The uuid of the hunt this checkpoint advances
    /// * `checkpoint` - The checkpoint to commit
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::Thorium;
    /// use thorium::models::{HuntCheckpoint, HuntCounters};
    /// # use thorium::Error;
    /// # use uuid::Uuid;
    ///
    /// # async fn exec() -> Result<(), Error> {
    /// // create Thorium client
    /// let thorium = Thorium::build("http://127.0.0.1").token("<token>").build().await?;
    /// // build the commit for one processed chunk of lane 0
    /// let checkpoint = HuntCheckpoint {
    ///     lane: 0,
    ///     seq: 0,
    ///     current: chrono::Utc::now(),
    ///     cursor: None,
    ///     exhausted: false,
    ///     failed: None,
    ///     counters: HuntCounters::default(),
    /// };
    /// // commit this checkpoint (use a real hunt id though)
    /// thorium.entities.checkpoint_hunt(Uuid::new_v4(), &checkpoint).await?;
    /// # // allow test code to be compiled but don't unwrap as no API instance would be up
    /// # Ok(())
    /// # }
    /// # tokio_test::block_on(async {
    /// #    exec().await
    /// # });
    /// ```
    #[cfg_attr(
        feature = "trace",
        instrument(
            name = "Thorium::Entities::checkpoint_hunt",
            skip(self, checkpoint),
            err(Debug)
        )
    )]
    pub async fn checkpoint_hunt(
        &self,
        id: Uuid,
        checkpoint: &HuntCheckpoint,
    ) -> Result<HuntCheckpointResponse, Error> {
        // build url for checkpointing a hunt
        let url = format!("{base}/api/entities/hunt/{id}/checkpoint", base = self.host);
        // build request
        let req = self
            .client
            .post(&url)
            .json(checkpoint)
            .header("authorization", &self.token);
        // send this request and build the checkpoint outcome
        send_build!(self.client, req, HuntCheckpointResponse)
    }

    /// Gets details about a specific [`Entity`] in Thorium
    ///
    /// # Arguments
    ///
    /// * `id` - The uuid of the entity to get details on
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::Thorium;
    /// # use thorium::Error;
    /// # use uuid::Uuid;
    ///
    /// # async fn exec() -> Result<(), Error> {
    /// // create Thorium client
    /// let thorium = Thorium::build("http://127.0.0.1").token("<token>").build().await?;
    /// // get details on this entity by id (use a real entity id though)
    /// thorium.entities.get(Uuid::new_v4()).await?;
    /// # // allow test code to be compiled but don't unwrap as no API instance would be up
    /// # Ok(())
    /// # }
    /// # tokio_test::block_on(async {
    /// #    exec().await
    /// # });
    /// ```
    #[cfg_attr(
        feature = "trace",
        instrument(name = "Thorium::Entities::get", skip(self), err(Debug))
    )]
    pub async fn get(&self, id: Uuid) -> Result<Entity, Error> {
        // build url for getting info on a sample
        let url = format!("{base}/api/entities/{id}", base = self.host);
        // build request
        let req = self.client.get(&url).header("authorization", &self.token);
        // send this request and build a sample from the response
        send_build!(self.client, req, Entity)
    }

    /// Lists every association of an [`Entity`] in both directions
    ///
    /// Only admins can list an entity's associations this way; this is meant for tests.
    ///
    /// # Arguments
    ///
    /// * `id` - The id of the entity to list associations for
    #[cfg_attr(
        feature = "trace",
        instrument(name = "Thorium::Entities::associations", skip(self), err(Debug))
    )]
    pub async fn associations(&self, id: Uuid) -> Result<Vec<Association>, Error> {
        // build the url for listing this entity's associations
        let url = format!("{base}/api/entities/{id}/associations", base = self.host);
        // build our request
        let req = self.client.get(&url).header("authorization", &self.token);
        // send this request and build the associations from the response
        send_build!(self.client, req, Vec<Association>)
    }

    /// Lists the ban notifications of a model entity
    ///
    /// The type parameter is the metadata struct of the entity's kind (for example
    /// [`ModelEndpointEntity`](crate::models::ModelEndpointEntity)); entities of other kinds
    /// have none.
    ///
    /// # Arguments
    ///
    /// * `id` - The id of the entity to list notifications for
    #[cfg_attr(
        feature = "trace",
        instrument(name = "Thorium::Entities::notifications", skip(self), err(Debug))
    )]
    pub async fn notifications<N: NotificationSupport>(
        &self,
        id: Uuid,
    ) -> Result<Vec<Notification<N>>, Error> {
        // build the url for listing this entity's notifications
        let url = format!("{base}/api/entities/{id}/notifications", base = self.host);
        // build our request
        let req = self.client.get(&url).header("authorization", &self.token);
        // send this request and build the notifications from the response
        send_build!(self.client, req, Vec<Notification<N>>)
    }

    /// List a tool's instances, newest first, including deprecated instances
    ///
    /// # Arguments
    ///
    /// * `tool` - The id of the tool to list instances of
    #[cfg_attr(
        feature = "trace",
        instrument(name = "Thorium::Entities::tool_instances", skip(self), err(Debug))
    )]
    pub async fn tool_instances(&self, tool: Uuid) -> Result<Vec<ToolInstanceSummary>, Error> {
        // build the url for listing this tool's instances
        let url = format!(
            "{base}/api/entities/tools/{tool}/instances",
            base = self.host
        );
        // build our request
        let req = self.client.get(&url).header("authorization", &self.token);
        // send this request and build the summaries from the response
        send_build!(self.client, req, Vec<ToolInstanceSummary>)
    }

    /// Ask for a new instance of a workflow, which the event handler builds
    ///
    /// # Arguments
    ///
    /// * `workflow` - The id of the workflow to instance
    #[cfg_attr(
        feature = "trace",
        instrument(name = "Thorium::Entities::request_instance", skip(self), err(Debug))
    )]
    pub async fn request_instance(&self, workflow: Uuid) -> Result<reqwest::Response, Error> {
        // build the url for requesting an instance of this workflow
        let url = format!(
            "{base}/api/entities/workflows/{workflow}/instance",
            base = self.host
        );
        // build our request
        let req = self.client.post(&url).header("authorization", &self.token);
        // send this request
        send!(self.client, req)
    }

    /// Ask for a new instance of a toolset, which the event handler builds
    ///
    /// # Arguments
    ///
    /// * `tool_set` - The id of the toolset to instance
    #[cfg_attr(
        feature = "trace",
        instrument(
            name = "Thorium::Entities::request_tool_set_instance",
            skip(self),
            err(Debug)
        )
    )]
    pub async fn request_tool_set_instance(
        &self,
        tool_set: Uuid,
    ) -> Result<reqwest::Response, Error> {
        // build the url for requesting an instance of this toolset
        let url = format!(
            "{base}/api/entities/toolsets/{tool_set}/instance",
            base = self.host
        );
        // build our request
        let req = self.client.post(&url).header("authorization", &self.token);
        // send this request
        send!(self.client, req)
    }

    /// Ask for a refresh of a model endpoint's catalog, which the event handler lists
    ///
    /// Only owners and managers of one of the endpoint's groups, and admins, may ask.
    ///
    /// # Arguments
    ///
    /// * `endpoint` - The id of the model endpoint to refresh
    #[cfg_attr(
        feature = "trace",
        instrument(
            name = "Thorium::Entities::refresh_model_endpoint",
            skip(self),
            err(Debug)
        )
    )]
    pub async fn refresh_model_endpoint(&self, endpoint: Uuid) -> Result<reqwest::Response, Error> {
        // build the url for refreshing this endpoint
        let url = format!(
            "{base}/api/entities/models/endpoints/{endpoint}/refresh",
            base = self.host
        );
        // build our request
        let req = self.client.post(&url).header("authorization", &self.token);
        // send this request
        send!(self.client, req)
    }

    /// Read a group model credential with its secrets, which only admins may do
    ///
    /// # Arguments
    ///
    /// * `credential` - The id of the model credential to read
    #[cfg_attr(
        feature = "trace",
        instrument(
            name = "Thorium::Entities::model_credential_secret",
            skip(self),
            err(Debug)
        )
    )]
    pub async fn model_credential_secret(
        &self,
        credential: Uuid,
    ) -> Result<ModelCredentialEntity, Error> {
        // build the url for reading this credential's secrets
        let url = format!(
            "{base}/api/entities/models/credentials/{credential}/secret",
            base = self.host
        );
        // build our request
        let req = self.client.get(&url).header("authorization", &self.token);
        // send this request and build the credential from the response
        send_build!(self.client, req, ModelCredentialEntity)
    }

    /// Get one agent of a workflow instance and the stored spec of each tool it was given
    ///
    /// # Arguments
    ///
    /// * `workflow_instance` - The id of the workflow instance
    /// * `agent` - The id of the agent in that instance
    #[cfg_attr(
        feature = "trace",
        instrument(name = "Thorium::Entities::toolset", skip(self), err(Debug))
    )]
    pub async fn toolset(
        &self,
        workflow_instance: Uuid,
        agent: Uuid,
    ) -> Result<InstancedAgentToolset, Error> {
        // build the url for reading this agent's toolset
        let url = format!(
            "{base}/api/entities/workflows/instances/{workflow_instance}/toolset",
            base = self.host
        );
        // build our request naming the agent
        let req = self
            .client
            .get(&url)
            .header("authorization", &self.token)
            .query(&[("agent", agent.to_string())]);
        // send this request and build the toolset from the response
        send_build!(self.client, req, InstancedAgentToolset)
    }

    /// Gets every [`Entity`] sharing an exact name that this user can see
    ///
    /// Matches come back ordered oldest-created first and an unknown name is
    /// just an empty list rather than an error, so callers can distinguish the
    /// zero, one, and many match cases themselves.
    ///
    /// # Arguments
    ///
    /// * `name` - The exact name of the entities to get
    /// * `kinds` - The entity kinds to include (an empty list means all kinds)
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::Thorium;
    /// use thorium::models::EntityKinds;
    /// # use thorium::Error;
    ///
    /// # async fn exec() -> Result<(), Error> {
    /// // create Thorium client
    /// let thorium = Thorium::build("http://127.0.0.1").token("<token>").build().await?;
    /// // get every yara rule named corn (use a real rule name though)
    /// thorium.entities.get_by_name("corn", &[EntityKinds::YaraRule]).await?;
    /// # // allow test code to be compiled but don't unwrap as no API instance would be up
    /// # Ok(())
    /// # }
    /// # tokio_test::block_on(async {
    /// #    exec().await
    /// # });
    /// ```
    #[cfg_attr(
        feature = "trace",
        instrument(name = "Thorium::Entities::get_by_name", skip(self), err(Debug))
    )]
    pub async fn get_by_name(
        &self,
        name: &str,
        kinds: &[EntityKinds],
    ) -> Result<Vec<Entity>, Error> {
        // build url for getting entities by their exact name
        let url = format!("{base}/api/entities/by-name/", base = self.host);
        // build our query params starting with the name to look up
        let mut query = vec![("name".to_owned(), name.to_owned())];
        // add the kinds to include in this lookup
        add_query_list!(query, "kinds[]".to_owned(), kinds);
        // build request
        let req = self
            .client
            .get(&url)
            .header("authorization", &self.token)
            .query(&query);
        // send this request and build a list of entities from the response
        send_build!(self.client, req, Vec<Entity>)
    }

    /// Updates an [`Entity`] in Thorium
    ///
    /// # Arguments
    ///
    /// * `id` - The id of the entity to update
    /// * `update` - The update to apply to an entity
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::Thorium;
    /// # use thorium::Error;
    /// use thorium::models::EntityUpdate;
    /// use uuid::Uuid;
    ///
    /// # async fn exec() -> Result<(), Error> {
    /// // update Thorium client
    /// let thorium = Thorium::build("http://127.0.0.1").token("<token>").build().await?;
    /// // build the entity update
    /// let update = EntityUpdate::default().group("woot");
    /// // try to update an entity in Thorium
    /// thorium.entities.update(Uuid::new_v4(), update).await?;
    /// # // allow test code to be compiled but don't unwrap as no API instance would be up
    /// # Ok(())
    /// # }
    /// # tokio_test::block_on(async {
    /// #    exec().await
    /// # });
    /// ```
    #[cfg_attr(
        feature = "trace",
        instrument(name = "Thorium::Entities::update", skip_all, err(Debug))
    )]
    pub async fn update(&self, id: Uuid, update: EntityUpdate) -> Result<reqwest::Response, Error> {
        // build url for claiming a job
        let url = format!("{base}/api/entities/{id}", base = self.host);
        // build request
        let req = self
            .client
            .patch(&url)
            .multipart(update.to_form()?)
            .header("authorization", &self.token);
        // send this request
        send!(self.client, req)
    }

    /// Deletes an [`Entity`] in Thorium
    ///
    /// # Arguments
    ///
    /// * `id` - The id of the entity to delete
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::Thorium;
    /// # use thorium::Error;
    /// use uuid::Uuid;
    ///
    /// # async fn exec() -> Result<(), Error> {
    /// // create a Thorium client
    /// let thorium = Thorium::build("http://127.0.0.1").token("<token>").build().await?;
    /// // try to delete an entity in Thorium (use a real entity id though)
    /// thorium.entities.delete(Uuid::new_v4()).await?;
    /// # // allow test code to be compiled but don't unwrap as no API instance would be up
    /// # Ok(())
    /// # }
    /// # tokio_test::block_on(async {
    /// #    exec().await
    /// # });
    /// ```
    #[cfg_attr(
        feature = "trace",
        instrument(name = "Thorium::Entities::delete", skip(self), err(Debug))
    )]
    pub async fn delete(&self, id: Uuid) -> Result<reqwest::Response, Error> {
        // build url for deleting an entity
        let url = format!("{base}/api/entities/{id}", base = self.host);
        // build request
        let req = self
            .client
            .delete(&url)
            .header("authorization", &self.token);
        // send this request
        send!(self.client, req)
    }

    /// Adds tags to an [`Entity`] in Thorium
    ///
    /// # Arguments
    ///
    /// * `id` - The id of the entity to add tags to
    /// * `tags` - The tag request to send, naming the groups the tags are visible to
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::Thorium;
    /// use thorium::models::TagRequest;
    /// # use thorium::Error;
    /// use uuid::Uuid;
    ///
    /// # async fn exec() -> Result<(), Error> {
    /// // create a Thorium client
    /// let thorium = Thorium::build("http://127.0.0.1").token("<token>").build().await?;
    /// // build a request to add a tag in one group
    /// let tags = TagRequest::default().add("plant", "corn").group("corn");
    /// // add the tag to an entity (use a real entity id though)
    /// thorium.entities.tag(Uuid::new_v4(), &tags).await?;
    /// # // allow test code to be compiled but don't unwrap as no API instance would be up
    /// # Ok(())
    /// # }
    /// # tokio_test::block_on(async {
    /// #    exec().await
    /// # });
    /// ```
    #[cfg_attr(
        feature = "trace",
        instrument(name = "Thorium::Entities::tag", skip(self, tags), err(Debug))
    )]
    pub async fn tag(
        &self,
        id: Uuid,
        tags: &TagRequest<Entity>,
    ) -> Result<reqwest::Response, Error> {
        // build url for tagging an entity
        let url = format!("{base}/api/entities/tags/{id}", base = self.host);
        // build request
        let req = self
            .client
            .post(&url)
            .json(tags)
            .header("authorization", &self.token);
        // send this request
        send!(self.client, req)
    }

    /// Removes tags from an [`Entity`] in Thorium
    ///
    /// # Arguments
    ///
    /// * `id` - The id of the entity to remove tags from
    /// * `tags_del` - The delete request to send, naming the groups to remove the tags from
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::Thorium;
    /// use thorium::models::TagDeleteRequest;
    /// # use thorium::Error;
    /// use uuid::Uuid;
    ///
    /// # async fn exec() -> Result<(), Error> {
    /// // create a Thorium client
    /// let thorium = Thorium::build("http://127.0.0.1").token("<token>").build().await?;
    /// // build a request to remove a tag from one group
    /// let tags_del = TagDeleteRequest::default().add("plant", "corn").group("corn");
    /// // remove the tag from an entity (use a real entity id though)
    /// thorium.entities.untag(Uuid::new_v4(), &tags_del).await?;
    /// # // allow test code to be compiled but don't unwrap as no API instance would be up
    /// # Ok(())
    /// # }
    /// # tokio_test::block_on(async {
    /// #    exec().await
    /// # });
    /// ```
    #[cfg_attr(
        feature = "trace",
        instrument(name = "Thorium::Entities::untag", skip(self, tags_del), err(Debug))
    )]
    pub async fn untag(
        &self,
        id: Uuid,
        tags_del: &TagDeleteRequest<Entity>,
    ) -> Result<reqwest::Response, Error> {
        // build url for removing tags from an entity
        let url = format!("{base}/api/entities/tags/{id}", base = self.host);
        // build request
        let req = self
            .client
            .delete(&url)
            .json(tags_del)
            .header("authorization", &self.token);
        // send this request
        send!(self.client, req)
    }

    /// Lists all entities that meet some search criteria
    ///
    /// Each line holds an entity's id, name, kind, and creation time; use
    /// [`Entities::list_details`] to get the full entities.
    ///
    /// # Arguments
    ///
    /// * `opts` - The options for this cursor
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::{Thorium, SearchDate};
    /// use thorium::models::EntityListOpts;
    /// # use thorium::Error;
    ///
    /// # async fn exec() -> Result<(), Error> {
    /// // create Thorium client
    /// let thorium = Thorium::build("http://127.0.0.1").token("<token>").build().await?;
    /// // build a search to list entities from 2020
    /// let search = EntityListOpts::default()
    ///     .start(SearchDate::year(2020, false)?)
    ///     .end(SearchDate::year(2020, true)?)
    ///     // limit it to 100 entities
    ///     .limit(100);
    /// // list the up to 100 entities from 2020
    /// thorium.entities.list(&search).await?;
    /// # // allow test code to be compiled but don't unwrap as no API instance would be up
    /// # Ok(())
    /// # }
    /// # tokio_test::block_on(async {
    /// #    exec().await
    /// # });
    /// ```
    #[cfg_attr(
        feature = "trace",
        instrument(name = "Thorium::entities::list", skip_all, err(Debug))
    )]
    pub async fn list(&self, opts: &EntityListOpts) -> Result<Cursor<EntityListLine>, Error> {
        // build the url for listing entities
        let url = format!("{}/api/entities/", self.host);
        // get the correct page size if our limit is smaller then our page_size
        let page_size = opts.limit.map_or_else(
            || opts.page_size,
            |limit| std::cmp::min(opts.page_size, limit),
        );
        // build our query params
        let mut query = vec![("limit".to_owned(), page_size.to_string())];
        add_query_list!(query, "groups[]".to_owned(), opts.groups);
        add_date!(query, "start".to_owned(), opts.start);
        add_date!(query, "end".to_owned(), opts.end);
        add_query!(query, "cursor".to_owned(), opts.cursor);
        add_query_list!(query, "kinds[]".to_owned(), opts.kinds);
        // add our tag query params
        for (key, values) in &opts.tags {
            // build the key for this tag param
            let query_key = format!("tags[{key}][]");
            // add this tag keys filters to our query params
            add_query_list_clone!(query, query_key, values);
        }
        add_query_bool!(
            query,
            "tags_case_insensitive".to_owned(),
            opts.tags_case_insensitive
        );
        // get the data for this request and create our cursor
        Cursor::new(
            &url,
            opts.page_size,
            opts.limit,
            &self.token,
            &query,
            &self.client,
        )
        .await
    }

    /// Lists all entities that meet some search criteria with details
    ///
    /// # Arguments
    ///
    /// * `search` - The search criteria for this query
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::{Thorium, SearchDate};
    /// use thorium::models::EntityListOpts;
    /// # use thorium::Error;
    ///
    /// # async fn exec() -> Result<(), Error> {
    /// // create Thorium client
    /// let thorium = Thorium::build("http://127.0.0.1").token("<token>").build().await?;
    /// // build a search to list entities from 2020 with details
    /// let search = EntityListOpts::default()
    ///     .start(SearchDate::year(2020, false)?)
    ///     .end(SearchDate::year(2020, true)?)
    ///     // limit it to 100 entities
    ///     .limit(100);
    /// // list the up to 100 entities from 2020
    /// thorium.entities.list_details(&search).await?;
    /// # // allow test code to be compiled but don't unwrap as no API instance would be up
    /// # Ok(())
    /// # }
    /// # tokio_test::block_on(async {
    /// #    exec().await
    /// # });
    /// ```
    #[cfg_attr(
        feature = "trace",
        instrument(name = "Thorium::entities::list_details", skip_all, err(Debug))
    )]
    pub async fn list_details(&self, opts: &EntityListOpts) -> Result<Cursor<Entity>, Error> {
        // build the url for listing entities
        let url = format!("{}/api/entities/details/", self.host);
        // get the correct page size if our limit is smaller then our page_size
        let page_size = opts.limit.map_or_else(
            || opts.page_size,
            |limit| std::cmp::min(opts.page_size, limit),
        );
        // build our query params
        let mut query = vec![("limit".to_owned(), page_size.to_string())];
        add_query_list!(query, "groups[]".to_owned(), opts.groups);
        add_query!(query, "start".to_owned(), opts.start);
        add_query!(query, "end".to_owned(), opts.end);
        add_query!(query, "cursor".to_owned(), opts.cursor);
        add_query_list!(query, "kinds[]".to_owned(), opts.kinds);
        // add our tag query params
        for (key, values) in &opts.tags {
            // build the key for this tag param
            let query_key = format!("tags[{key}][]");
            // add this tag keys filters to our query params
            add_query_list_clone!(query, query_key, values);
        }
        add_query_bool!(
            query,
            "tags_case_insensitive".to_owned(),
            opts.tags_case_insensitive
        );
        // get the data for this request and create our cursor
        Cursor::new(
            &url,
            opts.page_size,
            opts.limit,
            &self.token,
            &query,
            &self.client,
        )
        .await
    }
}

/// A wrapper that lets an entity's string key satisfy the generic key bound
struct EntityKey(String);

impl AsRef<String> for EntityKey {
    /// Borrow the string key inside this wrapper
    fn as_ref(&self) -> &String {
        &self.0
    }
}

#[cfg_attr(feature = "sync", thorium_derive::blocking_struct)]
impl Entities {
    /// Deletes tags from an entity in Thorium
    ///
    /// # Arguments
    ///
    /// * `id` - The id of the entity to delete tags from
    /// * `req` - The tags to delete from this entity
    pub async fn delete_tags(
        &self,
        id: Uuid,
        req: &TagDeleteRequest<Entity>,
    ) -> Result<reqwest::Response, Error> {
        // delete these tags through the untag route
        self.untag(id, req).await
    }

    /// Creates a notification for an entity
    ///
    /// # Arguments
    ///
    /// * `id` - The id of the entity to create a notification for
    /// * `req` - The notification create request
    /// * `params` - The notification create params
    pub async fn create_notification(
        &self,
        id: Uuid,
        req: &NotificationRequest<Entity>,
        params: &NotificationParams,
    ) -> Result<reqwest::Response, Error> {
        // entities are keyed by their id
        self.create_notification_generic(EntityKey(id.to_string()), req, params)
            .await
    }

    /// Gets all of an entity's notifications
    ///
    /// # Arguments
    ///
    /// * `id` - The id of the entity whose notifications we're requesting
    pub async fn get_notifications(&self, id: Uuid) -> Result<Vec<Notification<Entity>>, Error> {
        // entities are keyed by their id
        self.get_notifications_generic(EntityKey(id.to_string()))
            .await
    }

    /// Deletes an entity notification
    ///
    /// # Arguments
    ///
    /// * `id` - The id of the entity whose notification is being deleted
    /// * `notification_id` - The id of the notification to delete
    pub async fn delete_notification(
        &self,
        id: Uuid,
        notification_id: &Uuid,
    ) -> Result<reqwest::Response, Error> {
        // entities are keyed by their id
        self.delete_notification_generic(EntityKey(id.to_string()), notification_id)
            .await
    }
}

impl GenericClient for Entities {
    /// Provide the base url to the entities route in the API
    fn base_url(&self) -> String {
        format!("{}/api/entities", self.host)
    }

    /// Provide the configured client from `self`
    fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// Provide the configured auth token from `self`
    fn token(&self) -> &str {
        &self.token
    }
}

impl NotificationsClient for Entities {
    /// The underlying type that has the Notifications (see [`crate::models::backends::NotificationSupport`])
    type NotificationSupport = Entity;
}

impl GrepClientHelper for Entities {}

#[cfg_attr(feature = "sync", thorium_derive::blocking_trait)]
impl GrepClient for Entities {
    /// Greps a page of entities in Thorium
    ///
    /// This searches every entity you can see, so if there are more entities left to search
    /// then the returned cursor id should be passed back in with
    /// [`GrepOpts::cursor`](crate::models::GrepOpts::cursor) to continue the search.
    ///
    /// # Arguments
    ///
    /// * `opts` - The options to use when grepping entities
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::Thorium;
    /// use thorium::client::GrepClient;
    /// use thorium::models::{GrepOpts, GrepSource};
    /// # use thorium::Error;
    ///
    /// # async fn exec() -> Result<(), Error> {
    /// // create Thorium client
    /// let thorium = Thorium::build("http://127.0.0.1").token("<token>").build().await?;
    /// // grep the first page of entities for a pattern in their metadata
    /// let opts = GrepOpts::new("corn[0-9]+").source(GrepSource::Metadata).limit(10);
    /// let page = thorium.entities.grep(opts).await?;
    /// // if we got a cursor back then there are more entities left to search
    /// if let Some(cursor) = page.cursor {
    ///     let opts = GrepOpts::new("corn[0-9]+").source(GrepSource::Metadata).cursor(cursor);
    ///     thorium.entities.grep(opts).await?;
    /// }
    /// # // allow test code to be compiled but don't unwrap as no API instance would be up
    /// # Ok(())
    /// # }
    /// # tokio_test::block_on(async {
    /// #    exec().await
    /// # });
    /// ```
    #[cfg_attr(
        feature = "trace",
        instrument(name = "GrepClient<Entities>::grep", skip_all, err(Debug))
    )]
    async fn grep(&self, opts: GrepOpts) -> Result<GrepPage, Error> {
        self.grep_generic(opts).await
    }

    /// Greps a single entity in Thorium
    ///
    /// # Arguments
    ///
    /// * `id` - The id of the entity to grep
    /// * `opts` - The options to use when grepping this entity
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::Thorium;
    /// use thorium::client::GrepClient;
    /// use thorium::models::GrepOpts;
    /// use uuid::Uuid;
    /// # use thorium::Error;
    ///
    /// # async fn exec() -> Result<(), Error> {
    /// // create Thorium client
    /// let thorium = Thorium::build("http://127.0.0.1").token("<token>").build().await?;
    /// // grep a single entity for a pattern
    /// let id = Uuid::new_v4();
    /// thorium.entities.grep_key(id.to_string(), GrepOpts::new("corn[0-9]+")).await?;
    /// # // allow test code to be compiled but don't unwrap as no API instance would be up
    /// # Ok(())
    /// # }
    /// # tokio_test::block_on(async {
    /// #    exec().await
    /// # });
    /// ```
    #[cfg_attr(
        feature = "trace",
        instrument(name = "GrepClient<Entities>::grep_key", skip_all, err(Debug))
    )]
    async fn grep_key<K: AsRef<str>>(&self, id: K, opts: GrepOpts) -> Result<GrepMatches, Error> {
        self.grep_key_generic(id, opts).await
    }
}
