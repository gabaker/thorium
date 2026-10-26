//! Backend related logic for entities

use axum::extract::multipart::Field;
use axum::extract::{FromRequestParts, Multipart};
use axum::http::StatusCode;
use axum::http::request::Parts;
use chrono::{DateTime, Utc};
use futures::stream::{self, StreamExt};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::str::FromStr;
use tracing::instrument;
use uuid::Uuid;

use super::db;
use crate::models::backends::associations as edges;
use crate::models::backends::db::{CursorCore, ScyllaCursor, ScyllaCursorSupport};
use crate::models::backends::{GraphicSupport, TagSupport};
use crate::models::entities::EntityMetadataForm;
use crate::models::entities::incident::Incident;
use crate::models::entities::network_activity::{NetConState, NetworkConnection};
use crate::models::schema::queries::EntitiesListCursor;
use crate::models::schema::{Bucket, CursorLimit, DEFAULT_LIMIT_CAP};
use crate::models::{
    ApiCursor, Association, AssociationKind, AssociationListOpts, AssociationRequest,
    AssociationTarget, AssociationTargetColumn, CollectionEntity, CompiledFunction, Country,
    CriticalSector, DecompiledFunction, DeviceEntity, Entity, EntityForm, EntityKinds,
    EntityListLine, EntityListParams, EntityListRow, EntityMetadata, EntityMetadataUpdateForm,
    EntityResponse, EntityRow, EntityUpdateForm, Event, FileSystemEntity, FileSystemFolderEntity,
    Flag, Group, GroupAllowAction, GroupPolicy, ListableAssociation, ModelApi, ModelMatcher,
    ModelSecret, NetworkPolicy, PeImportEntity, PeSectionEntity, Repo, Sample, SigmaRule,
    TagListRow, TagMap, TagType, ToolSetSelector, TreeSupport, User, VendorEntity,
    WindowsProcessEntity, WindowsProcessTreeEntity, WorkflowGraphChange, WorkflowLimits,
};
use crate::utils::{ApiError, Shared, helpers};
use crate::{
    bad, bad_internal, deserialize, ensure_empty_segment, ensure_segments_complete, for_groups,
    internal_err, internal_err_unwrapped, is_admin, opt_tag, opt_tag_to_string, serialize, tag,
    tag_list_clone, unauthorized, update, update_add_rem, update_clear_opt, update_opt,
};

mod agents;
mod collections;
mod devices;
pub(crate) mod models;
pub(crate) mod references;
pub(crate) mod tools;
pub(crate) mod toolsets;
pub mod workflows;

pub(crate) use agents::check_new_calls;
pub use tools::{PublishStamp, create_migrated_tool};

/// How a group change treats an entity's `PublishedFrom` edges
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PublishedFromEdges {
    /// Copy them into every added group the other end is in, whatever the group allows, since
    /// they link an instance to its root and are written by Thorium rather than a user
    Follow,
    /// Leave them alone, because the root's own group change copies them
    Skip,
}

/// Refuse the changes tools, tool instances, toolset instances, and workflow instances never take
///
/// # Arguments
///
/// * `kind` - The kind of entity being updated
/// * `update_form` - The update to check
/// * `image_uploaded` - Whether the update uploads an image
fn check_tool_update(
    kind: EntityKinds,
    update_form: &EntityUpdateForm,
    image_uploaded: bool,
) -> Result<(), ApiError> {
    // a toolset instance only takes a new name or description
    if kind == EntityKinds::ToolSetInstance {
        toolsets::check_instance_update(update_form, image_uploaded)?;
    }
    // so does a workflow instance
    if kind == EntityKinds::WorkflowInstance {
        workflows::check_instance_update(update_form, image_uploaded)?;
    }
    // a tool instance's name and description are set when it's published
    if kind == EntityKinds::ToolInstance
        && (update_form.name.is_some()
            || update_form.description.is_some()
            || update_form.clear_description == Some(true))
    {
        return bad!("a tool instance's name and description can't change".to_owned());
    }
    // a tool publishes a new instance on its own, never with a group change
    let publishes =
        update_form.metadata.tool_version.is_some() || update_form.metadata.tool_spec.is_some();
    if kind == EntityKinds::Tool
        && publishes
        && (!update_form.add_groups.is_empty() || !update_form.remove_groups.is_empty())
    {
        return bad!(
            "publish a new tool version and change the tool's groups in separate requests"
                .to_owned()
        );
    }
    Ok(())
}

/// Apply the settings fields of an update to an agent, toolset, or workflow, marking it modified
///
/// # Arguments
///
/// * `name` - The name of the entity
/// * `metadata` - The entity's metadata
/// * `form` - The metadata update form
fn update_graph_meta(
    name: &str,
    metadata: &mut EntityMetadata,
    form: &mut EntityMetadataUpdateForm,
) -> Result<(), ApiError> {
    match metadata {
        // replace the config and model auth levels
        EntityMetadata::Agent(agent) => agents::update_meta(agent, form),
        // change the settings and selector
        EntityMetadata::ToolSet(tool_set) => toolsets::update_meta(tool_set, form),
        // change the entry agent and settings
        EntityMetadata::WorkflowCatalog(workflow) => workflows::update_meta(name, workflow, form),
        // no other kind is part of the agentic graph
        _ => Ok(()),
    }
}

/// The settings of an agent, toolset, or workflow whose changes may require new instances
#[derive(Debug, PartialEq)]
enum GraphSettings {
    /// The hash of an agent's config
    Agent(String),
    /// A toolset's settings
    ToolSet {
        /// Whether only tools that declare no effects are members
        read_only: bool,
        /// The tag that selects members
        selector: Option<ToolSetSelector>,
        /// Whether instances are built automatically
        auto_instance: bool,
    },
    /// A workflow's graph settings
    Workflow {
        /// The agent that receives a run's inputs first
        entry_agent: Option<Uuid>,
        /// Whether instances are built automatically
        auto_instance: bool,
        /// The run-wide limits
        limits: WorkflowLimits,
        /// The default models, highest priority first
        default_models: Vec<ModelMatcher>,
    },
}

impl GraphSettings {
    /// Get the graph settings of an entity's metadata, if its kind has any
    ///
    /// # Arguments
    ///
    /// * `metadata` - The entity's metadata
    fn of(metadata: &EntityMetadata) -> Option<Self> {
        // agents are compared by their config, and toolsets and workflows by the settings they're
        // built from
        match metadata {
            EntityMetadata::Agent(agent) => Some(Self::Agent(agent.config_hash.clone())),
            EntityMetadata::ToolSet(tool_set) => Some(Self::ToolSet {
                read_only: tool_set.read_only,
                selector: tool_set.selector.clone(),
                auto_instance: tool_set.auto_instance,
            }),
            EntityMetadata::WorkflowCatalog(workflow) => Some(Self::Workflow {
                entry_agent: workflow.entry_agent,
                auto_instance: workflow.auto_instance,
                limits: workflow.limits.clone(),
                default_models: workflow.default_models.clone(),
            }),
            _ => None,
        }
    }

    /// Get the change to queue when an entity's graph settings changed in an update
    ///
    /// # Arguments
    ///
    /// * `before` - The entity's graph settings before the update
    /// * `metadata` - The entity's metadata after the update
    /// * `id` - The entity's id
    fn changed(
        before: Option<&Self>,
        metadata: &EntityMetadata,
        id: Uuid,
    ) -> Option<WorkflowGraphChange> {
        // only a change of the settings queues anything
        let after = Self::of(metadata);
        if before.is_none() || before == after.as_ref() {
            return None;
        }
        // name the agent, toolset, or workflow that changed
        match after {
            Some(Self::Agent(_)) => Some(WorkflowGraphChange::AgentUpdated { agent: id }),
            Some(Self::ToolSet { .. }) => {
                Some(WorkflowGraphChange::ToolSetChanged { tool_set: id })
            }
            Some(Self::Workflow { .. }) => {
                Some(WorkflowGraphChange::WorkflowUpdated { workflow: id })
            }
            None => None,
        }
    }
}

/// An entity update read from its form, with the images it uploaded
struct ParsedUpdate {
    /// The fields of the update
    form: EntityUpdateForm,
    /// The keys of the images the update uploaded, in the order they were sent
    images: Vec<String>,
}

/// A target that gains groups so the edges a request adds to it stay contained
///
/// A tool instance widens its tool, and every instance of a tool follows it.
#[derive(Debug)]
pub(crate) struct WideningPlan {
    /// The root that gains the groups, in every one of its groups
    root: Entity,
    /// The groups the root gains
    add: Vec<String>,
    /// The root's instances, which gain the groups with it
    instances: Vec<Entity>,
    /// The network policies an admin's widening of a tool adds to the added groups
    policy_adds: Vec<(NetworkPolicy, Vec<String>)>,
}

impl WideningPlan {
    /// Get the id of the root this plan widens
    #[must_use]
    pub(crate) fn root_id(&self) -> Uuid {
        self.root.id
    }

    /// Merge another plan for the same root into this one
    ///
    /// # Arguments
    ///
    /// * `other` - The other plan for this root
    pub(crate) fn merge(&mut self, other: WideningPlan) {
        // add the other plan's groups this plan doesn't add yet
        let added = helpers::missing_groups(&other.add, &self.add);
        self.add.extend(added);
        // keep each policy add once
        for (policy, groups) in other.policy_adds {
            match self
                .policy_adds
                .iter_mut()
                .find(|(queued, _)| queued.id == policy.id)
            {
                Some((_, queued)) => {
                    let missing = helpers::missing_groups(&groups, queued);
                    queued.extend(missing);
                }
                None => self.policy_adds.push((policy, groups)),
            }
        }
    }
}

/// The checked parts of a group change that are written after the entity itself
#[derive(Debug, Default)]
pub(super) struct GroupChangePlan {
    /// The instances of a root that follow its group change
    instances: Vec<Entity>,
    /// The network policies an admin's widening of a tool adds to the added groups
    policy_adds: Vec<(NetworkPolicy, Vec<String>)>,
}

/// Whether an entity kind is a definition that runs and other runs follow, as opposed to data
/// that harnesses and tools write about files and repos
///
/// A run token is handed to every worker of a run, so it can't write these kinds.
///
/// # Arguments
///
/// * `kind` - The kind of entity to check
#[must_use]
pub fn is_definition_kind(kind: &EntityKinds) -> bool {
    // the kinds that describe tools, agents, workflows, and models
    matches!(
        kind,
        EntityKinds::Tool
            | EntityKinds::ToolInstance
            | EntityKinds::ToolSet
            | EntityKinds::ToolSetInstance
            | EntityKinds::Agent
            | EntityKinds::WorkflowCatalog
            | EntityKinds::WorkflowInstance
            | EntityKinds::ModelEndpoint
            | EntityKinds::ModelProfile
            | EntityKinds::ModelCredential
    )
}

/// Refuse a scoped token a write to a model credential
///
/// A credential holds a secret a group's runs authenticate with, so only a full token of an owner
/// or manager rotates, creates, or deletes one. A run token is a scoped token and is refused too.
///
/// # Arguments
///
/// * `kind` - The kind of entity the request would write
/// * `scoped` - Whether the user authenticated with a scoped token
/// * `action` - What the request would do to it, used in the refusal message
fn refuse_scoped_credential_write(
    kind: EntityKinds,
    scoped: bool,
    action: &str,
) -> Result<(), ApiError> {
    // only a scoped token writing a credential is refused
    if scoped && kind == EntityKinds::ModelCredential {
        return unauthorized!(format!(
            "A scoped token can't {action} a model credential, use your full token"
        ));
    }
    Ok(())
}

impl Entity {
    /// A helper function for creating an entity by taking a form, validating it, and submitting it
    /// to the database
    ///
    /// Returns the ID of the created entity. Tools, tool instances, model configuration, agents,
    /// toolsets, workflows, and their instances are each created by their own path that checks the
    /// form before anything is written. Every tool instance a form names must exist and be visible
    /// to the user.
    ///
    /// # Arguments
    ///
    /// * `form` - The multipart form that was submitted
    /// * `s3_path` - The s3 path to set if an image is uploaded
    /// * `user` - The user creating the entity
    /// * `run` - Whether the user authenticated with a run token
    /// * `scoped` - Whether the user authenticated with a scoped token
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "Entity::create_helper", skip_all, err(Debug))]
    async fn create_helper(
        mut form: Multipart,
        s3_path: &mut Option<String>,
        user: &User,
        run: bool,
        scoped: bool,
        shared: &Shared,
    ) -> Result<EntityResponse, ApiError> {
        // generate a UUID for this entity
        let entity_id = Uuid::new_v4();
        // build an entity form to populate
        let mut entity_form = EntityForm::default();
        // crawl the multipart form
        while let Some(field) = form.next_field().await? {
            // try to consume the field
            if let Some(image_field) = entity_form.add(field).await? {
                // get the base path for this entity
                let base_path = Self::build_graphic_base_path(&entity_id);
                // upload the graphic to S3
                let path = Self::upload_graphic(base_path, image_field, None, shared).await?;
                // set the s3 path
                s3_path.replace(path.clone());
                // mark that this entity has an image
                entity_form.image = Some(path);
            }
        }
        // a run token can't create a definition that other runs could follow
        if run && entity_form.kind.as_ref().is_some_and(is_definition_kind) {
            return unauthorized!("A run token can't create a definition".to_owned());
        }
        // only a full token creates a model credential
        if let Some(kind) = entity_form.kind {
            refuse_scoped_credential_write(kind, scoped, "create")?;
        }
        // give the entity the id a test chose for it, so the test can order ids
        #[cfg(feature = "test-utilities")]
        let entity_id = entity_form
            .name
            .as_deref()
            .and_then(crate::test_utilities::faults::take_create_id)
            .unwrap_or(entity_id);
        // first, make sure we actually have edit access in all requested groups
        let _ = Group::authorize_check_allow_all(
            user,
            &entity_form.groups,
            Group::editable,
            "edit",
            Some(GroupAllowAction::Entities),
            shared,
        )
        .await?;
        // make sure the data in the finished form is valid
        entity_form.validate()?;
        // tools and tool instances are created through the shared publish path
        match entity_form.kind {
            Some(EntityKinds::Tool) => {
                // create the tool and its first instance
                return Box::pin(tools::create_tool(
                    user,
                    entity_id,
                    entity_form.name.as_ref(),
                    entity_form.description,
                    entity_form.image,
                    entity_form.groups,
                    entity_form.metadata,
                    entity_form.tags,
                    shared,
                ))
                .await;
            }
            Some(EntityKinds::ToolInstance) => {
                // publish a new instance of an existing tool
                return Box::pin(tools::create_instance(
                    user,
                    entity_form.metadata,
                    entity_form.tags,
                    shared,
                ))
                .await;
            }
            Some(
                kind @ (EntityKinds::ModelEndpoint
                | EntityKinds::ModelProfile
                | EntityKinds::ModelCredential),
            ) => {
                // model configuration checks its references before anything is written
                return Box::pin(models::create(user, entity_id, kind, entity_form, shared)).await;
            }
            Some(EntityKinds::Agent) => {
                // agents check their config and every edge before anything is written
                return Box::pin(agents::create(user, entity_id, entity_form, shared)).await;
            }
            Some(EntityKinds::ToolSet) => {
                // toolsets check their settings and every edge before anything is written
                return Box::pin(toolsets::create(user, entity_id, entity_form, shared)).await;
            }
            Some(EntityKinds::ToolSetInstance) => {
                // a submitted toolset instance is validated, never rebuilt
                return Box::pin(toolsets::create_instance(
                    user,
                    entity_id,
                    entity_form,
                    shared,
                ))
                .await;
            }
            Some(EntityKinds::WorkflowCatalog) => {
                // workflows check their settings and every edge before anything is written
                return Box::pin(workflows::create(user, entity_id, entity_form, shared)).await;
            }
            Some(EntityKinds::WorkflowInstance) => {
                // a submitted workflow instance is validated, never rebuilt
                return Box::pin(workflows::create_instance(
                    user,
                    entity_id,
                    entity_form,
                    shared,
                ))
                .await;
            }
            _ => (),
        }
        // make sure every tool instance this entity names exists and this user can see it
        tools::check_tool_instances(user, &entity_form.metadata.tool_ids, shared).await?;
        // create the entity
        let entity = Box::pin(db::entities::create(user, entity_form, entity_id, shared)).await?;
        // the entity was created successfully
        Ok(EntityResponse::new(entity_id, entity.name))
    }

    /// Create an `Entity` in the db
    ///
    /// # Arguments
    ///
    /// * `user` - The user creating the entity
    /// * `run` - Whether the user authenticated with a run token
    /// * `scoped` - Whether the user authenticated with a scoped token
    /// * `form` - The multipart form that was uploaded
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "Entity::create", skip_all, err(Debug))]
    pub async fn create(
        user: &User,
        run: bool,
        scoped: bool,
        form: Multipart,
        shared: &Shared,
    ) -> Result<EntityResponse, ApiError> {
        // make a mutable option for the S3 path that's set if an image is uploaded to S3
        let mut s3_path: Option<String> = None;
        // try creating the entity
        match Box::pin(Self::create_helper(
            form,
            &mut s3_path,
            user,
            run,
            scoped,
            shared,
        ))
        .await
        {
            // entity was successfully created, so return a response
            Ok(resp) => Ok(resp),
            // we got an error creating the entity, so delete the image from S3 in case
            // we uploaded it and propagate the error
            Err(err) => {
                // delete from S3 if our path was set to avoid a dangling image
                if let Some(s3_path) = &s3_path {
                    // we have the path in S3, so just delete it with the client directly
                    if let Err(s3_err) = shared.s3.graphics.delete(s3_path).await {
                        return internal_err!(format!(
                            "Error cleaning up image after entity create error: {s3_err}. Original error: {err}"
                        ));
                    }
                }
                // propogate the error
                Err(err)
            }
        }
    }

    /// Add a tool, toolset, or workflow instance's intrinsic tags
    ///
    /// # Arguments
    ///
    /// * `tags` - The tags to add to
    fn instance_intrinsic_tags(&self, tags: &mut HashMap<String, HashSet<String>>) {
        match &self.metadata {
            EntityMetadata::ToolInstance(instance) => {
                // tag the tool, version, kind, and hash so versions can be queried
                tag!(tags, "ToolId", instance.tool.to_string());
                tag!(tags, "ToolVersion", instance.version.to_string());
                tag!(tags, "ToolKind", instance.spec.kind().to_string());
                tag!(tags, "ToolSpecHash", instance.spec_hash.clone());
                // tag the function name agentic tools are called by
                if let Some(function_name) = instance.spec.function_name() {
                    tag!(tags, "ToolFunction", function_name.to_owned());
                }
            }
            // tag the toolset, revision, digest, status, followed tools, and selector
            EntityMetadata::ToolSetInstance(instance) => {
                toolsets::intrinsic_tags(instance, &self.tags, tags);
            }
            // tag the workflow, revision, digest, status, agents, and followed roots
            EntityMetadata::WorkflowInstance(instance) => {
                workflows::intrinsic_tags(instance, &self.tags, tags);
            }
            // no other kind is an instance with intrinsic tags
            _ => (),
        }
    }

    /// Add the tags Thorium derives from this entity's own data to a tag map
    ///
    /// Instances are tagged with their root, version or revision, hash, and what they follow. Kinds
    /// with no taggable data add only the submitter tag.
    ///
    /// # Arguments
    ///
    /// * `tags` - The tag map to add this entity's intrinsic tags to
    pub fn populate_intrinsic_tags(
        &self,
        tags: &mut HashMap<String, HashSet<String>>,
    ) -> Result<(), ApiError> {
        // tag this entity with its submitter so it can be queried by submitter
        tag!(tags, "submitter", self.submitter.clone());
        match &self.metadata {
            EntityMetadata::Device(device) => {
                // add all of the vendors for this device to tags
                for sector in &device.critical_sectors {
                    tag!(tags, "CriticalSectors", sector.to_string());
                }
            }
            EntityMetadata::Vendor(vendor) => {
                // add all of the vendors for this device to tags
                for sector in &vendor.critical_sectors {
                    tag!(tags, "CriticalSectors", sector.to_string());
                }
            }
            EntityMetadata::FileSystem(fs) => {
                tag!(tags, "FsSha256", fs.sha256.clone());
            }
            EntityMetadata::Folder(folder) => {
                tag!(tags, "FolderNamesSha256", folder.names_sha256.clone());
                tag!(tags, "FolderDataSha256", folder.data_sha256.clone());
                tag!(tags, "FolderAllSha256", folder.all_sha256.clone());
            }
            EntityMetadata::WindowsProcess(win_proc) => {
                tag!(tags, "PID", win_proc.pid.to_string());
                opt_tag_to_string!(tags, "ParentPID", win_proc.parent_pid);
                opt_tag!(tags, "ProcessName", win_proc.name.clone());
                opt_tag!(tags, "ProcessImagePath", win_proc.image_path.clone());
                opt_tag!(tags, "ProcessCommand", win_proc.command.clone());
                opt_tag_to_string!(tags, "ProcessOffset", win_proc.offset);
                opt_tag_to_string!(tags, "ProcessThreads", win_proc.threads);
                opt_tag_to_string!(tags, "ProcessHandles", win_proc.handles);
                opt_tag_to_string!(tags, "ProcessIsWow64", win_proc.is_wow64);
                opt_tag_to_string!(tags, "ProcessSessionID", win_proc.session_id);
            }
            EntityMetadata::NetworkConnection(conn) => {
                // tag the source ip this network connection comes from
                tag!(tags, "NetConnSource", conn.source.to_string());
                // add our source port info if we have it
                if let Some(port) = &conn.source_port {
                    tag!(tags, "NetConnSourcePort", port.to_string());
                    // also add a addr/port combo
                    tag!(
                        tags,
                        "NetConnSourceAddrAndPort",
                        format!("{}:{port}", conn.source)
                    );
                }
                tag!(tags, "NetConnDest", conn.destination.to_string());
                // add our destination port info
                tag!(tags, "NetConnDestPort", conn.destination_port.to_string());
                // also add a addr/port combo
                tag!(
                    tags,
                    "NetConnDestAddrAndPort",
                    format!("{}:{}", conn.destination, conn.destination_port)
                );
                // add the state of this connection
                opt_tag_to_string!(tags, "NetConnState", conn.state);
                // add this network connections pid
                opt_tag_to_string!(tags, "NetConPID", conn.pid);
                // add this network connections owner process
                opt_tag!(tags, "NetConProcess", conn.process.clone());
            }
            EntityMetadata::SigmaRule(rule) => {
                // parse this sigma rule
                let parsed: sigma_rust::Rule = serde_norway::from_str(&rule.rule)?;
                // tag this rules id
                opt_tag!(tags, "SigmaRuleId", parsed.id);
                // tag this rules author
                opt_tag!(tags, "SigmaRuleAuthor", parsed.author);
                // tag this rules status
                opt_tag_to_string!(tags, "SigmaRuleStatus", parsed.status);
                // tag this rules level
                opt_tag_to_string!(tags, "SigmaRuleLevel", parsed.level);
            }
            EntityMetadata::Flag(flag) => {
                // tag this rules confidence
                tag!(tags, "FlagConfidence", flag.confidence.to_string());
            }
            EntityMetadata::Incident(incident) => {
                opt_tag!(
                    tags,
                    "IncidentCoverTerm".to_owned(),
                    incident.cover_term.clone()
                );
                tag_list_clone!(tags, "MissionTeam".to_owned(), incident.mission_teams);
                tag_list_clone!(tags, "IncidentNetwork".to_owned(), incident.networks);
                tag_list_clone!(tags, "IncidentMachine".to_owned(), incident.machines);
                tag_list_clone!(tags, "IncidentLocation".to_owned(), incident.locations);
            }
            EntityMetadata::PeSection(section) => {
                // tag this section's content hash so sections can be looked up by md5
                opt_tag!(tags, "SectionMd5", section.md5.clone());
            }
            EntityMetadata::PeImport(import) => {
                // tag each imported function so samples can be found by imported function
                for function in &import.functions {
                    tag!(tags, "ImportedFunction", function.clone());
                }
            }
            EntityMetadata::ToolInstance(_)
            | EntityMetadata::ToolSetInstance(_)
            | EntityMetadata::WorkflowInstance(_) => {
                // tag the instance's root, version or revision, hash, and what it follows
                self.instance_intrinsic_tags(tags);
            }
            // other and windows process trees have no taggable data
            EntityMetadata::Tool(_)
            | EntityMetadata::WorkflowCatalog(_)
            | EntityMetadata::Agent(_)
            | EntityMetadata::ToolSet(_)
            | EntityMetadata::ModelEndpoint(_)
            | EntityMetadata::ModelProfile(_)
            | EntityMetadata::ModelCredential(_)
            | EntityMetadata::Other
            | EntityMetadata::Collection(_)
            | EntityMetadata::WindowsProcessTree(_)
            | EntityMetadata::CompiledFunction(_)
            | EntityMetadata::DecompiledFunction(_) => (),
        }
        Ok(())
    }

    /// Populate some entities associations
    ///
    /// A tool lists its instances, a harness instance lists the native tools it provides, and an
    /// agent, toolset, or workflow fills its edges from its graph edges. A device lists its
    /// vendors.
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is populating assocations for an entity
    /// * `shared` - Shared thorium objects
    async fn populate_associations(
        mut self,
        user: &User,
        shared: &Shared,
    ) -> Result<Self, ApiError> {
        // a tool lists its instances from their edges in the groups we can see it in
        if self.kind == EntityKinds::Tool {
            // summarize the tool's instances newest first
            let summaries = tools::list_summaries(user, &self, shared).await?;
            if let EntityMetadata::Tool(tool) = &mut self.metadata {
                tool.instances = summaries;
            }
            return Ok(self);
        }
        // a harness instance lists the native tools it provides
        if self.kind == EntityKinds::ToolInstance {
            let provided = tools::provided_ids(user, &self, shared).await?;
            if let EntityMetadata::ToolInstance(instance) = &mut self.metadata {
                instance.provides = provided;
            }
            return Ok(self);
        }
        // an agent fills its edges from its outgoing graph edges
        if self.kind == EntityKinds::Agent {
            return Box::pin(agents::populate(user, self, shared)).await;
        }
        // a toolset fills its tools from its includes and finds its latest ready instance
        if self.kind == EntityKinds::ToolSet {
            return Box::pin(toolsets::populate(user, self, shared)).await;
        }
        // a workflow fills its agents from its edges
        if self.kind == EntityKinds::WorkflowCatalog {
            return Box::pin(workflows::populate(user, self, shared)).await;
        }
        // build the source for this entity
        if let Some(source) = self.build_association_target_column() {
            // build the options for listing this entities associations
            let opts = AssociationListOpts::default().groups(user.groups.clone());
            // list associations for this entity scoped to the requesting user
            let mut cursor = db::associations::list(user, opts, &source, shared).await?;
            // based on our kind populate any association data
            match &mut self.metadata {
                EntityMetadata::Device(metadata) => {
                    // build a set of entities associated with our entity
                    let mut ids = Vec::with_capacity(3);
                    // get only the vendor associations
                    loop {
                        // filter only to developed by associations
                        for association in cursor.data.drain(..) {
                            // skip any associations that are not developed by
                            if association.kind == AssociationKind::DevelopedBy {
                                // parse our other value for this association
                                let other: AssociationTargetColumn =
                                    deserialize!(&association.other);
                                // add any entity ids we find to our list
                                if let AssociationTargetColumn::Entity(id) = other {
                                    ids.push(id);
                                }
                            }
                        }
                        // if our cursor is exhausted then stop looping
                        if cursor.exhausted() {
                            break;
                        }
                        // get the next page of associations
                        cursor.next(shared).await?;
                    }
                    // get all of the entities we found
                    metadata.vendors = db::entities::get_many(&user.groups, &ids, shared).await?;
                }
                // vendor/collection/... has no data that we need to retrieve
                EntityMetadata::Vendor(_)
                | EntityMetadata::Collection(_)
                | EntityMetadata::Other
                | EntityMetadata::FileSystem(_)
                | EntityMetadata::Folder(_)
                | EntityMetadata::WindowsProcessTree(_)
                | EntityMetadata::WindowsProcess(_)
                | EntityMetadata::PeSection(_)
                | EntityMetadata::PeImport(_)
                | EntityMetadata::SigmaRule(_)
                | EntityMetadata::Flag(_)
                | EntityMetadata::Incident(_)
                | EntityMetadata::CompiledFunction(_)
                | EntityMetadata::DecompiledFunction(_)
                | EntityMetadata::Tool(_)
                | EntityMetadata::ToolInstance(_)
                | EntityMetadata::Agent(_)
                | EntityMetadata::ToolSet(_)
                | EntityMetadata::ToolSetInstance(_)
                | EntityMetadata::WorkflowCatalog(_)
                | EntityMetadata::WorkflowInstance(_)
                | EntityMetadata::ModelEndpoint(_)
                | EntityMetadata::ModelProfile(_)
                | EntityMetadata::ModelCredential(_)
                | EntityMetadata::NetworkConnection(_) => (),
            }
        }
        Ok(self)
    }

    /// Fill an entity's fields that are built from its associations
    ///
    /// # Arguments
    ///
    /// * `user` - The user reading the entity
    /// * `entity` - The entity to fill
    /// * `shared` - Shared Thorium objects
    pub(crate) async fn get_populated(
        user: &User,
        entity: Entity,
        shared: &Shared,
    ) -> Result<Entity, ApiError> {
        // fill the entity's associated data
        entity.populate_associations(user, shared).await
    }

    /// Get an `Entity` from the db
    ///
    /// # Arguments
    ///
    /// * `id` - The entity's id
    /// * `user` - The user getting the entity
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "Entity::get", skip_all, err(Debug))]
    pub async fn get(user: &User, id: Uuid, shared: &Shared) -> Result<Entity, ApiError> {
        // for users we can search their groups but for admins we need to get all groups
        // try to get this entity if it exists
        let entity = for_groups!(db::entities::get, user, shared, id)?;
        // populate any info based on associations for this entity
        entity.populate_associations(user, shared).await
    }

    /// List entities names, ids, and kinds according to the given params
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is listing entities
    /// * `params` - The params to use when listing entities
    /// * `dedupe` - Whether to dedupe when listing entities or not
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "Entity::list", skip(user, shared), err(Debug))]
    pub async fn list(
        user: &User,
        mut params: EntityListParams,
        dedupe: bool,
        shared: &Shared,
    ) -> Result<ApiCursor<EntityListLine>, ApiError> {
        // authorize the groups to list entities from
        user.authorize_groups(&mut params.groups, shared).await?;
        // get or create a cursor over entities scoped to the requesting user
        let scylla_cursor = db::entities::list(user, params, dedupe, shared).await?;
        // convert our scylla cursor to a user facing cursor
        Ok(ApiCursor::from(scylla_cursor))
    }

    /// Get a listable association cursor for this entity
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is listing associations
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "Entity::list_associations", skip_all, err(Debug))]
    pub(crate) async fn list_associations(
        &self,
        user: &User,
        shared: &Shared,
    ) -> Result<Option<ScyllaCursor<ListableAssociation>>, ApiError> {
        // get our source target column if possible
        match self.build_association_target_column() {
            Some(source) => {
                // list associations for this
                let opts = AssociationListOpts::default().groups(self.groups.clone());
                // list associations for this entity scoped to the requesting user
                let cursor = db::associations::list(user, opts, &source, shared).await?;
                Ok(Some(cursor))
            }
            None => Ok(None),
        }
    }

    /// List every association of an entity in both directions for an admin
    ///
    /// Only admins can list an entity's associations this way; this is meant for tests.
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is listing associations
    /// * `id` - The id of the entity to list associations for
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "Entity::admin_associations", skip(user, shared), err(Debug))]
    pub async fn admin_associations(
        user: &User,
        id: Uuid,
        shared: &Shared,
    ) -> Result<Vec<Association>, ApiError> {
        // only admins can list every association of an entity
        is_admin!(user);
        // get the entity, which fails with a 404 if it does not exist
        let entity = Entity::get(user, id, shared).await?;
        // list the first page of this entity's associations in all of its groups
        let Some(mut cursor) = entity.list_associations(user, shared).await? else {
            return Ok(Vec::default());
        };
        // start with the associations on the first page
        let mut associations = Vec::with_capacity(cursor.data.len());
        // page through every association of this entity
        loop {
            // convert this page's associations
            for listable in cursor.data.drain(..) {
                associations.push(Association::try_from(listable)?);
            }
            // stop once our cursor is exhausted
            if cursor.exhausted() {
                break;
            }
            // get the next page of associations
            cursor.next(shared).await?;
        }
        Ok(associations)
    }

    /// Update an entity's kind specific metadata with the data in the form
    ///
    /// Tool instances added to a filesystem or windows process tree must exist and be visible to
    /// the user. Every instance id the form adds is stored once, and the legacy tool names the form
    /// removes are dropped.
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is updating this entity
    /// * `form` - The form whose data to use to update
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "Entity::update_meta", skip_all, err(Debug))]
    async fn update_meta(
        &mut self,
        user: &User,
        mut form: EntityMetadataUpdateForm,
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // update the fields for our kind
        match &mut self.metadata {
            // update our device info
            EntityMetadata::Device(device) => {
                // add any new urls
                device.urls.append(&mut form.add_urls);
                // remove any requested urls
                device.urls.retain(|url| !form.remove_urls.contains(url));
                // update our critical system flag if requested
                match (form.clear_critical_system, form.critical_system) {
                    (Some(true), _) => device.critical_system = None,
                    (_, Some(critical_system)) => device.critical_system = Some(critical_system),
                    (_, None) => (),
                }
                // update our sensitive location field
                update_opt!(device.sensitive_location, form.sensitive_location);
                // clear our sensistive location if needed
                update_clear_opt!(device.sensitive_location, form.clear_sensitive_location);
                // add any new critical sectors
                device
                    .critical_sectors
                    .extend(form.add_critical_sectors.drain(..));
                // remove any old critical sectors
                device
                    .critical_sectors
                    .retain(|sector| !form.remove_critical_sectors.contains(sector));
            }
            // update our vendor info
            EntityMetadata::Vendor(vendor) => {
                // Update the countries for this vendor
                vendor.countries.extend(form.add_countries.drain(..));
                // remove any countries from this vendor
                vendor
                    .countries
                    .retain(|country| !form.remove_countries.contains(country));
                // add any new critical sectors
                vendor
                    .critical_sectors
                    .extend(form.add_critical_sectors.drain(..));
                // remove any old critical sectors
                vendor
                    .critical_sectors
                    .retain(|sector| !form.remove_critical_sectors.contains(sector));
            }
            // update collection info
            EntityMetadata::Collection(collection) => {
                collection.update(&mut form)?;
            }
            // update this filesystems metadata
            EntityMetadata::FileSystem(fs) => {
                // make sure every added tool instance exists and this user can see it
                tools::check_tool_instances(user, &form.add_tool_ids, shared).await?;
                // add new tool instances that dumped this filesystem, each once
                crate::models::entities::shared::add_tool_ids(
                    &mut fs.tools,
                    std::mem::take(&mut form.add_tool_ids),
                );
                // remove any tool instances from old tool instances
                fs.tools.retain(|tool| !form.remove_tool_ids.contains(tool));
                // remove any stale legacy tool names
                fs.legacy_tools
                    .retain(|tool| !form.remove_legacy_tools.contains(tool));
            }
            EntityMetadata::WindowsProcessTree(win_proc_tree) => {
                // make sure every added tool instance exists and this user can see it
                tools::check_tool_instances(user, &form.add_tool_ids, shared).await?;
                // add new tool instances that dumped this windows process tree, each once
                crate::models::entities::shared::add_tool_ids(
                    &mut win_proc_tree.tools,
                    std::mem::take(&mut form.add_tool_ids),
                );
                // remove any tool instances from old tool instances
                win_proc_tree
                    .tools
                    .retain(|tool| !form.remove_tool_ids.contains(tool));
                // remove any stale legacy tool names
                win_proc_tree
                    .legacy_tools
                    .retain(|tool| !form.remove_legacy_tools.contains(tool));
            }
            EntityMetadata::WindowsProcess(win_proc) => {
                update_opt!(win_proc.name, form.name);
                update_opt!(win_proc.image_path, form.image_path);
                update_opt!(win_proc.command, form.command);
                update_opt!(win_proc.offset, form.offset);
                update_opt!(win_proc.threads, form.threads);
                update_opt!(win_proc.handles, form.handles);
                update_opt!(win_proc.is_wow64, form.is_wow64);
                update_opt!(win_proc.session_id, form.session_id);
                update_opt!(win_proc.create_time, form.create_time);
                update_opt!(win_proc.exit_time, form.exit_time);
            }
            EntityMetadata::NetworkConnection(conn) => {
                update_opt!(conn.protocol, form.protocol);
                update!(conn.source, form.source);
                update_opt!(conn.source_port, form.source_port);
                update!(conn.destination, form.destination);
                update!(conn.destination_port, form.destination_port);
                update_opt!(conn.state, form.state);
                update_opt!(conn.pid, form.pid);
                update_opt!(conn.process, form.process);
                update_opt!(conn.create_time, form.create_time);
            }
            EntityMetadata::SigmaRule(rule) => {
                // update our rule and score if needed
                update!(rule.rule, form.sigma_rule.take());
                update!(rule.score, form.score);
                // add new things this rule should apply too
                rule.applies_to.append(&mut form.add_sigma_applies_to);
                // remove any entities that this sigma rule no longer applies too
                rule.applies_to
                    .retain(|thing| !form.remove_sigma_applies_to.contains(thing));
                // add a new action to perform when this sigma rule hits
                rule.actions.append(&mut form.add_sigma_actions);
                // remove the requested indices in reverse order to ensure the indices remain valid
                // we could use swap_remove here and not have to do this but that would change the
                // order of things on update which could be annoying. Its also going to be rare that
                // we ever have multiple actions to remove anyways.
                for index in form.remove_sigma_actions.iter().rev() {
                    // make sure this index is in bounds
                    if *index < rule.actions.len() {
                        // remove this index if it exists
                        rule.actions.remove(*index);
                    }
                }
            }
            EntityMetadata::Flag(flag) => {
                // update the required values in this flag if needed
                update!(flag.suspicion, form.suspicion);
                update!(flag.confidence, form.confidence);
                update!(flag.reasoning, form.reasoning.take());
                // update any optional values if needed
                update_opt!(flag.content, form.content);
            }
            EntityMetadata::Incident(incident) => {
                // update our incident coverterm if needed
                update_opt!(incident.cover_term, form.cover_term);
                // add new mission teams to this incident
                incident.mission_teams.append(&mut form.add_mission_teams);
                // remove any mission teams from this incident
                incident
                    .mission_teams
                    .retain(|team| !form.remove_mission_teams.contains(team));
                // add new networks to this incident
                incident.networks.append(&mut form.add_networks);
                // remove any networks from this incident
                incident
                    .networks
                    .retain(|network| !form.remove_networks.contains(network));
                // add new machines to this incident
                incident.machines.append(&mut form.add_machines);
                // remove any machines from this incident
                incident
                    .machines
                    .retain(|machine| !form.remove_machines.contains(machine));
                // add new locations to this incident
                incident.locations.append(&mut form.add_locations);
                // remove any locations from this incident
                incident
                    .locations
                    .retain(|location| !form.remove_locations.contains(location));
            }
            EntityMetadata::CompiledFunction(func) => {
                // update this functions addres if needed
                update!(func.address, form.function_address);
                // replace our disassembly entirely if any was set
                if !form.disassembly.is_empty() {
                    // set our new disassembly
                    std::mem::swap(&mut func.disassembly, &mut form.disassembly);
                }
            }
            EntityMetadata::DecompiledFunction(decomp) => {
                // update this functions addres if needed
                update!(decomp.address, form.function_address);
                // update our decompilation if needed
                update!(decomp.content, form.decompilation_content);
                // make sure every added tool instance exists and this user can see it
                tools::check_tool_instances(user, &form.add_tool_ids, shared).await?;
                // add new tool instances that decompiled this function, each once
                crate::models::entities::shared::add_tool_ids(
                    &mut decomp.tools,
                    std::mem::take(&mut form.add_tool_ids),
                );
                // remove any old tool instances from this function
                decomp
                    .tools
                    .retain(|tool| !form.remove_tool_ids.contains(tool));
                // remove any stale legacy tool names
                decomp
                    .legacy_tools
                    .retain(|tool| !form.remove_legacy_tools.contains(tool));
            }
            EntityMetadata::PeSection(section) => {
                // update any section details that were set in the form
                update_opt!(section.md5, form.md5);
                update_opt!(section.raw_size, form.raw_size);
                update_opt!(section.virtual_size, form.virtual_size);
                update_opt!(section.entropy, form.entropy);
            }
            EntityMetadata::PeImport(import) => {
                // replace the imported functions if a new list was provided
                if !form.functions.is_empty() {
                    import.functions = std::mem::take(&mut form.functions);
                }
            }
            EntityMetadata::Tool(tool) => {
                // a tool only changes its definition by publishing a new instance
                if form.tool_instance_deprecated.is_some() {
                    return bad!(
                        "tool_instance_deprecated is only valid for a tool instance".to_owned()
                    );
                }
                // a harness's env and disabled models are edited on the root without publishing
                tools::update_harness_settings(tool, &mut form)?;
            }
            EntityMetadata::ToolInstance(instance) => {
                // a tool instance's spec, version, and native tools can never change
                if form.tool_version.is_some()
                    || form.tool_spec.is_some()
                    || !form.tool_provides.is_empty()
                {
                    return bad!(
                        "a tool instance's version, spec, and provided tools can't change; publish a new version of its tool instead"
                            .to_owned()
                    );
                }
                // a harness's settings live on its tool
                if form.tool_harness_env.is_some()
                    || form.clear_tool_harness_env.is_some()
                    || !form.disable_tool_models.is_empty()
                    || !form.enable_tool_models.is_empty()
                {
                    return bad!(
                        "a harness's env and disabled models are set on its tool, not on a tool instance"
                            .to_owned()
                    );
                }
                // deprecation is the only change a tool instance takes
                update!(instance.deprecated, form.tool_instance_deprecated);
            }
            graph @ (EntityMetadata::Agent(_)
            | EntityMetadata::ToolSet(_)
            | EntityMetadata::WorkflowCatalog(_)) => {
                // change an agent's, toolset's, or workflow's settings and mark it modified
                update_graph_meta(&self.name, graph, &mut form)?;
            }
            // other kinds have no metadata to update, and model configuration is updated by its
            // own handlers
            EntityMetadata::Other
            | EntityMetadata::Folder(_)
            | EntityMetadata::ToolSetInstance(_)
            | EntityMetadata::WorkflowInstance(_)
            | EntityMetadata::ModelEndpoint(_)
            | EntityMetadata::ModelProfile(_)
            | EntityMetadata::ModelCredential(_) => (),
        }
        // a tool publishes a new instance if a version and spec were given
        if self.kind == EntityKinds::Tool {
            Box::pin(tools::publish_update(
                user,
                self,
                form.tool_version.take(),
                form.tool_spec.take(),
                std::mem::take(&mut form.tool_provides),
                shared,
            ))
            .await?;
        }
        // update any associations based on this update
        helpers::heap_future(|| form.update_associations(user, self, shared)).await
    }

    /// Read an update's form, uploading each image it sends as it's read
    ///
    /// A failure deletes every image it uploaded, except one written over the image this entity
    /// already has.
    ///
    /// # Arguments
    ///
    /// * `user` - The user updating the entity
    /// * `scoped` - Whether the user authenticated with a scoped token
    /// * `form` - The multipart form that was submitted
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "Entity::parse_update", skip_all, err(Debug))]
    async fn parse_update(
        &self,
        user: &User,
        scoped: bool,
        mut form: Multipart,
        shared: &Shared,
    ) -> Result<ParsedUpdate, ApiError> {
        // build an entity update form to populate
        let mut update_form = EntityUpdateForm::default();
        // track the images this update uploads
        let mut images = Vec::new();
        // crawl the multipart form, uploading each image as it's read
        match self
            .read_update_form(user, &mut form, &mut update_form, &mut images, shared)
            .await
        {
            // a scoped token can't repoint a model endpoint, so discard what it uploaded
            Ok(())
                if scoped
                    && self.kind == EntityKinds::ModelEndpoint
                    && update_form.metadata.model_endpoint_provider.is_some() =>
            {
                let error = ApiError::new(
                    StatusCode::UNAUTHORIZED,
                    Some(
                        "A scoped token can't change a model endpoint's provider, use your full token"
                            .to_owned(),
                    ),
                );
                Err(Self::discard_uploads(&images, self.image.as_ref(), error, shared).await)
            }
            Ok(()) => Ok(ParsedUpdate {
                form: update_form,
                images,
            }),
            Err(error) => {
                Err(Self::discard_uploads(&images, self.image.as_ref(), error, shared).await)
            }
        }
    }

    /// Read every field of an update's form, uploading each image it sends
    ///
    /// # Arguments
    ///
    /// * `user` - The user updating the entity
    /// * `form` - The multipart form that was submitted
    /// * `update_form` - The update form to fill
    /// * `images` - The keys of the images uploaded so far, in the order they were sent
    /// * `shared` - Shared Thorium objects
    async fn read_update_form(
        &self,
        user: &User,
        form: &mut Multipart,
        update_form: &mut EntityUpdateForm,
        images: &mut Vec<String>,
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // crawl the multipart form
        while let Some(field) = form.next_field().await? {
            // try to consume the field
            if let Some(image_field) = update_form.add(field).await? {
                // a new image is an edit of model configuration, so authorize it before uploading
                if images.is_empty() && models::is_model_kind(self.kind) {
                    models::authorize_edit(user, self, shared).await?;
                }
                // get the base path for this entity
                let base_path = self.build_graphic_base_path_from_self(shared);
                // upload the graphic to S3
                images.push(Self::upload_graphic(base_path, image_field, None, shared).await?);
            }
        }
        Ok(())
    }

    /// Delete the images a failed update uploaded, keeping the image the entity already has
    ///
    /// Returns the update's error, or an error naming both failures if an image can't be
    /// deleted.
    ///
    /// # Arguments
    ///
    /// * `images` - The keys of the images the update uploaded
    /// * `kept` - The image the entity already has
    /// * `error` - The error the update failed with
    /// * `shared` - Shared Thorium objects
    async fn discard_uploads(
        images: &[String],
        kept: Option<&String>,
        error: ApiError,
        shared: &Shared,
    ) -> ApiError {
        // delete each uploaded image once, unless it was written over the kept one
        let mut deleted = HashSet::new();
        for key in images {
            // skip the kept image and images already deleted
            if kept == Some(key) || !deleted.insert(key) {
                continue;
            }
            // delete the image with the client directly
            if let Err(s3_error) = Self::delete_graphic(key, shared).await {
                return internal_err_unwrapped!(format!(
                    "Error cleaning up image after entity update error: {s3_error}. Original error: {error}"
                ));
            }
        }
        error
    }

    /// Delete the images a failed update uploaded, keeping the image the entity has now
    ///
    /// The update's copy of the entity was read before its tool's lock was taken, and another
    /// write may have given the entity an image under the same key since, so the entity is read
    /// again. Nothing is deleted if it can't be read.
    ///
    /// # Arguments
    ///
    /// * `id` - The id of the entity that was being updated
    /// * `images` - The keys of the images the update uploaded
    /// * `error` - The error the update failed with
    /// * `shared` - Shared Thorium objects
    async fn discard_uploads_keeping_current(
        id: Uuid,
        images: &[String],
        error: ApiError,
        shared: &Shared,
    ) -> ApiError {
        // read the image the entity has now, in every one of its groups
        let current = match db::entities::all_groups(id, shared).await {
            Ok(groups) => db::entities::get(&groups, id, shared)
                .await
                .map(|entity| entity.image),
            Err(read_error) => Err(read_error),
        };
        // keep the current image, or keep every upload if it can't be read
        match current {
            Ok(kept) => Self::discard_uploads(images, kept.as_ref(), error, shared).await,
            Err(read_error) if read_error.status() == StatusCode::NOT_FOUND => {
                Self::discard_uploads(images, None, error, shared).await
            }
            Err(_) => error,
        }
    }

    /// Apply a parsed update to an entity, deleting the images it uploaded if it fails
    ///
    /// Once the entity is written it may hold an uploaded image, so a failure after that keeps
    /// every upload.
    ///
    /// # Arguments
    ///
    /// * `user` - The user updating the entity
    /// * `parsed` - The update read from its form
    /// * `reauthorize` - Whether to check the user's rights against this entity first
    /// * `shared` - Shared Thorium objects
    async fn apply_update(
        mut self,
        user: &User,
        parsed: ParsedUpdate,
        reauthorize: bool,
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // keep the image the entity already has, which a failed update must never delete
        let original_image = self.image.clone();
        // keep the uploaded images to delete if the update fails
        let images = parsed.images.clone();
        // check the user's rights against this copy of the entity if asked to
        let authorized = if reauthorize {
            helpers::heap_future(|| self.authorize_update(user, shared)).await
        } else {
            Ok(())
        };
        // track whether the update wrote the entity before any failure
        let mut written = false;
        // apply the update once the user is authorized
        let result = match authorized {
            Ok(()) => {
                helpers::heap_future(|| self.update_helper(user, parsed, &mut written, shared))
                    .await
            }
            Err(error) => Err(error),
        };
        // delete the uploaded images if the update failed before writing the entity
        match result {
            Ok(()) => Ok(()),
            Err(error) if written => Err(error),
            Err(error) => {
                Err(Self::discard_uploads(&images, original_image.as_ref(), error, shared).await)
            }
        }
    }

    /// Set the last image an update uploaded as this entity's image
    ///
    /// Returns the images that are no longer needed once the update succeeds: the old image
    /// and any earlier uploads, unless the new image was written over them.
    ///
    /// # Arguments
    ///
    /// * `images` - The keys of the images the update uploaded, in the order they were sent
    fn replace_image(&mut self, images: &[String]) -> Vec<String> {
        // an update without images keeps the entity's image
        let Some((new, earlier)) = images.split_last() else {
            return Vec::new();
        };
        // replace the image, keeping the old one to delete
        let old = self.image.replace(new.clone());
        // delete the old image and earlier uploads once each, unless they're the new one
        let mut deletes: Vec<String> = Vec::new();
        for key in old.into_iter().chain(earlier.iter().cloned()) {
            // skip the new image and keys already queued for deletion
            if &key != new && !deletes.contains(&key) {
                deletes.push(key);
            }
        }
        deletes
    }

    /// A helper function for updating an entity by taking a parsed update, validating
    /// it, and submitting the changes to the database
    ///
    /// # Arguments
    ///
    /// * `user` - The user updating the entity
    /// * `parsed` - The update read from its form
    /// * `written` - Set once the entity itself is written
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "Entity::update_helper", skip_all, err(Debug))]
    #[allow(clippy::too_many_lines)]
    async fn update_helper(
        mut self,
        user: &User,
        parsed: ParsedUpdate,
        written: &mut bool,
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // split the update into its fields and the images it uploaded
        let ParsedUpdate {
            form: mut update_form,
            images,
        } = parsed;
        // track whether this update uploads a new image
        let image_uploaded = !images.is_empty();
        // set the new image, tracking any images we need to delete
        let mut deletes = self.replace_image(&images);
        // model configuration is authorized once we know whether only its bans change
        if models::is_model_kind(self.kind) {
            helpers::heap_future(|| {
                models::authorize_update(user, &self, &update_form, image_uploaded, shared)
            })
            .await?;
        }
        // keep the groups we could see before any group changes
        let original_groups = self.groups.clone();
        // refuse the changes tools and tool instances never take
        check_tool_update(self.kind, &update_form, image_uploaded)?;
        // authorize and apply any group changes
        let plan =
            helpers::heap_future(|| update_form.update_groups(user, &mut self, shared)).await?;
        // make sure the new name is valid
        if update_form.name.as_ref().is_some_and(String::is_empty) {
            return bad!("Entity cannot have an empty name!".to_string());
        }
        // a rename can't take the word for an entity kind, though an unchanged protected name stays
        if let Some(new_name) = update_form.name.as_ref()
            && matches!(
                self.kind,
                EntityKinds::Tool | EntityKinds::ToolSet | EntityKinds::WorkflowCatalog
            )
            && *new_name != self.name
        {
            crate::utils::bounder::entity_name(new_name)?;
        }
        // update the entity's name
        update!(self.name, update_form.name);
        // update the entity's other data
        update_opt!(self.description, update_form.description);
        update_clear_opt!(self.description, update_form.clear_description);
        // keep the graph settings from before the update to see if they change
        let graph_before = GraphSettings::of(&self.metadata);
        // update our entity metadata, getting what a model update needs done once it's written
        let model_update = if models::is_model_kind(self.kind) {
            let applied = helpers::heap_future(|| {
                models::update_meta(user, &mut self, update_form.metadata, shared)
            })
            .await?;
            Some(applied)
        } else {
            helpers::heap_future(|| self.update_meta(user, update_form.metadata, shared)).await?;
            None
        };
        // any edit of a tool marks it modified
        if let EntityMetadata::Tool(tool) = &mut self.metadata {
            tool.modified = Utc::now();
        }
        // find the change to queue in every group of the entity if its graph settings changed;
        // a toolset's group change changes what its selector matches
        let groups_changed =
            !update_form.add_groups.is_empty() || !update_form.remove_groups.is_empty();
        let graph_change = GraphSettings::changed(graph_before.as_ref(), &self.metadata, self.id)
            .or_else(|| {
                (self.kind == EntityKinds::ToolSet && groups_changed)
                    .then_some(WorkflowGraphChange::ToolSetChanged { tool_set: self.id })
            })
            .map(|change| (change, self.groups.clone()));
        // clear our entities image if clear image is set
        if update_form.clear_image == Some(true) {
            // add our current image to our deletes by taking it from the entity
            if let Some(image_path) = self.image.take() {
                deletes.push(image_path);
            }
        }
        // get what our edges need before the entity is consumed by the update
        let source = self.build_association_target_column();
        let source_extra = Some(self.name.clone());
        // get the tool whose latest a tool instance change may move
        let instance_tool = match &self.metadata {
            EntityMetadata::ToolInstance(instance) => Some(instance.tool),
            _ => None,
        };
        // add the network policies a widened tool needs to its new groups
        helpers::heap_future(|| tools::add_policies(user, plan.policy_adds, shared)).await?;
        // keep the entity's id so a test can inject a failure once it's written
        #[cfg(feature = "test-utilities")]
        let id = self.id;
        // fail here when a test asked for a failure before the entity is written
        #[cfg(feature = "test-utilities")]
        if crate::test_utilities::faults::take_entity_update_failure(id) {
            return internal_err!(format!("injected failure before updating entity {id}"));
        }
        // wait here when a test asked to hold the update right before the entity is written
        #[cfg(feature = "test-utilities")]
        crate::test_utilities::faults::wait_if_entity_update_paused(id).await;
        // keep the written model entity for what its update needs done afterwards
        let model_written = model_update.as_ref().map(|_| self.clone());
        // update this entity
        helpers::heap_future(|| {
            db::entities::update(
                user,
                self,
                &update_form.add_groups,
                &update_form.remove_groups,
                shared,
            )
        })
        .await?;
        // the entity now holds any uploaded image, so failures from here on keep the uploads
        *written = true;
        // fail here when a test asked for a failure after the entity is written
        #[cfg(feature = "test-utilities")]
        if crate::test_utilities::faults::take_entity_write_failure(id) {
            return internal_err!(format!("injected failure after writing entity {id}"));
        }
        // a root's instances follow its group change
        helpers::heap_future(|| {
            Self::change_instance_groups(
                user,
                plan.instances,
                &update_form.add_groups,
                &update_form.remove_groups,
                shared,
            )
        })
        .await?;
        // make this entity's edges follow its group changes
        if let Some(source) = source {
            helpers::heap_future(|| {
                Self::follow_group_changes(
                    user,
                    &source,
                    source_extra,
                    &original_groups,
                    &update_form.add_groups,
                    &update_form.remove_groups,
                    PublishedFromEdges::Follow,
                    shared,
                )
            })
            .await?;
        }
        // finish a model update: move its reference, notify its ban changes, and queue refreshes
        if let (Some(applied), Some(written)) = (model_update, model_written) {
            helpers::heap_future(|| models::finish_update(user, &written, applied, shared)).await?;
        }
        // a deprecation may move the tool's latest, under the lock this update holds
        if let Some(tool) = instance_tool {
            helpers::heap_future(|| tools::refresh_latest(user, tool, shared)).await?;
        }
        // queue the change of the entity's graph settings once every write is done
        if let Some((change, groups)) = graph_change {
            Event::queue_workflow_graph(user, &groups, change, shared).await?;
        }
        // delete any graphics that are no longer needed
        helpers::heap_future(|| Self::delete_graphics(deletes, shared)).await
    }

    /// Delete graphics that are no longer needed, up to ten at a time
    ///
    /// # Arguments
    ///
    /// * `keys` - The keys of the graphics to delete
    /// * `shared` - Shared Thorium objects
    async fn delete_graphics(keys: Vec<String>, shared: &Shared) -> Result<(), ApiError> {
        // delete every graphic, failing if any delete failed
        stream::iter(keys)
            .map(|key| async move { Self::delete_graphic(&key, shared).await })
            .buffer_unordered(10)
            .collect::<Vec<Result<(), ApiError>>>()
            .await
            .into_iter()
            .collect::<Result<(), ApiError>>()
    }

    /// Update an `Entity`
    ///
    /// An update of a tool or one of its instances reads its form and uploads any image before
    /// taking the tool's lock, then applies to the entity as it is once the lock is held.
    ///
    /// # Arguments
    ///
    /// * `user` - The user updating the entity
    /// * `scoped` - Whether the user authenticated with a scoped token
    /// * `form` - The update to apply
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "Entity::update", skip_all, err(Debug))]
    pub async fn update(
        mut self,
        user: &User,
        scoped: bool,
        form: Multipart,
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // only a full token rotates a model credential
        refuse_scoped_credential_write(self.kind, scoped, "change")?;
        // only writes to tools and their instances take a lock
        let Some(root) = tools::lock_root(&self) else {
            return helpers::heap_future(|| self.update_unlocked(user, scoped, form, shared)).await;
        };
        // refuse a user who can't edit the entity before waiting on its lock
        helpers::heap_future(|| self.authorize_update(user, shared)).await?;
        // read the form before taking the lock, so it's never held while a body arrives
        let parsed = helpers::heap_future(|| self.parse_update(user, scoped, form, shared)).await?;
        // wait for any other write to the tool
        let token = match tools::lock(root, &self.name, shared).await {
            Ok(token) => token,
            Err(error) => {
                return Err(Self::discard_uploads_keeping_current(
                    self.id,
                    &parsed.images,
                    error,
                    shared,
                )
                .await);
            }
        };
        // reread the entity now that no other write to its tool can run, then authorize and
        // apply the update against it
        let result = match Self::get(user, self.id, shared).await {
            Ok(fresh) => {
                helpers::heap_future(|| fresh.apply_update(user, parsed, true, shared)).await
            }
            Err(error) => {
                Err(
                    Self::discard_uploads_keeping_current(self.id, &parsed.images, error, shared)
                        .await,
                )
            }
        };
        // release the lock whether or not the update worked
        tools::unlock_after_write(root, &token, shared).await;
        result
    }

    /// Check that a user may edit this entity, the way its kind's groups are kept
    ///
    /// A shared definition is loaded in all of its groups first.
    ///
    /// # Arguments
    ///
    /// * `user` - The user updating the entity
    /// * `shared` - Shared Thorium objects
    async fn authorize_update(&mut self, user: &User, shared: &Shared) -> Result<(), ApiError> {
        // check the user's rights the way this kind's groups are kept
        match self.kind.group_policy() {
            GroupPolicy::Contained => {
                // a shared definition is edited in every one of its groups
                self.load_all_groups(shared).await?;
                // model configuration is authorized once its update is parsed, since an update
                // of only its bans needs different rights
                if !models::is_model_kind(self.kind) {
                    // the user must be able to edit it in all of them
                    Group::authorize_graph(
                        user,
                        &self.groups,
                        Group::editable,
                        Some(GroupAllowAction::Entities),
                        "edit",
                        &self.name,
                        shared,
                    )
                    .await?;
                }
            }
            GroupPolicy::Overlap => {
                // validate that this user can edit this entity in all requested groups
                let _ = Group::authorize_check_allow_all(
                    user,
                    &self.groups,
                    Group::editable,
                    "edit",
                    Some(GroupAllowAction::Entities),
                    shared,
                )
                .await?;
            }
        }
        Ok(())
    }

    /// Update an `Entity` whose writes take no lock
    ///
    /// # Arguments
    ///
    /// * `user` - The user updating the entity
    /// * `scoped` - Whether the user authenticated with a scoped token
    /// * `form` - The update to apply
    /// * `shared` - Shared Thorium objects
    async fn update_unlocked(
        mut self,
        user: &User,
        scoped: bool,
        form: Multipart,
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // check the user's rights the way this kind's groups are kept
        helpers::heap_future(|| self.authorize_update(user, shared)).await?;
        // read the form, uploading any image it sends
        let parsed = helpers::heap_future(|| self.parse_update(user, scoped, form, shared)).await?;
        // apply the update
        helpers::heap_future(|| self.apply_update(user, parsed, false, shared)).await
    }

    /// Delete all associations for this
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is deleting associations
    /// * `shared` - Shared Thorium objects
    pub(crate) async fn delete_associations(
        &self,
        user: &User,
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // build the associations list opts for this entity
        let opts = AssociationListOpts::default()
            .groups(self.groups.clone())
            .limit(500);
        // build the source target for this entity if we have one
        if let Some(source) = self.build_association_target_column() {
            // list all associations for this entity scoped to the requesting user
            let mut cursor = db::associations::list(user, opts, &source, shared).await?;
            // step over our associations and delete them
            loop {
                // delete this page of associations, queuing a change for each graph edge
                db::associations::delete_many(&source, &cursor.data, shared).await?;
                edges::queue_removed_edges(user, self.id, &cursor.data, shared).await?;
                // check if this cursor has been exhausted
                if cursor.exhausted() {
                    break;
                }
                // clear our current cursor
                cursor.data.clear();
                // get the next page of data
                cursor.next(shared).await?;
            }
        }
        Ok(())
    }

    /// Delete an `Entity`
    ///
    /// Deleting a tool or one of its instances holds the tool's lock and deletes the entity as it
    /// is once the lock is held.
    ///
    /// # Arguments
    ///
    /// * `user` - The user deleting an entity
    /// * `scoped` - Whether the user used a scoped token, which acts only in its own groups
    /// * `shared` - Shared Thorium objects
    pub async fn delete(
        mut self,
        user: &User,
        scoped: bool,
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // only a full token deletes a model credential
        refuse_scoped_credential_write(self.kind, scoped, "delete")?;
        // only writes to tools and their instances take a lock
        let Some(root) = tools::lock_root(&self) else {
            return Box::pin(self.delete_unlocked(user, scoped, shared)).await;
        };
        // refuse a user who can't delete the entity before waiting on its lock
        self.authorize_delete(user, scoped, shared).await?;
        // wait for any other write to the tool
        let token = tools::lock(root, &self.name, shared).await?;
        // reread the entity now that no other write to its tool can run, then delete it
        let result = match Self::get(user, self.id, shared).await {
            Ok(fresh) => Box::pin(fresh.delete_unlocked(user, scoped, shared)).await,
            Err(err) => Err(err),
        };
        // release the lock whether or not the delete worked
        tools::unlock_after_write(root, &token, shared).await;
        result
    }

    /// Check that a user may delete this entity, the way its kind's groups are kept
    ///
    /// A shared definition is loaded in all of its groups first.
    ///
    /// # Arguments
    ///
    /// * `user` - The user deleting the entity
    /// * `scoped` - Whether the user used a scoped token, which acts only in its own groups
    /// * `shared` - Shared Thorium objects
    async fn authorize_delete(
        &mut self,
        user: &User,
        scoped: bool,
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // check the user's rights the way this kind's groups are kept
        match self.kind.group_policy() {
            GroupPolicy::Contained => {
                // a shared definition is deleted from every one of its groups
                self.load_all_groups(shared).await?;
                // a scoped token acts only in its own groups, so it can't delete a definition that
                // is also in a group outside them, even one it submitted
                if scoped && self.groups.iter().any(|group| !user.groups.contains(group)) {
                    return unauthorized!(format!(
                        "Cannot delete {} with a token limited to fewer groups than it is in",
                        self.name
                    ));
                }
                // admins can always delete it, the submitter can unless it's a model endpoint or
                // credential, and anyone else must manage or own every one of its groups
                let submitter_may = !models::needs_manager(self.kind);
                if !user.is_admin() && (!submitter_may || self.submitter != user.username) {
                    Group::authorize_graph(
                        user,
                        &self.groups,
                        |group: &Group, user: &User| {
                            // only managers and owners may delete others' data
                            if group.is_manager_or_owner(&user.username) {
                                Ok(())
                            } else {
                                unauthorized!()
                            }
                        },
                        None,
                        "delete",
                        &self.name,
                        shared,
                    )
                    .await?;
                }
            }
            GroupPolicy::Overlap => {
                // make sure we can delete this entity from all of the groups we can see it in
                self.authorize_removal(user, &self.groups, shared).await?;
            }
        }
        Ok(())
    }

    /// Delete an `Entity` once any lock its writes need is held
    ///
    /// # Arguments
    ///
    /// * `user` - The user deleting an entity
    /// * `scoped` - Whether the user used a scoped token, which acts only in its own groups
    /// * `shared` - Shared Thorium objects
    async fn delete_unlocked(
        mut self,
        user: &User,
        scoped: bool,
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // check the user's rights the way this kind's groups are kept
        self.authorize_delete(user, scoped, shared).await?;
        // a shared definition in use can't be deleted
        if self.kind.group_policy() == GroupPolicy::Contained {
            // a tool's instances are deleted before it
            if self.kind == EntityKinds::Tool {
                tools::check_delete(user, &self, shared).await?;
            }
            // referenced model configuration can't be deleted
            models::check_delete(user, &self, shared).await?;
            // a definition that other entities, pipelines, or tool specs reference can't be deleted
            references::check_delete(user, &self, shared).await?;
        }
        // ban the assumed roles whose source access key is being deleted
        if self.kind == EntityKinds::ModelCredential {
            Box::pin(models::ban_dependents(user, &self, shared)).await?;
        }
        // remove any associations for this entity
        self.delete_associations(user, shared).await?;
        // delete the entity
        db::entities::delete(user, &self, shared).await?;
        // fail here when a test asked for a failure after the entity is removed
        #[cfg(feature = "test-utilities")]
        if crate::test_utilities::faults::take_entity_delete_failure(self.id) {
            return internal_err!(format!(
                "injected failure after deleting entity {}",
                self.id
            ));
        }
        // a deleted model entity's ban notifications go with it
        if models::is_model_kind(self.kind) {
            models::delete_notifications(&self, shared).await?;
        }
        // deleting a tool instance may move its tool's latest, and deleting a workflow instance
        // its workflow's newest and latest, under the lock this delete holds
        match &self.metadata {
            EntityMetadata::ToolInstance(instance) => {
                tools::refresh_latest(user, instance.tool, shared).await?;
                // the instance's stored runtime goes with it
                db::jobs::remove_workload_runtime(self.id, shared).await?;
            }
            EntityMetadata::WorkflowInstance(instance) => {
                workflows::refresh_instances(user, instance.workflow, self.id, shared).await?;
            }
            _ => (),
        }
        // delete this entities image if one exists
        if let Some(s3_path) = &self.image {
            // delete our image graphic
            Self::delete_graphic(s3_path, shared).await?;
        }
        Ok(())
    }

    /// Drop the groups a reader can't see from the snapshot groups this entity records
    ///
    /// This covers the toolset instances nested in an agent and the agents nested in a workflow
    /// too. Only what a read returns is filtered; `Entity::get` leaves the snapshots whole, since
    /// updates write back what it reads.
    ///
    /// # Arguments
    ///
    /// * `user` - The user reading this entity
    pub fn hide_snapshot_groups(&mut self, user: &User) {
        // toolset and workflow instances record the groups of what they snapshot
        toolsets::hide_snapshot_groups(self, user);
        workflows::hide_snapshot_groups(self, user);
        // agents carry the toolset instances they call, and workflows carry their agents
        match &mut self.metadata {
            EntityMetadata::Agent(agent) => {
                for tool_set in &mut agent.tool_sets {
                    tool_set.hide_snapshot_groups(user);
                }
            }
            EntityMetadata::WorkflowCatalog(workflow) => {
                for agent in &mut workflow.agents {
                    agent.hide_snapshot_groups(user);
                }
            }
            _ => (),
        }
    }

    /// Refuse a tag deletion that removes a tag Thorium sets on a shared definition
    ///
    /// The intrinsic tags of `Contained` kinds identify their versions and roots, so users can
    /// only remove their own tags from them.
    ///
    /// # Arguments
    ///
    /// * `req` - The tag deletion to check
    pub fn protect_intrinsic_tags(
        &self,
        req: &crate::models::TagDeleteRequest<Entity>,
    ) -> Result<(), ApiError> {
        // only contained kinds have intrinsic tags to protect
        if self.kind.group_policy() != GroupPolicy::Contained {
            return Ok(());
        }
        // build the tags Thorium sets on this entity
        let mut intrinsic = HashMap::default();
        self.populate_intrinsic_tags(&mut intrinsic)?;
        // refuse removing any of them
        for (key, values) in &req.tags {
            // check each value being removed under this key
            if let Some(protected) = intrinsic.get(key)
                && values.iter().any(|value| protected.contains(value))
            {
                return bad!(format!(
                    "the `{key}` tag is set by Thorium and can't be removed"
                ));
            }
        }
        Ok(())
    }

    /// Refuse user tags under the keys Thorium sets on an instance kind
    ///
    /// Toolset and workflow instances are found, deduplicated, and followed through their
    /// intrinsic tags, so a user's own tags can't use those keys in any case.
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of entity being tagged
    /// * `keys` - The keys of the user's tags
    pub fn refuse_intrinsic_tag_keys<'a>(
        kind: EntityKinds,
        keys: impl IntoIterator<Item = &'a String>,
    ) -> Result<(), ApiError> {
        // only toolset and workflow instances have keys users can't set
        let reserved: &[&str] = match kind {
            EntityKinds::ToolSetInstance => &toolsets::INTRINSIC_TAG_KEYS,
            EntityKinds::WorkflowInstance => &workflows::INTRINSIC_TAG_KEYS,
            _ => return Ok(()),
        };
        // refuse the first key that matches a reserved one in any case
        if let Some(key) = keys
            .into_iter()
            .find(|key| reserved.iter().any(|known| known.eq_ignore_ascii_case(key)))
        {
            return bad!(format!(
                "the `{key}` tag is set by Thorium on a {kind}, so it can't be set by users"
            ));
        }
        Ok(())
    }

    /// Load every group of an entity and its tags in all of them
    ///
    /// Writes to a `Contained` entity act on every copy, including the ones in groups the user
    /// can't see.
    ///
    /// # Arguments
    ///
    /// * `shared` - Shared Thorium objects
    async fn load_all_groups(&mut self, shared: &Shared) -> Result<(), ApiError> {
        // get every group this entity is in
        self.groups = db::entities::all_groups(self.id, shared).await?;
        // reload its tags in all of those groups
        self.tags.clear();
        let groups = self.groups.clone();
        self.get_tags(&groups, shared).await
    }

    /// Apply a root's group change to each of its instances
    ///
    /// Each instance's rows and tags are written in the added groups and removed from the
    /// removed ones, and its edges follow, except the edges to its root, which follow the root.
    ///
    /// # Arguments
    ///
    /// * `user` - The user changing the root's groups
    /// * `instances` - The root's instances, in all of their groups
    /// * `add_groups` - The groups added to the root
    /// * `remove_groups` - The groups removed from the root
    /// * `shared` - Shared Thorium objects
    async fn change_instance_groups(
        user: &User,
        instances: Vec<Entity>,
        add_groups: &[String],
        remove_groups: &[String],
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // change each instance's groups
        for mut instance in instances {
            // keep the instance's groups before the change
            let original_groups = instance.groups.clone();
            // add the groups the instance doesn't have yet
            let added = helpers::missing_groups(add_groups, &instance.groups);
            instance.groups.extend(added.iter().cloned());
            // drop the removed groups
            instance
                .groups
                .retain(|group| !remove_groups.contains(group));
            // get what the instance's edges need before it's consumed by the update
            let source = AssociationTargetColumn::Entity(instance.id);
            let source_extra = Some(instance.name.clone());
            // write the instance in its new groups
            helpers::heap_future(|| {
                db::entities::update(user, instance, &added, remove_groups, shared)
            })
            .await?;
            // make the instance's other edges follow its group change
            helpers::heap_future(|| {
                Self::follow_group_changes(
                    user,
                    &source,
                    source_extra,
                    &original_groups,
                    &added,
                    remove_groups,
                    PublishedFromEdges::Skip,
                    shared,
                )
            })
            .await?;
        }
        Ok(())
    }

    /// Check whether a user may add groups to a root, as a group add on it would need
    ///
    /// A group add needs edit rights, with entities allowed, in every one of the root's groups
    /// and in each added group.
    ///
    /// # Arguments
    ///
    /// * `user` - The user adding the groups
    /// * `root` - The root, in every one of its groups
    /// * `add` - The groups the root would gain
    /// * `shared` - Shared Thorium objects
    async fn may_add_groups(
        user: &User,
        root: &Entity,
        add: &[String],
        shared: &Shared,
    ) -> Result<bool, ApiError> {
        // read every group the add touches
        let names = root
            .groups
            .iter()
            .chain(add)
            .cloned()
            .collect::<HashSet<String>>();
        let groups = db::groups::list_details(names.iter(), shared).await?;
        // a group that doesn't exist blocks the add
        if groups.len() < names.len() {
            return Ok(false);
        }
        // the user must be able to edit entities in every one of them
        Ok(Group::check_graph(
            user,
            &groups,
            Group::editable,
            Some(GroupAllowAction::Entities),
            "edit",
            &root.name,
        )
        .is_ok())
    }

    /// Check that a user may widen an edge target into more groups, before anything is written
    ///
    /// A tool instance widens its tool, and the tool's instances follow it. The user must hold
    /// the rights a group add on the root needs, a tool's network policies must be available in
    /// the added groups, and an agent's own edges must stay contained. Returns no plan when the
    /// user may not add the groups to the root.
    ///
    /// # Arguments
    ///
    /// * `user` - The user widening the target
    /// * `target` - The edge target that misses some groups
    /// * `add` - The groups the target must gain
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "Entity::check_widening", skip_all, fields(target = target.id.to_string()), err(Debug))]
    pub(crate) async fn check_widening(
        user: &User,
        target: &Entity,
        add: &[String],
        shared: &Shared,
    ) -> Result<Option<WideningPlan>, ApiError> {
        // a tool instance's groups follow its tool, so the tool is widened
        let root_id = match &target.metadata {
            EntityMetadata::ToolInstance(instance) => instance.tool,
            EntityMetadata::ToolSetInstance(instance) => instance.tool_set,
            EntityMetadata::WorkflowInstance(instance) => instance.workflow,
            _ => target.id,
        };
        // read the root in every one of its groups with its tags
        let groups = db::entities::all_groups(root_id, shared).await?;
        let mut root = helpers::heap_future(|| db::entities::get(&groups, root_id, shared)).await?;
        helpers::heap_future(|| root.load_all_groups(shared)).await?;
        // only add the groups the root is missing
        let add = helpers::missing_groups(add, &root.groups);
        // the user must hold the rights a group add on the root needs
        if !add.is_empty() && !Self::may_add_groups(user, &root, &add, shared).await? {
            return Ok(None);
        }
        let mut plan = WideningPlan {
            root,
            add,
            instances: Vec::new(),
            policy_adds: Vec::new(),
        };
        // check what the root's kind needs in the added groups
        match plan.root.kind {
            EntityKinds::Tool => {
                // a tool's instances follow it, and their policies must be available
                plan.instances =
                    helpers::heap_future(|| tools::instances_for_change(user, &plan.root, shared))
                        .await?;
                plan.policy_adds = helpers::heap_future(|| {
                    tools::check_policies(user, &plan.root, &plan.instances, &plan.add, shared)
                })
                .await?;
            }
            EntityKinds::Agent => {
                // an agent's own edges must stay contained in its new groups
                helpers::heap_future(|| {
                    agents::check_group_change(user, &plan.root, &plan.add, &[], &[], shared)
                })
                .await?;
            }
            EntityKinds::ToolSet => {
                // a toolset's instances follow it, and its includes must stay contained
                plan.instances = helpers::heap_future(|| {
                    toolsets::instances(user, plan.root.id, &plan.root.groups, shared)
                })
                .await?;
                helpers::heap_future(|| {
                    toolsets::check_group_change(
                        user,
                        &plan.root,
                        &plan.instances,
                        &plan.add,
                        &[],
                        &[],
                        shared,
                    )
                })
                .await?;
            }
            _ => (),
        }
        Ok(Some(plan))
    }

    /// Write a checked widening: the root, its instances, and their edges gain the groups, and a
    /// widened toolset queues its change
    ///
    /// # Arguments
    ///
    /// * `user` - The user widening the target
    /// * `plan` - The checked widening
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "Entity::widen", skip_all, err(Debug))]
    pub(crate) async fn widen(
        user: &User,
        plan: WideningPlan,
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // a root already in every group has nothing to write
        let WideningPlan {
            mut root,
            add,
            instances,
            policy_adds,
        } = plan;
        if add.is_empty() {
            return Ok(());
        }
        // add the network policies a widened tool needs to its new groups
        helpers::heap_future(|| tools::add_policies(user, policy_adds, shared)).await?;
        // keep what the root's edges need before it's consumed by the write
        let original_groups = root.groups.clone();
        let source = AssociationTargetColumn::Entity(root.id);
        let source_extra = Some(root.name.clone());
        // write the root in its new groups, keeping a widened toolset's groups for its change
        root.groups.extend(add.iter().cloned());
        let widened_tool_set =
            (root.kind == EntityKinds::ToolSet).then(|| (root.id, root.groups.clone()));
        helpers::heap_future(|| db::entities::update(user, root, &add, &[], shared)).await?;
        // the root's instances follow it
        helpers::heap_future(|| Self::change_instance_groups(user, instances, &add, &[], shared))
            .await?;
        // the root's edges follow it into the groups their other ends are in
        helpers::heap_future(|| {
            Self::follow_group_changes(
                user,
                &source,
                source_extra,
                &original_groups,
                &add,
                &[],
                PublishedFromEdges::Follow,
                shared,
            )
        })
        .await?;
        // a widened toolset's selector can match tools in its new groups, so queue its change
        if let Some((tool_set, groups)) = widened_tool_set {
            let change = WorkflowGraphChange::ToolSetChanged { tool_set };
            Event::queue_workflow_graph(user, &groups, change, shared).await?;
        }
        Ok(())
    }

    /// Make sure a user may remove this entity from some groups
    ///
    /// Removing an entity from a group, by a delete or a group change, needs the submitter, an
    /// admin, or a manager or owner of each group.
    ///
    /// # Arguments
    ///
    /// * `user` - The user removing this entity
    /// * `groups` - The groups this entity is being removed from
    /// * `shared` - Shared Thorium objects
    pub(crate) async fn authorize_removal(
        &self,
        user: &User,
        groups: &[String],
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // the submitter and admins can remove this entity from any group
        if self.submitter == user.username || user.is_admin() {
            return Ok(());
        }
        // get the details for the groups this entity is being removed from
        let details = db::groups::list_details(groups.iter(), shared).await?;
        // make sure we are a manager or owner in each of them
        for group in details {
            // refuse removal from a group we don't manage or own
            if !group.is_manager_or_owner(&user.username) {
                return unauthorized!(format!("Cannot delete data from group {}", group.name));
            }
        }
        Ok(())
    }

    /// Get the groups the other end of an association is in
    ///
    /// Entities are checked in every group, including ones the user can't see; data that no
    /// longer exists is in no group.
    ///
    /// # Arguments
    ///
    /// * `user` - The user changing an entity's groups
    /// * `other` - The other end of the association
    /// * `shared` - Shared Thorium objects
    async fn other_end_groups(
        user: &User,
        other: &AssociationTargetColumn,
        shared: &Shared,
    ) -> Result<Vec<String>, ApiError> {
        // get the groups for this kind of data
        let groups = match other {
            AssociationTargetColumn::Entity(id) => db::entities::all_groups(*id, shared).await,
            AssociationTargetColumn::File(sha256) => Sample::get(user, sha256, shared)
                .await
                .map(|sample| sample.groups().into_iter().map(ToOwned::to_owned).collect()),
            AssociationTargetColumn::Repo(url) => Repo::get(user, url, shared)
                .await
                .map(|repo| repo.groups().into_iter().map(ToOwned::to_owned).collect()),
        };
        // data that can't be found is in no group
        match groups {
            Err(err) if err.status() == StatusCode::NOT_FOUND => Ok(Vec::new()),
            groups => groups,
        }
    }

    /// Make an entity's edges follow its group changes
    ///
    /// Each added group that allows associations gets a copy of the entity's associations whose
    /// other end is also in it, created at the change and submitted by the user adding the
    /// group, and the entity's associations in each removed group are deleted. An added group
    /// that disallows associations gets no copies, since nobody may write associations there.
    ///
    /// # Arguments
    ///
    /// * `user` - The user changing the entity's groups
    /// * `source` - The entity's association target column
    /// * `source_extra` - The extra info stored with the entity's side of its associations
    /// * `original_groups` - The groups the entity was in before the change
    /// * `add_groups` - The groups added to the entity
    /// * `remove_groups` - The groups removed from the entity
    /// * `published_from` - How the entity's `PublishedFrom` edges follow the change
    /// * `shared` - Shared Thorium objects
    #[allow(clippy::too_many_arguments)]
    #[instrument(name = "Entity::follow_group_changes", skip_all, err(Debug))]
    async fn follow_group_changes(
        user: &User,
        source: &AssociationTargetColumn,
        source_extra: Option<String>,
        original_groups: &[String],
        add_groups: &[String],
        remove_groups: &[String],
        published_from: PublishedFromEdges,
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // edges Thorium writes between an instance and its root may go in every added group
        let all_added = add_groups.to_vec();
        // only added groups that allow associations may hold copies of our other edges
        let add_groups = if add_groups.is_empty() {
            Vec::new()
        } else {
            db::groups::list_details(add_groups.iter(), shared)
                .await?
                .into_iter()
                .filter(|group| group.allowed.is_allowed(GroupAllowAction::Associations))
                .map(|group| group.name)
                .collect::<Vec<String>>()
        };
        // copy our edges into any added groups that hold their other end
        if !all_added.is_empty() {
            // the copied rows are created at the time of this group change
            let now = Utc::now();
            // list this entity's associations in the groups it was already in
            let opts = AssociationListOpts::default()
                .groups(original_groups.to_vec())
                .limit(500);
            let mut cursor = db::associations::list(user, opts, source, shared).await?;
            // step over every page of associations
            loop {
                // copy each association into the added groups its other end is in
                for assoc in &cursor.data {
                    // pick the added groups this edge may be copied into
                    let candidates = match (assoc.kind, published_from) {
                        (
                            AssociationKind::PublishedFrom | AssociationKind::BuiltFrom,
                            PublishedFromEdges::Follow,
                        )
                        | (
                            AssociationKind::AuthenticatesWith
                            | AssociationKind::Serves
                            | AssociationKind::Provides,
                            _,
                        ) => &all_added,
                        (
                            AssociationKind::PublishedFrom | AssociationKind::BuiltFrom,
                            PublishedFromEdges::Skip,
                        ) => continue,
                        _ => &add_groups,
                    };
                    // get the groups the other end is in
                    let other = assoc.deserialize_other()?;
                    let other_groups = Self::other_end_groups(user, &other, shared).await?;
                    // copy this association into the added groups both ends are in
                    let groups = helpers::intersection(candidates, &other_groups);
                    if !groups.is_empty() {
                        db::associations::copy_to_groups(
                            &user.username,
                            now,
                            source,
                            source_extra.clone(),
                            assoc,
                            &groups,
                            shared,
                        )
                        .await?;
                    }
                }
                // stop once this cursor has been exhausted
                if cursor.exhausted() {
                    break;
                }
                // clear our current page and get the next one
                cursor.data.clear();
                cursor.next(shared).await?;
            }
        }
        // delete our edges from any removed groups
        if !remove_groups.is_empty() {
            // list this entity's associations in the removed groups
            let opts = AssociationListOpts::default()
                .groups(remove_groups.to_vec())
                .limit(500);
            let mut cursor = db::associations::list(user, opts, source, shared).await?;
            // step over every page of associations
            loop {
                // delete this page of associations from the removed groups
                db::associations::delete_many(source, &cursor.data, shared).await?;
                // stop once this cursor has been exhausted
                if cursor.exhausted() {
                    break;
                }
                // clear our current page and get the next one
                cursor.data.clear();
                cursor.next(shared).await?;
            }
        }
        Ok(())
    }

    /// Ensure that user has group privileges up to the given `role_check` and
    /// that all the groups allow the given `action`
    ///
    /// # Arguments
    ///
    /// * `user` - The user requesting the action
    /// * `groups` - The entity groups for which the action was requested
    /// * `role_check` - The function used to check for the user's role/privileges in the groups
    /// * `role_check_name` - The name of the role check to use in logging (i.e. "view"/"edit")
    /// * `action` - The action to check in each group if one is given
    /// * `shared` - Shared Thorium objects
    pub async fn validate_check_allow_groups<F>(
        &self,
        user: &User,
        groups: &mut Vec<String>,
        role_check: F,
        role_check_name: &str,
        action: Option<GroupAllowAction>,
        shared: &Shared,
    ) -> Result<(), ApiError>
    where
        F: Fn(&Group, &User) -> Result<(), ApiError>,
    {
        if groups.is_empty() {
            // the user specified no groups, so default to ones
            // that pass the privilege check function and allow the given action;
            // first check that we have access to all of the groups we have
            let group_objs = Group::authorize_all(user, &self.groups, shared).await?;
            if let Some(action) = action {
                groups.extend(
                    group_objs
                        .into_iter()
                        .filter(|group| group.allowable(action).is_ok())
                        .filter(|group| role_check(group, user).is_ok())
                        .map(|group| group.name),
                );
            } else {
                groups.extend(
                    group_objs
                        .into_iter()
                        .filter(|group| role_check(group, user).is_ok())
                        .map(|group| group.name),
                );
            }
        } else {
            // make sure the entity is in all the given groups
            if !groups.iter().all(|group| self.groups.contains(group)) {
                return unauthorized!(format!(
                    "Entity '{}' is not in all specified groups",
                    self.name
                ));
            }
            // make sure we have access in all requested groups
            let _ = Group::authorize_check_allow_all(
                user,
                groups,
                role_check,
                role_check_name,
                action,
                shared,
            )
            .await?;
        }
        // make sure we got at least some groups
        if groups.is_empty() {
            return unauthorized!(format!(
                "The user does not have permissions to {role_check_name} \
                    entities in any of the given groups!"
            ));
        }
        // all groups valid
        Ok(())
    }

    /// Add an [`EntityRow`] to an existing entity
    ///
    /// # Arguments
    ///
    /// * `row` - The row to add
    pub(super) fn add_row(&mut self, row: EntityRow) {
        // add this row's group to the list
        self.groups.push(row.group);
    }

    /// Drop any data that is built based on associations instead of statically
    ///
    /// We do this because we don't want to write this to scylla as it is built dynamically at retrieval time
    pub(super) fn drop_associated_data(&mut self) {
        match &mut self.metadata {
            // vendors in devices is built by association
            EntityMetadata::Device(device) => device.vendors.clear(),
            // a tool's instances are built from their edges
            EntityMetadata::Tool(tool) => tool.instances.clear(),
            // a harness instance's native tools are built from its edges
            EntityMetadata::ToolInstance(instance) => instance.provides.clear(),
            // an agent's harness, tools, toolsets, and sub-agents are built from its edges
            EntityMetadata::Agent(agent) => agent.clear_edges(),
            // a toolset's tools are built from its edges and its latest instance is looked up
            EntityMetadata::ToolSet(tool_set) => tool_set.clear_populated(),
            // a workflow's agents are built from its edges
            EntityMetadata::WorkflowCatalog(workflow) => workflow.clear_populated(),
            // most other entities have no association specific data
            EntityMetadata::Vendor(_)
            | EntityMetadata::Collection(_)
            | EntityMetadata::FileSystem(_)
            | EntityMetadata::Folder(_)
            | EntityMetadata::WindowsProcessTree(_)
            | EntityMetadata::WindowsProcess(_)
            | EntityMetadata::PeSection(_)
            | EntityMetadata::PeImport(_)
            | EntityMetadata::NetworkConnection(_)
            | EntityMetadata::SigmaRule(_)
            | EntityMetadata::Flag(_)
            | EntityMetadata::Incident(_)
            | EntityMetadata::CompiledFunction(_)
            | EntityMetadata::DecompiledFunction(_)
            | EntityMetadata::ToolSetInstance(_)
            | EntityMetadata::WorkflowInstance(_)
            | EntityMetadata::ModelEndpoint(_)
            | EntityMetadata::ModelProfile(_)
            | EntityMetadata::ModelCredential(_)
            | EntityMetadata::Other => (),
        }
    }
}

// Add graphic support for entities
impl GraphicSupport for Entity {
    /// A unique, immutable key to use to reference the implementing object
    #[cfg(feature = "api")]
    type GraphicKey<'a> = &'a Uuid;

    /// Build the base path for this graphic
    ///
    /// # Arguments
    ///
    /// * `key` - The id of the entity this graphic belongs to
    fn build_graphic_base_path(key: Self::GraphicKey<'_>) -> PathBuf {
        // entities just use their id as their key
        PathBuf::from(key.to_string())
    }

    /// Build the base path for this graphic
    ///
    /// # Arguments
    ///
    /// * `_shared` - Shared Thorium objects
    fn build_graphic_base_path_from_self(&self, _shared: &Shared) -> PathBuf {
        // call our base path builder
        Self::build_graphic_base_path(&self.id)
    }
}

impl EntityMetadata {
    /// Split an entity kind into its name and its serialized data if it has any
    pub fn split(&self) -> Result<(EntityKinds, Option<String>), ApiError> {
        let data = match self {
            EntityMetadata::Device(device) => Some(serialize!(device)),
            EntityMetadata::Vendor(vendor) => Some(serialize!(vendor)),
            EntityMetadata::Collection(collection) => Some(serialize!(collection)),
            EntityMetadata::FileSystem(fs) => Some(serialize!(fs)),
            EntityMetadata::Folder(folder) => Some(serialize!(folder)),
            EntityMetadata::WindowsProcessTree(win_proc_tree) => Some(serialize!(win_proc_tree)),
            EntityMetadata::WindowsProcess(win_proc) => Some(serialize!(win_proc)),
            EntityMetadata::NetworkConnection(conn) => Some(serialize!(conn)),
            EntityMetadata::PeSection(section) => Some(serialize!(section)),
            EntityMetadata::PeImport(import) => Some(serialize!(import)),
            EntityMetadata::SigmaRule(rule) => Some(serialize!(rule)),
            EntityMetadata::Flag(flag) => Some(serialize!(flag)),
            EntityMetadata::Incident(incident) => Some(serialize!(incident)),
            EntityMetadata::CompiledFunction(func) => Some(serialize!(func)),
            EntityMetadata::DecompiledFunction(decomp) => Some(serialize!(decomp)),
            EntityMetadata::Tool(tool) => Some(serialize!(tool)),
            EntityMetadata::ToolInstance(instance) => Some(serialize!(instance)),
            EntityMetadata::Agent(agent) => Some(serialize!(agent)),
            EntityMetadata::ToolSet(tool_set) => Some(serialize!(tool_set)),
            EntityMetadata::ToolSetInstance(instance) => Some(serialize!(instance)),
            EntityMetadata::WorkflowCatalog(workflow) => Some(serialize!(workflow)),
            EntityMetadata::WorkflowInstance(instance) => Some(serialize!(instance)),
            EntityMetadata::ModelEndpoint(endpoint) => Some(serialize!(endpoint)),
            EntityMetadata::ModelProfile(profile) => Some(serialize!(profile)),
            EntityMetadata::ModelCredential(credential) => Some(serialize!(credential)),
            EntityMetadata::Other => None,
        };
        Ok((self.into(), data))
    }
}

impl EntityForm {
    /// Adds a multipart field to our entity form
    ///
    /// # Returns
    ///
    /// Returns the field again if it's an image, otherwise attempts to consume it and
    /// returns None on success
    ///
    /// # Errors
    ///
    /// Returns an error if the field is invalid
    ///
    /// # Arguments
    ///
    /// * `field` - The field to try to add
    pub async fn add<'a>(&'a mut self, field: Field<'a>) -> Result<Option<Field<'a>>, ApiError> {
        // get the name of this field
        if let Some(name) = field.name().map(ToOwned::to_owned) {
            // iterate over the segments ('<NAME>[<KEY1>][<KEY2>]') in the field name
            let name_segments = super::helpers::parse_bracket_segments(&name)?;
            let mut name_segments_iter = name_segments.into_iter();
            // add this fields value to our form
            match name_segments_iter
                .next()
                .ok_or(bad_internal!("Multipart field name is empty".to_string()))?
            {
                "name" => self.name = Some(field.text().await?),
                "description" => self.description = Some(field.text().await?),
                // this is image data so return it so we can stream it to s3
                "image" => return Ok(Some(field)),
                // kind fields
                "kind" => {
                    // try to cast our kind to the correct kind
                    let kind_raw = field.text().await.map_err(|_| {
                        bad_internal!(
                            "Invalid entity kind: entity kind must be a string".to_string(),
                        )
                    })?;
                    let cast = EntityKinds::from_str(&kind_raw)
                        .map_err(|_| bad_internal!(format!("Invalid entity kind '{kind_raw}'")))?;
                    // set our kind
                    self.kind = Some(cast);
                }
                "metadata" => {
                    name_segments_iter =
                        self.metadata.add(field, &name, name_segments_iter).await?;
                }
                // this could be a list field
                maybe_list => {
                    match maybe_list {
                        "groups" => self.groups.push(field.text().await?),
                        "tags" => {
                            let key = name_segments_iter.next().filter(|s| !s.is_empty()).ok_or(
                                bad_internal!("Entity tag key is empty or missing".to_string()),
                            )?;
                            // get an entry to this tags value vec
                            let entry = self.tags.entry(key.to_owned()).or_default();
                            // add our value
                            entry.insert(field.text().await?);
                        }
                        _ => return bad!(format!("'{name}' is not a valid form name")),
                    }
                    // this is a list field, so make sure the next segment is an empty `[]`
                    ensure_empty_segment!(name_segments_iter, name)?;
                }
            }
            // make sure there aren't extra segments left over
            ensure_segments_complete!(name_segments_iter, name)?;
            // we found and consumed a valid form entry
            return Ok(None);
        }
        bad!("All entity form entries must have a name!".to_string())
    }

    /// Build an association request for anything in this entity form
    ///
    /// # Arguments
    ///
    /// * `user` - The user building an association request
    /// * `id` - The id of the entity this association is coming from
    /// * `shared` - Shared Thorium objects
    pub(super) async fn build_association_req(
        &mut self,
        user: &User,
        id: Uuid,
        shared: &Shared,
    ) -> Result<Option<AssociationRequest>, ApiError> {
        // theres some validation duplication here
        // get our kind or raise an error
        let Some(kind) = self.kind else {
            return bad!("No kind set for entity?".to_owned());
        };
        // get our name or raise an error
        let Some(name) = &self.name else {
            return bad!("No name set for entity?".to_owned());
        };
        // build the association request for each of our different entities
        match kind {
            EntityKinds::Device => {
                // devices to vendor relationships is always developed by
                let assoc_kind = AssociationKind::DevelopedBy;
                // get the numer of associations to make
                let vendor_len = self.metadata.vendors.len();
                // build our source target
                let source = AssociationTarget::Entity {
                    id,
                    name: name.to_owned(),
                };
                // get the groups we may write this device's associations in before anything
                // is written; a form without groups is rejected when it's cast
                let allowed = if vendor_len > 0 && !self.groups.is_empty() {
                    AssociationRequest::authorize_source_groups(user, &self.groups, shared).await?
                } else {
                    self.groups.clone()
                };
                // start with an empty req
                let mut req =
                    AssociationRequest::with_capacity(assoc_kind, source.clone(), vendor_len);
                // step over the vendors we are associating with this device
                for vendor_id in self.metadata.vendors.drain(..) {
                    // get the entity for this vendor if it exists
                    let entity = Entity::get(user, vendor_id, shared).await?;
                    // make sure this is a vendor entity
                    if entity.kind != EntityKinds::Vendor {
                        return bad!(format!("Entity {vendor_id} is not a vendor!"));
                    }
                    // build the source object for this entity
                    let other = AssociationTarget::Entity {
                        id: vendor_id,
                        name: entity.name,
                    };
                    // make sure the device and this vendor share a group we can write in
                    AssociationRequest::shared_groups(&source, &allowed, &other, &entity.groups)?;
                    // add this link to the other
                    req.targets.push(other);
                }
                Ok(Some(req))
            }
            _ => Ok(None),
        }
    }

    /// Cast this entity form to an entity, verifying the form is valid
    ///
    /// The form must have a kind, a non-empty name, and at least one group, and none of its groups
    /// may have an empty name.
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is creating this entity
    /// * `id` - The id to give this entity
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "EntityForm::cast", skip_all, fields(name = self.name), err(Debug))]
    pub async fn cast(
        mut self,
        user: &User,
        id: Uuid,
        shared: &Shared,
    ) -> Result<Entity, ApiError> {
        // make sure we have a kind set
        let Some(kind) = self.kind.take() else {
            return bad!("A kind must be set for all entities".to_owned());
        };
        // make sure we got a name
        let Some(name) = self.name.take() else {
            return bad!("Entity must have a name!".to_string());
        };
        // make sure the name isn't empty
        if name.is_empty() {
            return bad!("Entity's name cannot be empty!".to_string());
        }
        // make sure we got groups
        if self.groups.is_empty() {
            return bad!("Entity must be in at least 1 group!".to_string());
        }
        // make sure none of the groups have empty names
        if self.groups.iter().any(String::is_empty) {
            return bad!(format!(
                "Entity cannot have any groups with empty names: {:?}",
                self.groups
            ));
        }
        // convert our metadata to an actual entity
        let metadata = self.metadata.cast(kind, &self.groups, shared).await?;
        // build an association request for this
        // build our casted entity
        let cast = Entity {
            id,
            name,
            kind,
            metadata,
            description: self.description,
            submitter: user.username.clone(),
            groups: self.groups,
            created: Utc::now(),
            tags: HashMap::default(),
            image: self.image,
        };
        Ok(cast)
    }
}

impl EntityMetadataForm {
    /// Adds a multipart field to our entity metadata form
    ///
    /// # Errors
    ///
    /// Returns an error if the field is invalid
    ///
    /// # Arguments
    ///
    /// * `field` - The field to try to add
    /// * `name` - The name of the field to add
    /// * `name_segments` - An iterator over the segments of the field name
    pub async fn add<'a, I: Iterator<Item = &'a str>>(
        &'a mut self,
        field: Field<'a>,
        name: &str,
        mut name_segments: I,
    ) -> Result<I, ApiError> {
        match name_segments.next().ok_or(bad_internal!(
            "Invalid entity metadata field: metadata field name is missing".to_string()
        ))? {
            // the device/vendor specific form fields
            "critical_system" => {
                self.critical_system = Some(field.text().await?.parse()?);
            }
            "sensitive_location" => {
                self.sensitive_location = Some(field.text().await?.parse()?);
            }
            // the collection specific form fields
            "collection_kind" => {
                // try to cast the collection kind
                let kind_raw = field.text().await.map_err(|_| {
                    bad_internal!(
                        "Invalid collection kind: entity kind must be a string".to_string(),
                    )
                })?;
                self.collection_kind =
                    Some(kind_raw.parse().map_err(|_| {
                        bad_internal!(format!("Invalid collection kind '{kind_raw}'"))
                    })?);
            }
            "collection_tags_case_insensitive" => {
                let raw = field.text().await?;
                self.collection_tags_case_insensitive = Some(raw.parse().map_err(|err| {
                    bad_internal!(format!(
                        "Invalid collection tags case insensitive value '{raw}': {err}"
                    ))
                })?);
            }
            "collection_ignore_groups" => {
                let raw = field.text().await?;
                self.collection_ignore_groups = Some(raw.parse().map_err(|err| {
                    bad_internal!(format!(
                        "Invalid collection ignore groups value '{raw}': {err}"
                    ))
                })?);
            }
            "collection_start" => {
                let start_raw = field.text().await?;
                self.collection_start = Some(start_raw.parse().map_err(|_| {
                    bad_internal!(format!("Invalid collection start datetime '{start_raw}'"))
                })?);
            }
            "collection_end" => {
                let end_raw = field.text().await?;
                self.collection_end = Some(end_raw.parse().map_err(|_| {
                    bad_internal!(format!("Invalid collection end datetime '{end_raw}'"))
                })?);
            }
            // the filesystem specific form fields
            "filesystem_id" => self.filesystem_id = Some(field.text().await?.parse::<Uuid>()?),
            "sha256" => self.sha256 = Some(field.text().await?),
            "names_sha256" => self.names_sha256 = Some(field.text().await?),
            "data_sha256" => self.data_sha256 = Some(field.text().await?),
            "all_sha256" => self.all_sha256 = Some(field.text().await?),
            // the process specific form fields
            "pid" => self.pid = Some(field.text().await?.parse()?),
            "parent_pid" => self.parent_pid = Some(field.text().await?.parse()?),
            "name" => self.name = Some(field.text().await?),
            "image_path" => self.image_path = Some(field.text().await?),
            "command" => self.command = Some(field.text().await?),
            "offset" => self.offset = Some(field.text().await?.parse()?),
            "threads" => self.threads = Some(field.text().await?.parse()?),
            "handles" => self.handles = Some(field.text().await?.parse()?),
            "is_wow64" => self.is_wow64 = Some(field.text().await?.parse()?),
            "session_id" => self.session_id = Some(field.text().await?.parse()?),
            "create_time" => self.create_time = Some(field.text().await?.parse()?),
            "exit_time" => self.exit_time = Some(field.text().await?.parse()?),
            // the network connection form fields
            "protocol" => self.protocol = Some(field.text().await?.parse()?),
            "source" => self.source = Some(field.text().await?.parse()?),
            "source_port" => self.source_port = Some(field.text().await?.parse()?),
            "destination" => self.destination = Some(field.text().await?.parse()?),
            "destination_port" => self.destination_port = Some(field.text().await?.parse()?),
            "state" => self.state = Some(field.text().await?.parse::<NetConState>()?),
            "process" => self.process = Some(field.text().await?),
            // the PE section specific form fields
            "md5" => self.md5 = Some(field.text().await?),
            "raw_size" => self.raw_size = Some(field.text().await?.parse()?),
            "virtual_size" => self.virtual_size = Some(field.text().await?.parse()?),
            "entropy" => self.entropy = Some(field.text().await?.parse()?),
            // the sigma rule specific fields
            "sigma_rule" => self.sigma_rule = Some(field.text().await?),
            "score" => self.score = Some(field.text().await?.parse()?),
            // the flag specific fields
            "suspicion" => self.suspicion = Some(field.text().await?.parse()?),
            "confidence" => self.confidence = Some(field.text().await?.parse()?),
            "content" => self.content = Some(field.text().await?),
            "reasoning" => self.reasoning = Some(field.text().await?),
            // the function specific fields
            "function_address" => self.function_address = Some(field.text().await?.parse()?),
            "decompilation_content" => self.decompilation_content = Some(field.text().await?),
            // the incident specific fields
            "cover_term" => self.cover_term = Some(field.text().await?),
            // the tool specific fields
            "tool_kind" => {
                // parse the kind of tool to create
                let raw = field.text().await?;
                self.tool_kind = Some(
                    raw.parse()
                        .map_err(|_| bad_internal!(format!("Invalid tool kind '{raw}'")))?,
                );
            }
            "tool_version" => self.tool_version = Some(field.text().await?),
            "tool_spec" => self.tool_spec = Some(field.text().await?),
            "tool_instance_tool" => {
                self.tool_instance_tool = Some(field.text().await?.parse::<Uuid>()?);
            }
            "tool_harness_env" => self.tool_harness_env = Some(field.text().await?),
            // the model configuration specific fields
            "model_endpoint_provider" => self.model_endpoint_provider = Some(field.text().await?),
            "model_endpoint_credential" => {
                self.model_endpoint_credential = Some(field.text().await?.parse::<Uuid>()?);
            }
            "model_endpoint_models" => self.model_endpoint_models = Some(field.text().await?),
            "model_profile_endpoint" => {
                self.model_profile_endpoint = Some(field.text().await?.parse::<Uuid>()?);
            }
            "model_profile_model_id" => self.model_profile_model_id = Some(field.text().await?),
            "model_profile_api" => {
                // parse the protocol by its name
                let raw = field.text().await?;
                self.model_profile_api =
                    Some(ModelApi::parse_name(&raw).map_err(|msg| bad_internal!(msg))?);
            }
            "model_profile_parameters" => {
                self.model_profile_parameters = Some(field.text().await?);
            }
            "model_profile_limits" => self.model_profile_limits = Some(field.text().await?),
            "model_profile_capabilities" => {
                self.model_profile_capabilities = Some(field.text().await?);
            }
            "model_credential_kind" => self.model_credential_kind = Some(field.text().await?),
            "model_credential_secret" => {
                self.model_credential_secret = Some(ModelSecret::from(field.text().await?));
            }
            "model_credential_session_token" => {
                self.model_credential_session_token = Some(ModelSecret::from(field.text().await?));
            }
            "model_credential_expires" => {
                self.model_credential_expires = Some(field.text().await?.parse()?);
            }
            // the agent specific fields
            "agent_config" => self.agent_config = Some(field.text().await?),
            "agent_model_auth" => self.agent_model_auth = Some(field.text().await?),
            "agent_harness" => self.agent_harness = Some(field.text().await?.parse::<Uuid>()?),
            "extend_target_groups" => {
                self.extend_target_groups = Some(field.text().await?.parse()?);
            }
            // the toolset and workflow specific fields
            graph if graph.starts_with("toolset_") || graph.starts_with("workflow_") => {
                // a list field's next segment is an empty `[]`
                if self.add_graph_field(graph, field).await? {
                    ensure_empty_segment!(name_segments, name)?;
                }
            }
            maybe_list => {
                match maybe_list {
                    // the agent edge list fields
                    "agent_tools" => self.agent_tools.push(field.text().await?.parse::<Uuid>()?),
                    "agent_tool_sets" => {
                        self.agent_tool_sets
                            .push(field.text().await?.parse::<Uuid>()?);
                    }
                    "agent_sub_agents" => {
                        self.agent_sub_agents
                            .push(field.text().await?.parse::<Uuid>()?);
                    }
                    "urls" => {
                        self.urls.push(field.text().await?);
                    }
                    "vendors" => {
                        // try to parse this vendor id
                        let vendor_id = field.text().await?.parse::<Uuid>()?;
                        // add this id to our vendor list
                        self.vendors.push(vendor_id);
                    }
                    "critical_sectors" => {
                        // try to convert this field to a critical sector
                        let sector = CriticalSector::from_str(&field.text().await?)?;
                        // add this critical sector
                        self.critical_sectors.insert(sector);
                    }
                    "countries" => {
                        // validate and parse this country
                        let country = Country::new(&field.text().await?)?;
                        // add this country to our metadata form
                        self.countries.insert(country);
                    }
                    "collection_tags" => {
                        let key =
                            name_segments
                                .next()
                                .filter(|s| !s.is_empty())
                                .ok_or(bad_internal!(
                                    "Collection tag key is empty or missing".to_string()
                                ))?;
                        // get an entry to collection tags set
                        let entry = self.collection_tags.entry(key.to_owned()).or_default();
                        // add our value
                        entry.insert(field.text().await?);
                    }
                    "tool_ids" => self.tool_ids.push(field.text().await?.parse::<Uuid>()?),
                    "functions" => self.functions.push(field.text().await?),
                    "sigma_applies_to" => self.sigma_applies_to.push(field.text().await?.parse()?),
                    "sigma_actions" => self.sigma_actions.push(deserialize!(&field.text().await?)),
                    "disassembly" => self.disassembly.push(deserialize!(&field.text().await?)),
                    // the incident specific list fields
                    "mission_teams" => self.mission_teams.push(field.text().await?),
                    "networks" => self.networks.push(field.text().await?),
                    "machines" => self.machines.push(field.text().await?),
                    "locations" => self.locations.push(field.text().await?),
                    // the native tools a new harness instance provides
                    "tool_provides" => self
                        .tool_provides
                        .push(field.text().await?.parse::<Uuid>()?),
                    // the model ids a new harness tool never resolves to
                    "tool_disabled_models" => {
                        self.tool_disabled_models.insert(field.text().await?);
                    }
                    // the model ids a new model endpoint never resolves to
                    "model_endpoint_disabled_models" => {
                        self.model_endpoint_disabled_models
                            .insert(field.text().await?);
                    }
                    bad_name => {
                        return bad!(format!("'{bad_name}' is not a valid metadata form name"));
                    }
                }
                // this is a list field, so make sure the next segment is an empty `[]`
                ensure_empty_segment!(name_segments, name)?;
            }
        }
        Ok(name_segments)
    }

    /// Adds a toolset or workflow field to our entity metadata form
    ///
    /// Returns whether the field is a list field.
    ///
    /// # Arguments
    ///
    /// * `name` - The field's name
    /// * `field` - The field to add
    async fn add_graph_field(&mut self, name: &str, field: Field<'_>) -> Result<bool, ApiError> {
        match name {
            // the toolset specific fields
            "toolset_read_only" => self.toolset_read_only = Some(field.text().await?.parse()?),
            "toolset_selector_key" => self.toolset_selector_key = Some(field.text().await?),
            "toolset_selector_value" => self.toolset_selector_value = Some(field.text().await?),
            "toolset_auto_instance" => {
                self.toolset_auto_instance = Some(field.text().await?.parse()?);
            }
            "toolset_instance" => self.toolset_instance = Some(field.text().await?),
            // the workflow specific fields
            "workflow_entry_agent" => {
                self.workflow_entry_agent = Some(field.text().await?.parse::<Uuid>()?);
            }
            "workflow_auto_instance" => {
                self.workflow_auto_instance = Some(field.text().await?.parse()?);
            }
            "workflow_limits" => self.workflow_limits = Some(field.text().await?),
            "workflow_default_models" => self.workflow_default_models = Some(field.text().await?),
            "workflow_model_auth" => self.workflow_model_auth = Some(field.text().await?),
            "workflow_instance" => self.workflow_instance = Some(field.text().await?),
            // the toolset and workflow edge list fields
            "toolset_tools" => {
                self.toolset_tools
                    .push(field.text().await?.parse::<Uuid>()?);
                return Ok(true);
            }
            "workflow_agents" => {
                self.workflow_agents
                    .push(field.text().await?.parse::<Uuid>()?);
                return Ok(true);
            }
            bad_name => {
                return bad!(format!("'{bad_name}' is not a valid metadata form name"));
            }
        }
        Ok(false)
    }

    /// Attempt to cast the entity kind form to an [`EntityKind`], verifying the form is valid
    ///
    /// Tools, tool instances, agents, toolsets, workflows, and model configuration are never cast
    /// from a form because they are built by their own create paths, so casting them is an internal
    /// error.
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of entity to cast this form to
    /// * `groups` - The groups the entity will be added to
    /// * `shared` - Shared Thorium objects
    pub async fn cast(
        self,
        kind: EntityKinds,
        groups: &[String],
        shared: &Shared,
    ) -> Result<EntityMetadata, ApiError> {
        // cast the kind form depending on the kind name
        match kind {
            EntityKinds::Device => Ok(EntityMetadata::Device(
                DeviceEntity::from_form(self, groups, shared).await?,
            )),
            EntityKinds::Vendor => Ok(EntityMetadata::Vendor(VendorEntity::from_form(self))),
            EntityKinds::Collection => Ok(EntityMetadata::Collection(CollectionEntity::from_form(
                self,
            )?)),
            EntityKinds::FileSystem => Ok(EntityMetadata::FileSystem(FileSystemEntity::from_form(
                self,
            )?)),
            EntityKinds::Folder => Ok(EntityMetadata::Folder(FileSystemFolderEntity::from_form(
                self,
            )?)),
            EntityKinds::WindowsProcessTree => Ok(EntityMetadata::WindowsProcessTree(
                WindowsProcessTreeEntity::from_form(self)?,
            )),
            EntityKinds::WindowsProcess => Ok(EntityMetadata::WindowsProcess(
                WindowsProcessEntity::from_form(self)?,
            )),
            EntityKinds::NetworkConnection => Ok(EntityMetadata::NetworkConnection(
                NetworkConnection::from_form(self)?,
            )),
            EntityKinds::PeSection => {
                Ok(EntityMetadata::PeSection(PeSectionEntity::from_form(self)?))
            }
            EntityKinds::PeImport => Ok(EntityMetadata::PeImport(PeImportEntity::from_form(self)?)),
            EntityKinds::SigmaRule => Ok(EntityMetadata::SigmaRule(SigmaRule::from_form(self)?)),
            EntityKinds::Flag => Ok(EntityMetadata::Flag(Flag::from_form(self)?)),
            EntityKinds::Incident => Ok(EntityMetadata::Incident(Incident::from_form(self))),
            EntityKinds::CompiledFunction => Ok(EntityMetadata::CompiledFunction(
                CompiledFunction::from_form(self)?,
            )),
            EntityKinds::DecompiledFunction => Ok(EntityMetadata::DecompiledFunction(
                DecompiledFunction::from_form(self)?,
            )),
            // tools are built by the shared publish path, never cast from a form
            EntityKinds::Tool | EntityKinds::ToolInstance => {
                internal_err!(format!("{kind} entities are created by the publish path"))
            }
            // model configuration, agents, and toolsets are built by their own create paths,
            // never cast from a form
            EntityKinds::Agent
            | EntityKinds::ToolSet
            | EntityKinds::ToolSetInstance
            | EntityKinds::WorkflowCatalog
            | EntityKinds::WorkflowInstance
            | EntityKinds::ModelEndpoint
            | EntityKinds::ModelProfile
            | EntityKinds::ModelCredential => {
                internal_err!(format!(
                    "{kind} entities are created by their own create path"
                ))
            }
            EntityKinds::Other => Ok(EntityMetadata::Other),
        }
    }
}

impl EntityUpdateForm {
    /// Adds a multipart field to our entity update form
    ///
    /// # Returns
    ///
    /// Returns the field again if it's an image, otherwise attempts to consume it and
    /// returns None on success
    ///
    /// # Errors
    ///
    /// Returns an error if the field is invalid
    ///
    /// # Arguments
    ///
    /// * `field` - The field to try to add
    pub async fn add<'a>(&'a mut self, field: Field<'a>) -> Result<Option<Field<'a>>, ApiError> {
        // get the name of this field
        if let Some(name) = field.name().map(ToOwned::to_owned) {
            // iterate over the segments ('<NAME>[<KEY1>][<KEY2>]') in the field name
            let name_segments = super::helpers::parse_bracket_segments(&name)?;
            let mut name_segments_iter = name_segments.into_iter();
            // add this fields value to our form
            match name_segments_iter
                .next()
                .ok_or(bad_internal!("Multipart field name is empty".to_string()))?
            {
                "name" => self.name = Some(field.text().await?),
                "clear_image" => self.clear_image = Some(field.text().await?.parse()?),
                "description" => self.description = Some(field.text().await?),
                "clear_description" => self.clear_description = Some(field.text().await?.parse()?),
                // this is image data so return it so we can stream it to s3
                "image" => return Ok(Some(field)),
                "metadata" => {
                    name_segments_iter =
                        self.metadata.add(field, &name, name_segments_iter).await?;
                }
                // this could be a list field
                maybe_list => {
                    match maybe_list {
                        "add_groups" => self.add_groups.push(field.text().await?),
                        "remove_groups" => self.remove_groups.push(field.text().await?),
                        // this is an invalid form field
                        bad_name => {
                            return bad!(format!(
                                "'{bad_name}' is not a valid entity update form name"
                            ));
                        }
                    }
                    // this is a list field, so make sure the next segment is an empty `[]`
                    ensure_empty_segment!(name_segments_iter, name)?;
                }
            }
            // make sure there aren't extra segments left over
            ensure_segments_complete!(name_segments_iter, name)?;
            // we found and consumed a valid form entry
            return Ok(None);
        }
        bad!("All entity update form entries must have a name!".to_string())
    }

    /// Update an entity's groups
    ///
    /// Does not consume the form's groups because we need them later to calculate
    /// what should be modified in the db. Added groups need edit rights and the `Entities`
    /// flag, and removed groups need the same rights as a delete. A `Contained` root's change
    /// is checked against its instances, which follow it, and is returned so the caller can
    /// write it after the root; an instance's groups can't be changed directly.
    ///
    /// # Arguments
    ///
    /// * `user` - The user updating the entity
    /// * `entity` - The entity whose groups to update
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "EntityUpdate::update_groups", skip_all, fields(entity = entity.name), err(Debug))]
    async fn update_groups(
        &self,
        user: &User,
        entity: &mut Entity,
        shared: &Shared,
    ) -> Result<GroupChangePlan, ApiError> {
        // nothing is checked or planned without a group change
        let mut plan = GroupChangePlan::default();
        if self.add_groups.is_empty() && self.remove_groups.is_empty() {
            return Ok(plan);
        }
        // an instance's groups always follow its root's
        if entity.kind == EntityKinds::ToolInstance {
            return bad!(
                "a tool instance's groups follow its tool; change the tool's groups instead"
                    .to_owned()
            );
        }
        if entity.kind == EntityKinds::ToolSetInstance {
            return bad!(
                "a toolset instance's groups follow its toolset; change the toolset's groups instead"
                    .to_owned()
            );
        }
        if entity.kind == EntityKinds::WorkflowInstance {
            return bad!(
                "a workflow instance's groups follow its workflow; change the workflow's groups instead"
                    .to_owned()
            );
        }
        // make sure the groups added/removed aren't empty
        if self.add_groups.iter().any(String::is_empty) {
            return bad!("One or more of the groups to add has an empty name!".to_string());
        }
        if self.remove_groups.iter().any(String::is_empty) {
            return bad!("One or more of the groups to remove has an empty name!".to_string());
        }
        // make sure we can edit entities in every added group, which also checks they exist
        if !self.add_groups.is_empty() {
            let _ = Group::authorize_check_allow_all(
                user,
                &self.add_groups,
                Group::editable,
                "edit",
                Some(GroupAllowAction::Entities),
                shared,
            )
            .await?;
        }
        // make sure we can remove this entity from every removed group
        if !self.remove_groups.is_empty() {
            entity
                .authorize_removal(user, &self.remove_groups, shared)
                .await?;
        }
        // model configuration's references must stay contained
        if models::is_model_kind(entity.kind) {
            models::check_group_change(user, entity, &self.add_groups, &self.remove_groups, shared)
                .await?;
        }
        // a graph entity's edges must stay contained, and a root's instances follow it
        plan.instances =
            helpers::heap_future(|| self.check_graph_groups(user, entity, shared)).await?;
        // a tool's change is checked against its instances before anything is written
        if entity.kind == EntityKinds::Tool {
            // get the instances that follow the tool
            plan.instances = tools::instances_for_change(user, entity, shared).await?;
            // a group can't be removed while something in it references the tool
            if !self.remove_groups.is_empty() {
                tools::check_removal(user, entity, &plan.instances, &self.remove_groups, shared)
                    .await?;
            }
            // the policies the instances name must be available in each added group
            if !self.add_groups.is_empty() {
                plan.policy_adds =
                    tools::check_policies(user, entity, &plan.instances, &self.add_groups, shared)
                        .await?;
            }
        }
        update_add_rem!(
            entity.groups,
            self.add_groups,
            self.remove_groups,
            "Entity",
            "groups"
        )?;
        // make sure we still have some groups left
        if entity.groups.is_empty() {
            return bad!(
                "You cannot delete all of an entity's groups with an update request!".to_string()
            );
        }
        Ok(plan)
    }
}

impl EntityUpdateForm {
    /// Check a group change of a toolset, workflow, or agent against its graph edges
    ///
    /// Returns the instances of a toolset or workflow, which follow its group change.
    ///
    /// # Arguments
    ///
    /// * `user` - The user updating the entity
    /// * `entity` - The entity whose groups change, in all of its groups
    /// * `shared` - Shared Thorium objects
    async fn check_graph_groups(
        &self,
        user: &User,
        entity: &Entity,
        shared: &Shared,
    ) -> Result<Vec<Entity>, ApiError> {
        match entity.kind {
            // a toolset's includes must stay contained, referrers block a removal, and its
            // instances follow it
            EntityKinds::ToolSet => {
                let instances =
                    toolsets::instances(user, entity.id, &entity.groups, shared).await?;
                toolsets::check_group_change(
                    user,
                    entity,
                    &instances,
                    &self.add_groups,
                    &self.remove_groups,
                    &self.metadata.remove_toolset_tools,
                    shared,
                )
                .await?;
                Ok(instances)
            }
            // a workflow's agents and its instances' snapshots must be in each added group, and
            // its instances follow it
            EntityKinds::WorkflowCatalog => {
                let instances =
                    workflows::instances(user, entity.id, &entity.groups, shared).await?;
                workflows::check_group_change(
                    user,
                    entity,
                    &instances,
                    &self.add_groups,
                    &self.remove_groups,
                    &self.metadata.remove_workflow_agents,
                    shared,
                )
                .await?;
                Ok(instances)
            }
            // an agent's edges must stay contained, and agents using it block a removal
            EntityKinds::Agent => {
                agents::check_group_change(
                    user,
                    entity,
                    &self.add_groups,
                    &self.remove_groups,
                    &agents::removed_edges(&self.metadata),
                    shared,
                )
                .await?;
                Ok(Vec::new())
            }
            // other kinds have no graph edges to check
            _ => Ok(Vec::new()),
        }
    }
}

impl EntityMetadataUpdateForm {
    /// Adds a toolset or workflow field to our entity metadata update form
    ///
    /// Returns whether the field is a list field.
    ///
    /// # Arguments
    ///
    /// * `name` - The field's name
    /// * `field` - The field to add
    async fn add_graph_field(&mut self, name: &str, field: Field<'_>) -> Result<bool, ApiError> {
        match name {
            // the toolset specific fields
            "toolset_read_only" => self.toolset_read_only = Some(field.text().await?.parse()?),
            "toolset_selector_key" => self.toolset_selector_key = Some(field.text().await?),
            "toolset_selector_value" => self.toolset_selector_value = Some(field.text().await?),
            "clear_toolset_selector" => {
                self.clear_toolset_selector = Some(field.text().await?.parse()?);
            }
            "toolset_auto_instance" => {
                self.toolset_auto_instance = Some(field.text().await?.parse()?);
            }
            // the workflow specific fields
            "workflow_entry_agent" => {
                self.workflow_entry_agent = Some(field.text().await?.parse::<Uuid>()?);
            }
            "clear_workflow_entry_agent" => {
                self.clear_workflow_entry_agent = Some(field.text().await?.parse()?);
            }
            "workflow_auto_instance" => {
                self.workflow_auto_instance = Some(field.text().await?.parse()?);
            }
            "workflow_limits" => self.workflow_limits = Some(field.text().await?),
            "workflow_default_models" => self.workflow_default_models = Some(field.text().await?),
            "workflow_model_auth" => self.workflow_model_auth = Some(field.text().await?),
            // the toolset and workflow edge list fields
            "add_workflow_agents"
            | "remove_workflow_agents"
            | "add_toolset_tools"
            | "remove_toolset_tools" => {
                let id = field.text().await?.parse::<Uuid>()?;
                match name {
                    "add_workflow_agents" => self.add_workflow_agents.push(id),
                    "remove_workflow_agents" => self.remove_workflow_agents.push(id),
                    "add_toolset_tools" => self.add_toolset_tools.push(id),
                    _ => self.remove_toolset_tools.push(id),
                }
                return Ok(true);
            }
            bad_name => {
                return bad!(format!(
                    "'{bad_name}' is not a valid entity kind update form name"
                ));
            }
        }
        Ok(false)
    }

    /// Adds a multipart field to our entity metadata update form
    ///
    /// # Errors
    ///
    /// Returns an error if the field is invalid
    ///
    /// # Arguments
    ///
    /// * `field` - The field to try to add
    /// * `name` - The full name of the field
    /// * `name_segments` - An iterator over the parsed segments from the field name
    pub async fn add<'a, I: Iterator<Item = &'a str>>(
        &'a mut self,
        field: Field<'a>,
        name: &str,
        mut name_segments: I,
    ) -> Result<I, ApiError> {
        // remember every metadata field the update sends
        self.sent.push(name.to_owned());
        match name_segments.next().ok_or(bad_internal!(
            "Invalid entity metadata update field: metadata field name is missing".to_string()
        ))? {
            "critical_system" => self.critical_system = Some(field.text().await?.parse()?),
            "clear_critical_system" => {
                self.clear_critical_system = Some(field.text().await?.parse()?);
            }
            "sensitive_location" => self.sensitive_location = Some(field.text().await?.parse()?),
            "clear_sensitive_location" => {
                self.clear_sensitive_location = Some(field.text().await?.parse()?);
            }
            "collection_start" => {
                let start_raw = field.text().await?;
                self.collection_start = Some(start_raw.parse().map_err(|_| {
                    bad_internal!(format!("Invalid collection start datetime '{start_raw}'"))
                })?);
            }
            "collection_end" => {
                let end_raw = field.text().await?;
                self.collection_end = Some(end_raw.parse().map_err(|_| {
                    bad_internal!(format!("Invalid collection end datetime '{end_raw}'"))
                })?);
            }
            "clear_collection_start" => self.clear_collection_start = Some(true),
            "clear_collection_end" => self.clear_collection_end = Some(true),
            "collection_tags_case_insensitive" => {
                let raw = field.text().await?;
                self.collection_tags_case_insensitive = Some(raw.parse().map_err(|err| {
                    bad_internal!(format!(
                        "Invalid collection tags case insensitive value '{raw}': {err}"
                    ))
                })?);
            }
            "collection_ignore_groups" => {
                let raw = field.text().await?;
                self.collection_ignore_groups = Some(raw.parse().map_err(|err| {
                    bad_internal!(format!(
                        "Invalid collection ignore groups value '{raw}': {err}"
                    ))
                })?);
            }
            // the process specific form fields
            "name" => self.name = Some(field.text().await?),
            "image_path" => self.image_path = Some(field.text().await?),
            "command" => self.command = Some(field.text().await?),
            "offset" => self.offset = Some(field.text().await?.parse()?),
            "threads" => self.threads = Some(field.text().await?.parse()?),
            "handles" => self.handles = Some(field.text().await?.parse()?),
            "is_wow64" => self.is_wow64 = Some(field.text().await?.parse()?),
            "session_id" => self.session_id = Some(field.text().await?.parse()?),
            "create_time" => self.create_time = Some(field.text().await?.parse()?),
            "exit_time" => self.exit_time = Some(field.text().await?.parse()?),
            // the network connection form fields
            "protocol" => self.protocol = Some(field.text().await?.parse()?),
            "source" => self.source = Some(field.text().await?.parse()?),
            "source_port" => self.source_port = Some(field.text().await?.parse()?),
            "destination" => self.destination = Some(field.text().await?.parse()?),
            "destination_port" => self.destination_port = Some(field.text().await?.parse()?),
            "state" => self.state = Some(field.text().await?.parse::<NetConState>()?),
            "pid" => self.pid = Some(field.text().await?.parse()?),
            "process" => self.process = Some(field.text().await?),
            // the PE section specific form fields
            "md5" => self.md5 = Some(field.text().await?),
            "raw_size" => self.raw_size = Some(field.text().await?.parse()?),
            "virtual_size" => self.virtual_size = Some(field.text().await?.parse()?),
            "entropy" => self.entropy = Some(field.text().await?.parse()?),
            // the sigma rule specific fields
            "sigma_rule" => self.sigma_rule = Some(field.text().await?),
            "score" => self.score = Some(field.text().await?.parse()?),
            // the flag specific fields
            "suspicion" => self.suspicion = Some(field.text().await?.parse()?),
            "confidence" => self.confidence = Some(field.text().await?.parse()?),
            "content" => self.content = Some(field.text().await?),
            "reasoning" => self.reasoning = Some(field.text().await?),
            // the incident specific fields
            "cover_term" => self.cover_term = Some(field.text().await?),
            // the function specific fields
            "function_address" => self.function_address = Some(field.text().await?.parse()?),
            "decompilation_content" => self.decompilation_content = Some(field.text().await?),
            // the tool specific fields
            "tool_version" => self.tool_version = Some(field.text().await?),
            "tool_spec" => self.tool_spec = Some(field.text().await?),
            "tool_instance_deprecated" => {
                self.tool_instance_deprecated = Some(field.text().await?.parse()?);
            }
            "tool_harness_env" => self.tool_harness_env = Some(field.text().await?),
            "clear_tool_harness_env" => {
                self.clear_tool_harness_env = Some(field.text().await?.parse()?);
            }
            // the model endpoint specific fields
            "model_endpoint_provider" => self.model_endpoint_provider = Some(field.text().await?),
            "model_endpoint_credential" => {
                self.model_endpoint_credential = Some(field.text().await?.parse::<Uuid>()?);
            }
            "clear_model_endpoint_credential" => {
                self.clear_model_endpoint_credential = Some(field.text().await?.parse()?);
            }
            "add_model_endpoint_models" => {
                self.add_model_endpoint_models = Some(field.text().await?);
            }
            "model_endpoint_bans" => self.model_endpoint_bans = Some(field.text().await?),
            "model_endpoint_model_bans" => {
                self.model_endpoint_model_bans = Some(field.text().await?);
            }
            "model_endpoint_refresh_report" => {
                self.model_endpoint_refresh_report = Some(field.text().await?);
            }
            // the model profile specific fields
            "model_profile_endpoint" => {
                self.model_profile_endpoint = Some(field.text().await?.parse::<Uuid>()?);
            }
            "model_profile_model_id" => self.model_profile_model_id = Some(field.text().await?),
            "model_profile_api" => {
                // parse the protocol by its name
                let raw = field.text().await?;
                self.model_profile_api =
                    Some(ModelApi::parse_name(&raw).map_err(|msg| bad_internal!(msg))?);
            }
            "model_profile_parameters" => {
                self.model_profile_parameters = Some(field.text().await?);
            }
            "model_profile_limits" => self.model_profile_limits = Some(field.text().await?),
            "clear_model_profile_limits" => {
                self.clear_model_profile_limits = Some(field.text().await?.parse()?);
            }
            "model_profile_capabilities" => {
                self.model_profile_capabilities = Some(field.text().await?);
            }
            "clear_model_profile_capabilities" => {
                self.clear_model_profile_capabilities = Some(field.text().await?.parse()?);
            }
            "model_profile_bans" => self.model_profile_bans = Some(field.text().await?),
            // the model credential specific fields
            "model_credential_secret" => {
                self.model_credential_secret = Some(ModelSecret::from(field.text().await?));
            }
            "model_credential_session_token" => {
                self.model_credential_session_token = Some(ModelSecret::from(field.text().await?));
            }
            "clear_model_credential_session_token" => {
                self.clear_model_credential_session_token = Some(field.text().await?.parse()?);
            }
            "model_credential_expires" => {
                self.model_credential_expires = Some(field.text().await?.parse()?);
            }
            "clear_model_credential_expires" => {
                self.clear_model_credential_expires = Some(field.text().await?.parse()?);
            }
            "model_credential_bans" => self.model_credential_bans = Some(field.text().await?),
            // the agent specific fields
            "agent_config" => self.agent_config = Some(field.text().await?),
            "agent_model_auth" => self.agent_model_auth = Some(field.text().await?),
            "agent_harness" => self.agent_harness = Some(field.text().await?.parse::<Uuid>()?),
            "clear_agent_harness" => self.clear_agent_harness = Some(field.text().await?.parse()?),
            "extend_target_groups" => {
                self.extend_target_groups = Some(field.text().await?.parse()?);
            }
            // the toolset and workflow specific fields
            graph if graph.contains("toolset_") || graph.contains("workflow_") => {
                // a list field's next segment is an empty `[]`
                if self.add_graph_field(graph, field).await? {
                    ensure_empty_segment!(name_segments, name)?;
                }
            }
            // this could be a list field
            maybe_list => {
                match maybe_list {
                    // the agent edge list fields
                    "add_agent_tools" => {
                        self.add_agent_tools
                            .push(field.text().await?.parse::<Uuid>()?);
                    }
                    "remove_agent_tools" => {
                        self.remove_agent_tools
                            .push(field.text().await?.parse::<Uuid>()?);
                    }
                    "add_agent_tool_sets" => {
                        self.add_agent_tool_sets
                            .push(field.text().await?.parse::<Uuid>()?);
                    }
                    "remove_agent_tool_sets" => {
                        self.remove_agent_tool_sets
                            .push(field.text().await?.parse::<Uuid>()?);
                    }
                    "add_agent_sub_agents" => {
                        self.add_agent_sub_agents
                            .push(field.text().await?.parse::<Uuid>()?);
                    }
                    "remove_agent_sub_agents" => {
                        self.remove_agent_sub_agents
                            .push(field.text().await?.parse::<Uuid>()?);
                    }
                    "add_urls" => self.add_urls.push(field.text().await?),
                    "remove_urls" => self.remove_urls.push(field.text().await?),
                    "add_vendors" => self.add_vendors.push(field.text().await?.parse()?),
                    "remove_vendors" => self.remove_vendors.push(field.text().await?.parse()?),
                    "add_critical_sectors" => {
                        self.add_critical_sectors.push(field.text().await?.parse()?);
                    }
                    "remove_critical_sectors" => {
                        self.remove_critical_sectors
                            .push(field.text().await?.parse()?);
                    }
                    "add_countries" => {
                        // validate and parse this country
                        let country = Country::new(&field.text().await?)?;
                        // add this country to our metadata form
                        self.add_countries.push(country);
                    }
                    "remove_countries" => {
                        // validate and parse this country
                        let country = Country::new(&field.text().await?)?;
                        // add this country to our metadata form
                        self.remove_countries.push(country);
                    }
                    "add_collection_tags" => {
                        let key =
                            name_segments
                                .next()
                                .filter(|s| !s.is_empty())
                                .ok_or(bad_internal!(
                                    "Collection tag to add has a key that is empty or missing"
                                        .to_string()
                                ))?;
                        let entry = self.add_collection_tags.entry(key.to_owned()).or_default();
                        entry.insert(field.text().await?);
                    }
                    "delete_collection_tags" => {
                        let key =
                            name_segments
                                .next()
                                .filter(|s| !s.is_empty())
                                .ok_or(bad_internal!(
                                    "Collection tag to delete has a key that is empty or missing"
                                        .to_string()
                                ))?;
                        let entry = self
                            .delete_collection_tags
                            .entry(key.to_owned())
                            .or_default();
                        entry.insert(field.text().await?);
                    }
                    // the provenance list fields; legacy tool names can only be removed
                    "add_tool_ids" => {
                        self.add_tool_ids.push(field.text().await?.parse::<Uuid>()?);
                    }
                    "remove_tool_ids" => {
                        self.remove_tool_ids
                            .push(field.text().await?.parse::<Uuid>()?);
                    }
                    "remove_legacy_tools" => self.remove_legacy_tools.push(field.text().await?),
                    "functions" => self.functions.push(field.text().await?),
                    "add_sigma_applies_to" => {
                        self.add_sigma_applies_to.push(field.text().await?.parse()?);
                    }
                    "remove_sigma_applies_to" => self
                        .remove_sigma_applies_to
                        .push(field.text().await?.parse()?),
                    "add_sigma_actions" => self
                        .add_sigma_actions
                        .push(deserialize!(&field.text().await?)),
                    "remove_sigma_actions" => {
                        self.remove_sigma_actions
                            .insert(field.text().await?.parse()?);
                    }
                    // the incident specific list fields
                    "add_mission_teams" => self.add_mission_teams.push(field.text().await?),
                    "remove_mission_teams" => {
                        self.remove_mission_teams.push(field.text().await?);
                    }
                    "add_networks" => self.add_networks.push(field.text().await?),
                    "remove_networks" => self.remove_networks.push(field.text().await?),
                    "add_machines" => self.add_machines.push(field.text().await?),
                    "remove_machines" => self.remove_machines.push(field.text().await?),
                    "add_locations" => self.add_locations.push(field.text().await?),
                    "remove_locations" => self.remove_locations.push(field.text().await?),
                    // the compiled function disassembly (json serialized per element)
                    "disassembly" => self.disassembly.push(deserialize!(&field.text().await?)),
                    // the model endpoint list fields
                    "remove_model_endpoint_models" => {
                        self.remove_model_endpoint_models.push(field.text().await?);
                    }
                    "disable_model_endpoint_models" => {
                        self.disable_model_endpoint_models
                            .insert(field.text().await?);
                    }
                    // the harness tool list fields
                    "tool_provides" => self
                        .tool_provides
                        .push(field.text().await?.parse::<Uuid>()?),
                    "disable_tool_models" => {
                        self.disable_tool_models.insert(field.text().await?);
                    }
                    "enable_tool_models" => {
                        self.enable_tool_models.insert(field.text().await?);
                    }
                    "enable_model_endpoint_models" => {
                        self.enable_model_endpoint_models
                            .insert(field.text().await?);
                    }
                    // this is an invalid key so return an error
                    bad_name => {
                        return bad!(format!(
                            "'{bad_name}' is not a valid entity kind update form name"
                        ));
                    }
                }
                // this is a list field, so make sure the next segment is an empty `[]`
                ensure_empty_segment!(name_segments, name)?;
            }
        }
        Ok(name_segments)
    }

    /// Build an association request for anything in this entity form
    ///
    /// A device's new vendors are written only after the user is authorized to write the device's
    /// associations. An agent's, toolset's, or workflow's edges are checked together before any are
    /// written.
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is updating this entity
    /// * `entity` - The entity whose associations are being updated
    /// * `shared` - Shared Thorium objects
    pub(super) async fn update_associations(
        &mut self,
        user: &User,
        entity: &mut Entity,
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // build the association request for each of our different entities
        match &entity.metadata {
            EntityMetadata::Device(_) => {
                // we only have to update vendors for device updates
                if !self.add_vendors.is_empty() {
                    // devices to vendor relationships is always developed by
                    let assoc_kind = AssociationKind::DevelopedBy;
                    // get the numer of associations to make
                    let vendor_len = self.add_vendors.len();
                    // build our source target
                    let source = AssociationTarget::Entity {
                        id: entity.id,
                        name: entity.name.clone(),
                    };
                    // make sure we may write this device's associations before anything is written
                    AssociationRequest::authorize_source_groups(user, &entity.groups, shared)
                        .await?;
                    // start with an empty req
                    let mut req = AssociationRequest::with_capacity(assoc_kind, source, vendor_len);
                    // step over the vendors we are associating with this device
                    for vendor_id in self.add_vendors.drain(..) {
                        // get the entity for this vendor if it exists
                        let other_entity = Entity::get(user, vendor_id, shared).await?;
                        // make sure this is a vendor entity
                        if other_entity.kind != EntityKinds::Vendor {
                            return bad!(format!("Entity {vendor_id} is not a vendor!"));
                        }
                        // build the source object for this entity
                        let other = AssociationTarget::Entity {
                            id: vendor_id,
                            name: other_entity.name,
                        };
                        // add this link to the other
                        req.targets.push(other);
                    }
                    // create these associations
                    req.apply(user, shared).await?;
                }
                // if we have any associations to delete then do that
                if !self.remove_vendors.is_empty() {
                    // convert out list of vendors to a list of serialized target columns
                    let serialized = self
                        .remove_vendors
                        .iter()
                        .filter_map(|id| {
                            // this should never actually fail
                            serde_json::to_string(&AssociationTargetColumn::Entity(*id)).ok()
                        })
                        .collect::<Vec<String>>();
                    // build our association target column
                    let Some(source) = entity.build_association_target_column() else {
                        // this should be impossible and never occur
                        return internal_err!(format!(
                            "Failed to build association target column for {entity:#?}"
                        ));
                    };
                    // build the opts for listing associations
                    let opts = AssociationListOpts::default()
                        .groups(user.groups.clone())
                        .limit(100);
                    // list associations for this entity scoped to the requesting user
                    let mut cursor = db::associations::list(user, opts, &source, shared).await?;
                    // build a list of associations to remove
                    let mut remove_assoc = Vec::with_capacity(self.remove_vendors.len());
                    // step over all of our associations and find the ones to delete
                    loop {
                        // filter down to just the vendors that we want to remove
                        for assoc in cursor.data.drain(..) {
                            // determine if this is a vendor association
                            if assoc.kind == AssociationKind::DevelopedBy
                                && serialized.contains(&assoc.other)
                            {
                                // add this association that we want to remove
                                remove_assoc.push(assoc);
                            }
                        }
                        // check if this cursor is exhausted
                        if cursor.exhausted() {
                            break;
                        }
                        // get the next page of data in this cusor
                        cursor.next(shared).await?;
                    }
                    // get our source target column
                    if let Some(source) = entity.build_association_target_column() {
                        // delete all of the requested associations
                        db::associations::delete_many(&source, &remove_assoc, shared).await?;
                    }
                }
                Ok(())
            }
            // an agent's edges are checked together before any is written
            EntityMetadata::Agent(_) => {
                helpers::heap_future(|| agents::update_edges(user, entity, self, shared)).await
            }
            // a toolset's includes are checked together before any is written
            EntityMetadata::ToolSet(_) => {
                helpers::heap_future(|| toolsets::update_edges(user, entity, self, shared)).await
            }
            // a workflow's agents are checked together before any is written
            EntityMetadata::WorkflowCatalog(_) => {
                helpers::heap_future(|| workflows::update_edges(user, entity, self, shared)).await
            }
            _ => Ok(()),
        }
    }
}

impl TryFrom<EntityRow> for Entity {
    type Error = ApiError;

    fn try_from(row: EntityRow) -> Result<Self, Self::Error> {
        // return the entity with just the single group from this row and no tags
        Ok(Self {
            id: row.id,
            name: row.name,
            kind: row.kind,
            metadata: row.metadata,
            groups: vec![row.group],
            created: row.created,
            submitter: row.submitter,
            description: row.description,
            tags: TagMap::with_capacity(1),
            image: row.image,
        })
    }
}

// Implement cursor support for entities
impl CursorCore for EntityListLine {
    /// The params to build this cursor from
    type Params = EntityListParams;

    /// Filter by entity kind
    type ExtraFilters = Vec<EntityKinds>;

    /// The type of data to group our rows by
    type GroupBy = String;

    fn partition_size(shared: &Shared) -> u16 {
        shared.config.thorium.entities.partition_size
    }

    fn get_id(params: &Self::Params) -> Option<Uuid> {
        params.cursor
    }

    fn get_start_end(
        params: &Self::Params,
        shared: &Shared,
    ) -> Result<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>), ApiError> {
        // get our end timestmap
        let end = params.end(shared)?;
        Ok((params.start, end))
    }

    fn get_group_by(params: &mut Self::Params) -> Vec<Self::GroupBy> {
        std::mem::take(&mut params.groups)
    }

    fn get_extra_filters(params: &mut Self::Params) -> Self::ExtraFilters {
        std::mem::take(&mut params.kinds)
    }

    fn get_tag_filters(
        params: &mut Self::Params,
    ) -> Option<(TagType, HashMap<String, Vec<String>>)> {
        // Only return tags if some were set
        if params.tags.is_empty() {
            None
        } else {
            Some((TagType::Entities, params.tags.clone()))
        }
    }

    /// Get the max number of rows to return, checked against the most a listing allows
    ///
    /// # Arguments
    ///
    /// * `params` - The params to use to build this cursor
    fn get_limit(params: &Self::Params) -> Result<CursorLimit, ApiError> {
        // make sure this limit is one we can serve
        Ok(CursorLimit::from_user(
            params.limit,
            DEFAULT_LIMIT_CAP,
            "limit",
        )?)
    }

    fn dedupe_item(&self, dedupe_set: &mut HashSet<String>) -> bool {
        let id = self.id.to_string();
        // if this is already in our dedupe set then skip it
        if dedupe_set.contains(&id) {
            // we already have this sample so skip it
            false
        } else {
            // add this new sample to our dedupe set
            dedupe_set.insert(id);
            // keep this new sample
            true
        }
    }

    /// Get the tag clustering key for a row without the timestamp
    fn get_tag_clustering_key(&self) -> String {
        // tags are clustered by the entity id they tag, stored as text
        self.id.to_string()
    }
}

impl ScyllaCursorSupport for EntityListLine {
    type IntermediateRow = EntityListRow;

    /// Entity list pages are read with the generated [`EntitiesListCursor`] statements
    type Statements = EntitiesListCursor;

    type UniqueType<'a> = Uuid;

    /// Returns the prepared entity list cursor statements
    ///
    /// # Arguments
    ///
    /// * `shared` - Shared Thorium objects holding the prepared statements
    fn statements(shared: &Shared) -> &Self::Statements {
        &shared.scylla.queries.entities.list
    }

    /// Add an item to our tag tie breaker map, keeping the tie a group already has
    ///
    /// # Arguments
    ///
    /// * `ties` - Our current ties
    fn add_tag_tie(&self, ties: &mut HashMap<String, String>) {
        // if its not already in the tie map then add each of its groups to our map
        for group in &self.groups {
            // keep the tie of an earlier row in this group, since rows are consumed in order
            ties.entry(group.clone())
                .or_insert_with(|| self.id.to_string());
        }
    }

    fn get_timestamp(&self) -> DateTime<Utc> {
        // return the created time as the timestamp
        self.created
    }

    fn get_intermediate_unique_key(intermediate: &Self::IntermediateRow) -> Self::UniqueType<'_> {
        // the entity's id is a unique key
        intermediate.id
    }

    fn get_unique_key(&self) -> Self::UniqueType<'_> {
        // the entity's id is a unique key
        self.id
    }

    fn add_group_to_line(&mut self, group: String) {
        self.groups.insert(group);
    }

    fn add_intermediate_to_line(&mut self, intermediate: Self::IntermediateRow) {
        self.groups.insert(intermediate.group);
    }

    fn from_tag_row(row: TagListRow) -> Self {
        Self::from(row)
    }

    /// Build the keys of the census streams that list entities
    ///
    /// # Arguments
    ///
    /// * `group_by` - The values to group our the rows by
    /// * `_extra` - The extra values required to list this data
    /// * `year` - The year we are looking for census data for
    /// * `bucket` - The newest bucket to look for census data from
    /// * `keys` - The vec to add our census stream keys too
    /// * `shared` - Shared Thorium objects
    fn census_keys<'a>(
        group_by: &'a [Self::GroupBy],
        _extra: &Self::ExtraFilters,
        year: i32,
        bucket: Bucket,
        keys: &mut Vec<(&'a Self::GroupBy, String, Bucket)>,
        shared: &Shared,
    ) {
        // build the keys for each census stream we are going to crawl
        for group in group_by {
            // build the key for this entities census stream
            let key = format!(
                "{namespace}:census:entities:stream:{group}:{year}",
                namespace = shared.config.thorium.namespace,
                group = group,
                year = year,
            );
            // add this key to our keys
            keys.push((group, key, bucket));
        }
    }
}

impl From<EntityListRow> for EntityListLine {
    fn from(row: EntityListRow) -> Self {
        Self {
            groups: HashSet::from([row.group]),
            name: row.name,
            id: row.id,
            kind: row.kind,
            created: row.created,
        }
    }
}

impl From<TagListRow> for EntityListLine {
    /// Convert a tag list row to an entity list line
    ///
    /// # Panics
    ///
    /// Panics if the tag row's item is not the entity's UUID as it should be
    #[allow(clippy::expect_used)]
    fn from(row: TagListRow) -> Self {
        // build our initial group set
        let mut groups = HashSet::with_capacity(1);
        // add this group
        groups.insert(row.group);
        // parse the id from the row item column
        let id = Uuid::parse_str(&row.item).expect("Failed to parse UUID from tag row item");
        // build our repo list line
        Self {
            groups,
            id,
            // set defaults for name/kind because the tag rows don't have them;
            // we'll get them from the entities materialized view after we've
            // finished listing this round
            name: String::default(),
            kind: EntityKinds::default(),
            created: row.uploaded,
        }
    }
}

impl ApiCursor<EntityListLine> {
    /// Turns a cursor of [`EntityListLine`] into a cursor of [`Entity`]
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is getting the details for this list
    /// * `shared` - Shared Thorium objects
    #[instrument(
        name = "ApiCursor<EntityListLine>::details",
        skip_all
        err(Debug)
    )]
    pub(crate) async fn details(
        self,
        user: &User,
        shared: &Shared,
    ) -> Result<ApiCursor<Entity>, ApiError> {
        // build a list of entity names we need to get details on
        let ids = self
            .data
            .into_iter()
            .map(|line| line.id)
            .collect::<Vec<_>>();
        // use correct backend to list sample details
        let data = for_groups!(db::entities::get_many, user, shared, &ids)?;
        // get any required association data for the entities we found
        let mut populated = Vec::with_capacity(data.len());
        // create a stream of entities to populate with any association data
        let mut populator_stream = stream::iter(data)
            .map(|entity| entity.populate_associations(user, shared))
            .buffered(20);
        // get populated items from our stream until we hit a problem or no more exist
        while let Some(entity) = populator_stream.next().await {
            // raise an error if we ran into a problem
            populated.push(entity?);
        }
        // build our new cursor object
        Ok(ApiCursor {
            cursor: self.cursor,
            data: populated,
            metadata: None,
        })
    }
}

impl<S> FromRequestParts<S> for EntityListParams
where
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        // try to extract our query
        if let Some(query) = parts.uri.query() {
            // try to deserialize our query string
            Ok(serde_qs::Config::new()
                .max_depth(5)
                .deserialize_str(query)?)
        } else {
            // provide default params if none were given
            Ok(Self::default())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal entity with the given submitter for tag tests
    fn test_entity(submitter: &str) -> Entity {
        Entity {
            id: Uuid::nil(),
            name: "test-entity".to_owned(),
            kind: EntityKinds::Other,
            metadata: EntityMetadata::Other,
            description: None,
            submitter: submitter.to_owned(),
            groups: Vec::new(),
            tags: TagMap::default(),
            image: None,
            created: DateTime::<Utc>::UNIX_EPOCH,
        }
    }

    /// Every entity should be auto-tagged with its submitter so it can be
    /// queried by submitter via the `tags` list filter
    #[test]
    fn populate_intrinsic_tags_tags_submitter() {
        let entity = test_entity("alice");
        let mut tags: HashMap<String, HashSet<String>> = HashMap::new();
        entity
            .populate_intrinsic_tags(&mut tags)
            .expect("failed to populate intrinsic tags");
        // the submitter should be present under the "submitter" tag key
        let submitters = tags
            .get("submitter")
            .expect("submitter tag was not populated");
        assert!(submitters.contains("alice"));
    }

    /// Dropping a toolset's associated data clears its tools and latest instance, so its stored
    /// kind data holds neither
    #[test]
    fn drop_associated_data_clears_toolset_fields() {
        // a toolset with an included tool and a latest instance
        let neighbor = test_entity("io");
        let mut tool_set = test_entity("io");
        tool_set.kind = EntityKinds::ToolSet;
        let latest = Uuid::new_v4();
        tool_set.metadata = EntityMetadata::ToolSet(crate::models::ToolSetEntity {
            tools: vec![neighbor],
            latest_instance: Some(latest),
            ..crate::models::ToolSetEntity::default()
        });
        // drop the associated data
        tool_set.drop_associated_data();
        // the serialized kind data holds neither field's values
        let (_, data) = tool_set.metadata.split().unwrap();
        let stored: serde_json::Value = serde_json::from_str(&data.unwrap()).unwrap();
        assert_eq!(stored["tools"], serde_json::json!([]), "{stored}");
        assert!(stored.get("latest_instance").is_none(), "{stored}");
        assert!(
            !stored.to_string().contains(&latest.to_string()),
            "{stored}"
        );
    }

    /// Dropping a workflow's associated data clears its agents but keeps the newest and latest
    /// instances it stores
    #[test]
    fn drop_associated_data_clears_workflow_fields() {
        // a workflow with an agent and its newest and latest instances
        let agent = test_entity("io");
        let mut workflow = test_entity("io");
        workflow.kind = EntityKinds::WorkflowCatalog;
        let (latest, newest) = (Uuid::new_v4(), Uuid::new_v4());
        workflow.metadata = EntityMetadata::WorkflowCatalog(crate::models::WorkflowCatalogEntity {
            agents: vec![agent.clone()],
            latest_instance: Some(latest),
            newest_instance: Some(newest),
            ..crate::models::WorkflowCatalogEntity::default()
        });
        // drop the associated data
        workflow.drop_associated_data();
        // the serialized kind data holds no agent but keeps both instances
        let (kind, data) = workflow.metadata.split().unwrap();
        assert_eq!(kind, EntityKinds::WorkflowCatalog);
        let stored: serde_json::Value = serde_json::from_str(&data.unwrap()).unwrap();
        assert_eq!(stored["agents"], serde_json::json!([]), "{stored}");
        assert_eq!(stored["latest_instance"], latest.to_string(), "{stored}");
        assert_eq!(stored["newest_instance"], newest.to_string(), "{stored}");
        assert!(
            !stored.to_string().contains(&agent.id.to_string()),
            "{stored}"
        );
    }

    /// Dropping an agent's associated data clears every populated edge, so its stored kind data
    /// holds none of them
    #[test]
    fn drop_associated_data_clears_agent_edges() {
        // an agent with one entity in each populated edge field
        let neighbor = test_entity("io");
        let mut agent = test_entity("io");
        agent.kind = EntityKinds::Agent;
        agent.metadata = EntityMetadata::Agent(crate::models::AgentEntity {
            harness: Some(Box::new(neighbor.clone())),
            latest_tools: vec![neighbor.clone()],
            pinned_tools: vec![neighbor.clone()],
            tool_sets: vec![neighbor.clone()],
            sub_agents: vec![neighbor],
            ..crate::models::AgentEntity::default()
        });
        // drop the associated data
        agent.drop_associated_data();
        // the serialized kind data holds no edge targets
        let (_, data) = agent.metadata.split().unwrap();
        let stored: serde_json::Value = serde_json::from_str(&data.unwrap()).unwrap();
        assert!(stored.get("harness").is_none(), "{stored}");
        for field in ["latest_tools", "pinned_tools", "tool_sets", "sub_agents"] {
            assert_eq!(stored[field], serde_json::json!([]), "{field}");
        }
        assert!(!stored.to_string().contains("test-entity"), "{stored}");
    }
}
