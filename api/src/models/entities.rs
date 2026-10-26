//! Structures related to Thorium "Entities"

use chrono::{DateTime, Utc};
use gxhash::GxHasher;
use linearize::Linearize;
use schemars::JsonSchema;
use std::hash::Hasher;
use std::net::IpAddr;
use std::{collections::HashSet, hash::Hash};
use strum::{AsRefStr, EnumDiscriminants, EnumString, IntoEnumIterator};
use uuid::Uuid;

use crate::models::{
    CollectionEntity, CollectionEntityRequest, CollectionKind, Country, DeviceEntityRequest,
    TagMap, TreeSupport, VendorEntity, VendorEntityRequest,
};

pub mod agents;
pub mod collections;
pub mod countries;
pub mod devices;
#[cfg(any(feature = "api", feature = "client"))]
pub mod digests;
pub mod filesystem;
pub mod flags;
pub mod functions;
pub mod incident;
pub mod models;
pub mod network_activity;
pub mod pe;
pub mod processes;
pub mod rules;
pub mod shared;
pub mod tools;
pub mod toolsets;
pub mod vendors;
pub mod workflows;

use agents::{AgentEntity, AgentEntityRequest};
use devices::DeviceEntity;
use filesystem::{FileSystemEntity, FileSystemFolderEntity};
use flags::Confidence;
use flags::Flag;
use functions::{CompiledFunction, CompiledInstruction, DecompiledFunction};
use incident::{Incident, IncidentRequest};
use models::{
    ModelCredentialEntity, ModelCredentialEntityRequest, ModelEndpointEntity,
    ModelEndpointEntityRequest, ModelProfileEntity, ModelProfileEntityRequest,
};
use network_activity::NetworkConnection;
use pe::{PeImportEntity, PeSectionEntity};
use processes::{WindowsProcessEntity, WindowsProcessTreeEntity};
use rules::{SigmaRule, SigmaRuleAppliesTo};
use tools::{ToolEntity, ToolEntityRequest, ToolInstanceEntity, ToolInstanceEntityRequest};
use toolsets::{ToolSetEntity, ToolSetEntityRequest, ToolSetInstanceEntityRequest};
use workflows::{
    WorkflowCatalogEntity, WorkflowCatalogEntityRequest, WorkflowInstanceEntity,
    WorkflowInstanceEntityRequest,
};

// api/client imports
cfg_if::cfg_if! {
    if #[cfg(any(feature = "api", feature = "client"))] {
        use std::collections::HashMap;

        use super::TagType;
        use super::backends::TagSupport;
        use crate::models::scylla_utils::keys::KeySupport;
        use crate::{multipart_list, multipart_text, multipart_set};
    }
}

cfg_if::cfg_if! {
    if #[cfg(feature = "api")] {
        use std::collections::BTreeSet;
        use futures::stream::{self, StreamExt};
        use chrono::TimeZone;

        use network_activity::{NetConState, TransportLayerProtocol};
        use rules::SigmaActionToTake;
        use shared::CriticalSector;
        use super::{TagRequest, User, TagDeleteRequest, Group, GroupAllowAction, UnhashedTreeBranch};
        use crate::utils::{ApiError, Shared};
        use crate::models::Tree;

        /// The form for entity metadata
        #[derive(Debug, Default)]
        pub struct EntityMetadataForm {
            pub urls: Vec<String>,
            pub vendors: Vec<Uuid>,
            pub critical_system: Option<bool>,
            pub sensitive_location: Option<bool>,
            pub critical_sectors: BTreeSet<CriticalSector>,
            pub countries: BTreeSet<Country>,
            pub collection_kind: Option<CollectionKind>,
            pub collection_tags: HashMap<String, HashSet<String>>,
            pub collection_tags_case_insensitive: Option<bool>,
            pub collection_ignore_groups: Option<bool>,
            pub collection_start: Option<DateTime<Utc>>,
            pub collection_end: Option<DateTime<Utc>>,
            pub sha256: Option<String>,
            pub filesystem_id: Option<Uuid>,
            pub names_sha256: Option<String>,
            pub data_sha256: Option<String>,
            pub all_sha256: Option<String>,
            /// The tool instances that produced a filesystem, process tree, or decompiled function
            pub tool_ids: Vec<Uuid>,
            pub pid: Option<u64>,
            pub parent_pid: Option<u64>,
            pub name: Option<String>,
            pub image_path: Option<String>,
            pub command: Option<String>,
            pub offset: Option<u64>,
            pub threads: Option<u32>,
            pub handles: Option<u32>,
            pub is_wow64: Option<bool>,
            pub session_id: Option<u32>,
            pub create_time: Option<DateTime<Utc>>,
            pub exit_time: Option<DateTime<Utc>>,
            pub protocol: Option<TransportLayerProtocol>,
            pub source: Option<IpAddr>,
            pub source_port: Option<u16>,
            pub destination: Option<IpAddr>,
            pub destination_port: Option<u16>,
            pub state: Option<NetConState>,
            pub process: Option<String>,
            /// A sigma rule in yaml format
            pub sigma_rule: Option<String>,
            /// What this sigma rule applies too
            pub sigma_applies_to: Vec<SigmaRuleAppliesTo>,
            /// The action to take when a sigma rule hits
            pub sigma_actions: Vec<SigmaActionToTake>,
            /// The score that a rule applies
            pub score: Option<i64>,
            /// How suspicious this flag is where higher numbers are more suspicious
            pub suspicion: Option<i64>,
            /// How confident/reliable this flag is
            pub confidence: Option<Confidence>,
            /// The interesting, odd, or suspicious characteristic
            pub content: Option<String>,
            /// The reason for this Flag
            pub reasoning: Option<String>,
            /// The cover term used for this incident
            pub cover_term: Option<String>,
            /// The mission teams involved in this incident
            pub mission_teams: Vec<String>,
            /// The networks this incident was on
            pub networks: Vec<String>,
            /// The machines this incident was found on
            pub machines: Vec<String>,
            /// The locations this incident is from
            pub locations: Vec<String>,
            /// The address this function is located at
            pub function_address: Option<u64>,
            /// The dissasembled instructions for this function
            pub disassembly: Vec<CompiledInstruction>,
            /// The decompiled content for a function
            pub decompilation_content: Option<String>,
            /// The MD5 of a PE section's raw data
            pub md5: Option<String>,
            /// The raw (on disk) size of a PE section in bytes
            pub raw_size: Option<u64>,
            /// The virtual (in memory) size of a PE section in bytes
            pub virtual_size: Option<u64>,
            /// The Shannon entropy of a PE section's data
            pub entropy: Option<f64>,
            /// The functions imported from a PE import's library
            pub functions: Vec<String>,
            /// The kind of tool to create
            pub tool_kind: Option<tools::ToolKind>,
            /// The version label of a new tool instance
            pub tool_version: Option<String>,
            /// The JSON encoded spec of a new tool instance
            pub tool_spec: Option<String>,
            /// The tool a directly created tool instance belongs to
            pub tool_instance_tool: Option<Uuid>,
            /// The native agentic tool instances a new harness instance provides
            pub tool_provides: Vec<Uuid>,
            /// The JSON encoded env values of a new harness tool
            pub tool_harness_env: Option<String>,
            /// The model ids no run of a new harness tool may resolve to
            pub tool_disabled_models: BTreeSet<String>,
            /// The JSON encoded provider of a new model endpoint
            pub model_endpoint_provider: Option<String>,
            /// The group credential a new model endpoint authenticates with
            pub model_endpoint_credential: Option<Uuid>,
            /// The JSON encoded catalog entries of a new model endpoint
            pub model_endpoint_models: Option<String>,
            /// The model ids a new model endpoint never resolves to
            pub model_endpoint_disabled_models: BTreeSet<String>,
            /// The endpoint a new model profile calls
            pub model_profile_endpoint: Option<Uuid>,
            /// The model id a new model profile calls
            pub model_profile_model_id: Option<String>,
            /// The protocol a new model profile calls its model with
            pub model_profile_api: Option<models::ModelApi>,
            /// The JSON encoded parameters of a new model profile
            pub model_profile_parameters: Option<String>,
            /// The JSON encoded limits of a new model profile
            pub model_profile_limits: Option<String>,
            /// The JSON encoded capabilities of a new model profile
            pub model_profile_capabilities: Option<String>,
            /// The JSON encoded kind of a new model credential
            pub model_credential_kind: Option<String>,
            /// The primary secret of a new model credential
            pub model_credential_secret: Option<models::ModelSecret>,
            /// The session token of a new model credential
            pub model_credential_session_token: Option<models::ModelSecret>,
            /// When a new model credential stops working
            pub model_credential_expires: Option<DateTime<Utc>>,
            /// The JSON encoded config of a new agent
            pub agent_config: Option<String>,
            /// The JSON encoded model auth levels of a new agent
            pub agent_model_auth: Option<String>,
            /// The harness a new agent runs in
            pub agent_harness: Option<Uuid>,
            /// The tools and tool instances a new agent calls
            pub agent_tools: Vec<Uuid>,
            /// The toolsets and toolset instances a new agent calls
            pub agent_tool_sets: Vec<Uuid>,
            /// The agents a new agent delegates to
            pub agent_sub_agents: Vec<Uuid>,
            /// Whether to widen edge targets that miss some of the entity's groups
            pub extend_target_groups: Option<bool>,
            /// The tools and tool instances a new toolset includes
            pub toolset_tools: Vec<Uuid>,
            /// Whether a new toolset only includes read-only tools
            pub toolset_read_only: Option<bool>,
            /// The tag key of a new toolset's selector
            pub toolset_selector_key: Option<String>,
            /// The tag value of a new toolset's selector
            pub toolset_selector_value: Option<String>,
            /// Whether new instances of a new toolset are built automatically
            pub toolset_auto_instance: Option<bool>,
            /// The JSON encoded toolset instance being submitted
            pub toolset_instance: Option<String>,
            /// The agents a new workflow orchestrates
            pub workflow_agents: Vec<Uuid>,
            /// The agent that receives a new workflow's inputs first
            pub workflow_entry_agent: Option<Uuid>,
            /// Whether new instances of a new workflow are built automatically
            pub workflow_auto_instance: Option<bool>,
            /// The JSON encoded run-wide limits of a new workflow
            pub workflow_limits: Option<String>,
            /// The JSON encoded default models of a new workflow
            pub workflow_default_models: Option<String>,
            /// The JSON encoded model auth levels of a new workflow
            pub workflow_model_auth: Option<String>,
            /// The JSON encoded workflow instance being submitted
            pub workflow_instance: Option<String>,
        }

        impl EntityMetadataForm {
            /// Ensure the data in the entity metadata form is valid
            ///
            /// # Errors
            ///
            /// - `collection_start` is older than `collection_end`
            pub fn validate(&self) -> Result<(), ApiError> {
                // ensure start is newer than end
                if let (Some(start), Some(end)) = (
                    self.collection_start.as_ref(),
                    self.collection_end.as_ref(),
                ) && start < end {
                    return crate::bad!(format!(
                        "Start must be more recent than end: Start '{start}' < End '{end}'"
                    ));
                }
                Ok(())
            }
        }

        /// A request to create a new entity
        #[derive(Debug, Default)]
        pub struct EntityForm {
            /// The entity's name
            pub name: Option<String>,
            /// The kind of entity this is
            pub kind: Option<EntityKinds>,
            /// The metadata for this specific entity kind
            pub metadata: EntityMetadataForm,
            /// The groups this entity should be in
            pub groups: Vec<String>,
            /// The tags for this entity
            pub tags: HashMap<String, HashSet<String>>,
            /// A description of this entity
            pub description: Option<String>,
            /// This entities image
            pub image: Option<String>,
        }

        impl EntityForm {
            /// Ensure the data in the entity metadata form is valid
            pub fn validate(&self) -> Result<(), ApiError> {
                self.metadata.validate()?;
                Ok(())
            }
        }

        /// Fields from the multipart form for updating an entity
        #[derive(Debug, Default)]
        pub struct EntityUpdateForm {
            pub name: Option<String>,
            pub metadata: EntityMetadataUpdateForm,
            pub add_groups: Vec<String>,
            pub remove_groups: Vec<String>,
            pub clear_image: Option<bool>,
            /// A description of this entity
            pub description: Option<String>,
            pub clear_description: Option<bool>
        }

        /// The form for updating entity metadata
        #[derive(Debug, Default)]
        pub struct EntityMetadataUpdateForm {
            pub add_urls: Vec<String>,
            pub remove_urls: Vec<String>,
            pub add_vendors: Vec<Uuid>,
            pub remove_vendors: Vec<Uuid>,
            pub critical_system: Option<bool>,
            pub clear_critical_system: Option<bool>,
            pub sensitive_location: Option<bool>,
            pub clear_sensitive_location: Option<bool>,
            pub add_critical_sectors: Vec<CriticalSector>,
            pub remove_critical_sectors: Vec<CriticalSector>,
            pub add_countries: Vec<Country>,
            pub remove_countries: Vec<Country>,
            pub add_collection_tags: HashMap<String, HashSet<String>>,
            pub delete_collection_tags: HashMap<String, HashSet<String>>,
            pub collection_tags_case_insensitive: Option<bool>,
            pub collection_ignore_groups: Option<bool>,
            pub collection_start: Option<DateTime<Utc>>,
            pub collection_end: Option<DateTime<Utc>>,
            pub clear_collection_start: Option<bool>,
            pub clear_collection_end: Option<bool>,
            /// The tool instances to add to a filesystem, process tree, or decompiled function
            pub add_tool_ids: Vec<Uuid>,
            /// The tool instances to remove from a filesystem, process tree, or decompiled function
            pub remove_tool_ids: Vec<Uuid>,
            /// The legacy tool names to remove from a filesystem, process tree, or decompiled
            /// function
            pub remove_legacy_tools: Vec<String>,
            pub name: Option<String>,
            pub image_path: Option<String>,
            pub command: Option<String>,
            pub offset: Option<u64>,
            pub threads: Option<u32>,
            pub handles: Option<u32>,
            pub is_wow64: Option<bool>,
            pub session_id: Option<u32>,
            pub create_time: Option<DateTime<Utc>>,
            pub exit_time: Option<DateTime<Utc>>,
            pub protocol: Option<TransportLayerProtocol>,
            pub source: Option<IpAddr>,
            pub source_port: Option<u16>,
            pub destination: Option<IpAddr>,
            pub destination_port: Option<u16>,
            pub state: Option<NetConState>,
            pub pid: Option<u64>,
            pub process: Option<String>,
            /// A sigma rule in yaml format
            pub sigma_rule: Option<String>,
            /// The new things this sigma rule should apply too
            pub add_sigma_applies_to: Vec<SigmaRuleAppliesTo>,
            /// The things things sigma rule should no longer apply too
            pub remove_sigma_applies_to: Vec<SigmaRuleAppliesTo>,
            /// The new actions to take when a sigma rule hits
            pub add_sigma_actions: Vec<SigmaActionToTake>,
            /// The actions to remove by their index in this vec
            pub remove_sigma_actions: BTreeSet<usize>,
            /// The score that a rule applies
            pub score: Option<i64>,
            /// How suspicious this flag is where higher numbers are more suspicious
            pub suspicion: Option<i64>,
            /// How confident/reliable this flag is
            pub confidence: Option<Confidence>,
            /// The interesting, odd, or suspicious characteristic
            pub content: Option<String>,
            /// The reason for this Flag
            pub reasoning: Option<String>,
            /// The cover term used for this incident
            pub cover_term: Option<String>,
            /// The mission teams to add to this incident
            pub add_mission_teams: Vec<String>,
            /// The mission teams to remove from this incident
            pub remove_mission_teams: Vec<String>,
            /// The networks to add to this incident
            pub add_networks: Vec<String>,
            /// The networks to remove from this incident
            pub remove_networks: Vec<String>,
            /// The machines to add to this incident
            pub add_machines: Vec<String>,
            /// The machines to remove from this incident
            pub remove_machines: Vec<String>,
            /// The locations to add to this incident
            pub add_locations: Vec<String>,
            /// The locations to remove from this incident
            pub remove_locations: Vec<String>,
            /// The address this function is located at
            pub function_address: Option<u64>,
            /// The dissasembled instructions for this function
            pub disassembly: Vec<CompiledInstruction>,
            /// The decompiled content for a function
            pub decompilation_content: Option<String>,
            /// The MD5 of a PE section's raw data
            pub md5: Option<String>,
            /// The raw (on disk) size of a PE section in bytes
            pub raw_size: Option<u64>,
            /// The virtual (in memory) size of a PE section in bytes
            pub virtual_size: Option<u64>,
            /// The Shannon entropy of a PE section's data
            pub entropy: Option<f64>,
            /// The functions to set for a PE import's library
            pub functions: Vec<String>,
            /// The version label of a tool instance to publish
            pub tool_version: Option<String>,
            /// The JSON encoded spec of a tool instance to publish
            pub tool_spec: Option<String>,
            /// Whether a tool instance is deprecated
            pub tool_instance_deprecated: Option<bool>,
            /// The native agentic tool instances a new harness instance provides
            pub tool_provides: Vec<Uuid>,
            /// The JSON encoded new env values of a harness tool
            pub tool_harness_env: Option<String>,
            /// Clear a harness tool's env values
            pub clear_tool_harness_env: Option<bool>,
            /// The model ids to add to a harness tool's disabled models
            pub disable_tool_models: BTreeSet<String>,
            /// The model ids to remove from a harness tool's disabled models
            pub enable_tool_models: BTreeSet<String>,
            /// The JSON encoded new provider of a model endpoint
            pub model_endpoint_provider: Option<String>,
            /// The new group credential of a model endpoint
            pub model_endpoint_credential: Option<Uuid>,
            /// Clear a model endpoint's group credential
            pub clear_model_endpoint_credential: Option<bool>,
            /// The JSON encoded catalog entries to add to a model endpoint
            pub add_model_endpoint_models: Option<String>,
            /// The model ids to remove from a model endpoint's catalog
            pub remove_model_endpoint_models: Vec<String>,
            /// The model ids to add to a model endpoint's disabled models
            pub disable_model_endpoint_models: BTreeSet<String>,
            /// The model ids to remove from a model endpoint's disabled models
            pub enable_model_endpoint_models: BTreeSet<String>,
            /// The JSON encoded ban update for a model endpoint
            pub model_endpoint_bans: Option<String>,
            /// The JSON encoded ban updates for a model endpoint's catalog entries
            pub model_endpoint_model_bans: Option<String>,
            /// The JSON encoded listing result to merge into a model endpoint's catalog
            pub model_endpoint_refresh_report: Option<String>,
            /// The new endpoint of a model profile
            pub model_profile_endpoint: Option<Uuid>,
            /// The new model id of a model profile
            pub model_profile_model_id: Option<String>,
            /// The new protocol of a model profile
            pub model_profile_api: Option<models::ModelApi>,
            /// The JSON encoded new parameters of a model profile
            pub model_profile_parameters: Option<String>,
            /// The JSON encoded new limits of a model profile
            pub model_profile_limits: Option<String>,
            /// Clear a model profile's limits
            pub clear_model_profile_limits: Option<bool>,
            /// The JSON encoded new capabilities of a model profile
            pub model_profile_capabilities: Option<String>,
            /// Clear a model profile's capabilities
            pub clear_model_profile_capabilities: Option<bool>,
            /// The JSON encoded ban update for a model profile
            pub model_profile_bans: Option<String>,
            /// The new primary secret of a model credential
            pub model_credential_secret: Option<models::ModelSecret>,
            /// The new session token of a model credential
            pub model_credential_session_token: Option<models::ModelSecret>,
            /// Clear a model credential's session token
            pub clear_model_credential_session_token: Option<bool>,
            /// When a model credential stops working
            pub model_credential_expires: Option<DateTime<Utc>>,
            /// Clear a model credential's expiration
            pub clear_model_credential_expires: Option<bool>,
            /// The JSON encoded ban update for a model credential
            pub model_credential_bans: Option<String>,
            /// The JSON encoded new config of an agent
            pub agent_config: Option<String>,
            /// The JSON encoded new model auth levels of an agent
            pub agent_model_auth: Option<String>,
            /// The harness to set for an agent
            pub agent_harness: Option<Uuid>,
            /// Clear an agent's harness
            pub clear_agent_harness: Option<bool>,
            /// The tools and tool instances to add to an agent
            pub add_agent_tools: Vec<Uuid>,
            /// The tools and tool instances to remove from an agent
            pub remove_agent_tools: Vec<Uuid>,
            /// The toolsets and toolset instances to add to an agent
            pub add_agent_tool_sets: Vec<Uuid>,
            /// The toolsets and toolset instances to remove from an agent
            pub remove_agent_tool_sets: Vec<Uuid>,
            /// The agents to add as an agent's sub-agents
            pub add_agent_sub_agents: Vec<Uuid>,
            /// The agents to remove from an agent's sub-agents
            pub remove_agent_sub_agents: Vec<Uuid>,
            /// Whether to widen the targets of added edges that miss some of the entity's groups
            pub extend_target_groups: Option<bool>,
            /// Whether a toolset only includes read-only tools
            pub toolset_read_only: Option<bool>,
            /// The new tag key of a toolset's selector
            pub toolset_selector_key: Option<String>,
            /// The new tag value of a toolset's selector
            pub toolset_selector_value: Option<String>,
            /// Clear a toolset's selector
            pub clear_toolset_selector: Option<bool>,
            /// Whether new instances of a toolset are built automatically
            pub toolset_auto_instance: Option<bool>,
            /// The tools and tool instances to add to a toolset
            pub add_toolset_tools: Vec<Uuid>,
            /// The tools and tool instances to remove from a toolset
            pub remove_toolset_tools: Vec<Uuid>,
            /// The new entry agent of a workflow
            pub workflow_entry_agent: Option<Uuid>,
            /// Clear a workflow's entry agent
            pub clear_workflow_entry_agent: Option<bool>,
            /// Whether new instances of a workflow are built automatically
            pub workflow_auto_instance: Option<bool>,
            /// The JSON encoded new run-wide limits of a workflow
            pub workflow_limits: Option<String>,
            /// The JSON encoded new default models of a workflow, replacing the whole list
            pub workflow_default_models: Option<String>,
            /// The JSON encoded new model auth levels of a workflow
            pub workflow_model_auth: Option<String>,
            /// The agents to add to a workflow
            pub add_workflow_agents: Vec<Uuid>,
            /// The agents to remove from a workflow
            pub remove_workflow_agents: Vec<Uuid>,
            /// The names of the metadata fields the update set, in the order they were sent
            pub sent: Vec<String>,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
// TODO schema example
pub struct Entity {
    /// The entity's unique ID
    pub id: Uuid,
    /// The name of this entity
    pub name: String,
    /// The kind of entity this is
    pub kind: EntityKinds,
    /// The metadata for our specific entity
    pub metadata: EntityMetadata,
    /// A description of this entity
    pub description: Option<String>,
    /// The user that submitted this entity
    pub submitter: String,
    /// The groups this entity was submitted too
    pub groups: Vec<String>,
    /// The tags for this entity
    pub tags: TagMap,
    /// This entities image
    pub image: Option<String>,
    /// The time this entity was created
    pub created: DateTime<Utc>,
}

impl Entity {
    /// Hash the identifying info for this entity
    #[must_use]
    pub fn hash_identifying(&self) -> u64 {
        // Create a hasher with a static seed
        let mut hasher = GxHasher::with_seed(1234);
        // hash any required identifying info thats not metadata
        hasher.write(self.name.as_bytes());
        self.kind.hash(&mut hasher);
        // hash our identifying metadata
        self.metadata.hash_identifying(&mut hasher);
        // get this entities identifying hash
        hasher.finish()
    }
}

impl TreeSupport for Entity {
    /// The data used to generate this types tree hash
    type HashType<'a> = &'a Uuid;

    /// Hash this child object
    fn tree_hash(&self) -> u64 {
        Self::tree_hash_direct(&self.id)
    }

    /// Hash this child object
    ///
    /// # Arguments
    ///
    /// * `input` - The data needed to generate this nodes tree hash
    /// * `hasher` - The hasher to write data to
    fn tree_hash_direct_with_hasher(input: Self::HashType<'_>, hasher: &mut GxHasher) {
        // hash this samples sha
        hasher.write_u128(input.as_u128());
    }

    /// Gather any initial nodes for a tree
    #[cfg(feature = "api")]
    #[tracing::instrument(name = "TreeSupport::<Entities>::gather_initial", skip_all, err(Debug))]
    async fn gather_initial(
        user: &User,
        query: &crate::models::TreeQuery,
        shared: &crate::utils::Shared,
    ) -> Result<Vec<super::TreeNode>, crate::utils::ApiError> {
        // get info on all of the initial entities
        let entities =
            crate::models::backends::db::entities::get_many(&query.groups, &query.entities, shared)
                .await?;
        // build a list of initial data
        let mut initial = Vec::with_capacity(query.entities.len());
        // step over the entities we retrieved and add them to our tree
        for mut entity in entities {
            // trees never return a secret or name a group this user can't see
            models::RedactSecrets::redact_secrets(&mut entity);
            entity.hide_snapshot_groups(user);
            // wrap this entity in a tree node
            let node_data = super::TreeNode::Entity(entity);
            // add this tree node to our initial list
            initial.push(node_data);
        }
        Ok(initial)
    }

    /// Gather any children for this child node
    #[cfg(feature = "api")]
    #[tracing::instrument(
        name = "TreeSupport::<Entities>::gather_children",
        skip_all,
        err(Debug)
    )]
    async fn gather_children(
        &self,
        user: &User,
        tree: &Tree,
        ring: &crate::models::backends::trees::TreeRing,
        shared: &crate::utils::Shared,
    ) -> Result<(), crate::utils::ApiError> {
        // get a cursor for this entities associations scoped to the requesting user
        if let Some(mut cursor) = self.list_associations(user, shared).await? {
            // get this entities tree hash
            let source_hash = self.tree_hash();
            // crawl through this entities associations and add them to the tree
            loop {
                // build a full list of all associations
                let mut associations = Vec::with_capacity(cursor.data.len());
                // track which associations have been newly added
                let mut new_set = HashSet::with_capacity(cursor.data.len());
                // preallocate a list of associations that are filtered to just new ones
                let mut new_list = Vec::with_capacity(cursor.data.len());
                // convert all of the listable associations to full associations
                let assoc_iter = cursor.data.drain(..).map(super::Association::try_from);
                // iterate over our associations and filter them
                for cast in assoc_iter {
                    // check if the cast for this association failed
                    let assoc = cast?;
                    // get the hash for this association
                    let tree_hash = assoc.tree_hash();
                    // check if our ring already contains this association
                    if !ring.contains(tree, tree_hash).await {
                        // only add associations that aren't already in our new set
                        if !new_set.contains(&tree_hash) {
                            // add this associations treee hash to our new set
                            new_set.insert(tree_hash);
                            // add this association to our new list
                            new_list.push(assoc.clone());
                        }
                    }
                    // add this association
                    associations.push(assoc);
                }
                // get this pages tree nodes in parallel
                let mut node_stream = stream::iter(new_list)
                    .map(|assoc| async move { assoc.get_tree_node(user, shared).await })
                    .buffer_unordered(10);
                // add our tree nodes as we get them
                while let Some(node_result) = node_stream.next().await {
                    // if we failed to get this node then raise an error
                    let node = node_result?;
                    // add this node
                    ring.add_node(node).await;
                }
                // get an entry to our parent nodes relationships
                let entry = ring
                    .relationships
                    .entry_async(source_hash)
                    .await
                    .or_default();
                // build the branches for these associations
                for association in associations {
                    // get the tree hash for what this association points too
                    let target_hash = association.tree_hash();
                    // get this associations direction
                    let direction = association.direction;
                    // build the relationship for this branch
                    let relationship = crate::models::TreeRelationships::Association(association);
                    // wrap our relationship in a branch
                    let branch = UnhashedTreeBranch::new(target_hash, relationship, direction);
                    // get the hash for this branch (not the tree hash the hash of the full object)
                    let full_hash = branch.full_hash();
                    // insert our relationship
                    entry.upsert_async(full_hash, branch).await;
                }
                // if our cursor is exhausted then stop crawling
                if cursor.exhausted() {
                    break;
                }
                // get the next page of data
                cursor.next(shared).await?;
            }
        }
        // entities only use associations for children
        Ok(())
    }

    /// Build an association target column for an object
    #[cfg(feature = "api")]
    fn build_association_target_column(&self) -> Option<super::AssociationTargetColumn> {
        // build a target for this entity
        let target = super::AssociationTargetColumn::Entity(self.id);
        Some(target)
    }
}

#[cfg(any(feature = "api", feature = "client"))]
impl KeySupport for Entity {
    // the entity's ID
    // doesn't work for uuid with Utoipa even with the uuid feature flag since this is a generic
    // https://github.com/juhaku/utoipa/issues/1346
    type Key = String;

    type ExtraKey = ();

    fn build_key(key: Self::Key, _extra: &Self::ExtraKey) -> String {
        key
    }

    fn key_url(key: &Self::Key, _extra: Option<&Self::ExtraKey>) -> String {
        key.clone()
    }
}

#[cfg(any(feature = "api", feature = "client"))]
impl TagSupport for Entity {
    /// Get the tag kind to write to the DB
    fn tag_kind() -> TagType {
        TagType::Entities
    }

    fn earliest(&self) -> HashMap<&String, DateTime<Utc>> {
        // instance a map for the earliest time each group has seen this entity
        let mut earliest = HashMap::with_capacity(self.groups.len());
        // for entities all groups wil always have the same timestamp for now
        for group in &self.groups {
            earliest.insert(group, self.created);
        }
        earliest
    }

    /// Add some tags to an entity
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is creating tags
    /// * `req` - The tag request to apply
    /// * `shared` - Shared Thorium objects
    #[tracing::instrument(
        name = "TagSupport<Entity>::tag",
        skip_all,
        fields(name = self.name, id = self.id.to_string()),
        err(Debug))
    ]
    #[cfg(feature = "api")]
    async fn tag(
        &self,
        user: &User,
        mut req: TagRequest<Self>,
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // make sure we have edit permissions in all groups and that
        // all groups allow for entities
        self.validate_check_allow_groups(
            user,
            &mut req.groups,
            Group::editable,
            "edit",
            Some(GroupAllowAction::Entities),
            shared,
        )
        .await?;
        // get the earliest for each group (just the time the entity was created)
        let earliest = self.earliest();
        let key = Self::build_key(self.id.to_string(), &());
        // keep the pairs a tool's toolsets may select and the checked groups they're added in
        // before the request is consumed
        let pairs = (self.kind == EntityKinds::Tool).then(|| {
            let pairs = req
                .tags
                .iter()
                .map(|(key, values)| (key.clone(), values.iter().cloned().collect()))
                .collect();
            (pairs, req.groups.clone())
        });
        // save the tags to scylla
        super::backends::db::tags::create(user, key, req, &earliest, shared).await?;
        // a tool's tag change can change which toolsets select it in the tagged groups
        if let Some((pairs, groups)) = pairs {
            super::backends::entities::tools::queue_tag_change(
                user, self.id, pairs, &groups, shared,
            )
            .await?;
        }
        Ok(())
    }

    /// Delete some tags from this entity
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is deleting tags
    /// * `req` - The tags to delete
    /// * `shared` - Shared Thorium objects
    #[tracing::instrument(
        name = "TagSupport<Entity>::delete_tags",
        skip_all,
        fields(name = self.name, id = self.id.to_string()),
        err(Debug))
    ]
    #[cfg(feature = "api")]
    async fn delete_tags(
        &self,
        user: &User,
        mut req: TagDeleteRequest<Self>,
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // make sure we have edit permissions in all groups;
        // no need to check for the group action as deleting
        // is always allowed
        self.validate_check_allow_groups(
            user,
            &mut req.groups,
            Group::editable,
            "edit",
            None,
            shared,
        )
        .await?;
        // build our key
        let key = Self::build_key(self.id.to_string(), &());
        // delete the requested tags if they exist
        super::backends::db::tags::delete(&key, &req, shared).await?;
        // a tool's tag change can change which toolsets select it in the checked groups, so the
        // pairs it carried in one of them are queued
        if self.kind == EntityKinds::Tool {
            let pairs = req
                .tags
                .iter()
                .map(|(key, values)| {
                    let removed = values
                        .iter()
                        .filter(|value| {
                            self.tags
                                .get(key)
                                .and_then(|carried| carried.get(*value))
                                .is_some_and(|groups| {
                                    req.groups.iter().any(|group| groups.contains(group))
                                })
                        })
                        .cloned()
                        .collect::<std::collections::BTreeSet<String>>();
                    (key.clone(), removed)
                })
                .filter(|(_, removed)| !removed.is_empty())
                .collect();
            super::backends::entities::tools::queue_tag_change(
                user,
                self.id,
                pairs,
                &req.groups,
                shared,
            )
            .await?;
        }
        Ok(())
    }

    /// Gets tags for a specific entity
    ///
    /// # Arguments
    ///
    /// * `groups` - The groups to restrict our returned tags to
    /// * `shared` - Shared Thorium objects
    #[tracing::instrument(
        name = "TagSupport<Entity>::get_tags",
        skip_all,
        fields(name = self.name, id = self.id.to_string()),
        err(Debug))
    ]
    #[cfg(feature = "api")]
    async fn get_tags(&mut self, groups: &[String], shared: &Shared) -> Result<(), ApiError> {
        // build our key
        let key = Self::build_key(self.id.to_string(), &());
        // get the requested tags
        super::backends::db::tags::get(TagType::Entities, groups, &key, &mut self.tags, shared)
            .await
    }
}

/// The specific kind an entity is, including any data unique to its kind
///
/// Archived backups store each variant of `EntityKinds`, which lists this enum's variants in the
/// same order, as its position in that list, so variants are append-only: never reorder, remove,
/// or insert one; a new variant goes at the end. The golden-bytes test of archived enums in
/// thoradm's offline suite fails if the order changes.
#[derive(Debug, Clone, Serialize, Deserialize, EnumDiscriminants)]
// generate type just containing the entity kind's name with no data
#[strum_discriminants(name(EntityKinds))]
#[strum_discriminants(derive(
    Default,
    Serialize,
    Deserialize,
    AsRefStr,
    EnumString,
    strum::EnumIter,
    strum::Display,
    Hash,
    Linearize,
    JsonSchema,
))]
#[cfg_attr(feature = "python", strum_discriminants(pyo3::pyclass(from_py_object)))]
#[cfg_attr(
    feature = "scylla-utils",
    strum_discriminants(derive(thorium_derive::ScyllaStoreJson))
)]
#[cfg_attr(
    feature = "rkyv-support",
    strum_discriminants(derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))
)]
#[cfg_attr(
    feature = "rkyv-support",
    strum_discriminants(archive_attr(derive(Debug, bytecheck::CheckBytes)))
)]
#[cfg_attr(feature = "api", strum_discriminants(derive(utoipa::ToSchema)))]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "scylla-utils", derive(thorium_derive::ScyllaStoreJson))]
#[allow(clippy::large_enum_variant)]
pub enum EntityMetadata {
    /// A device entity
    Device(DeviceEntity),
    /// A vendor entity
    Vendor(VendorEntity),
    /// A collection entity
    ///
    /// Collections are dynamic lists of items in Thorium (e.g. samples, repos, etc.)
    /// based on search parameters like tags
    Collection(CollectionEntity),
    /// A filesystem entity
    FileSystem(FileSystemEntity),
    /// A folder within a filesystem entity
    Folder(FileSystemFolderEntity),
    /// A Windows process tree entity
    WindowsProcessTree(WindowsProcessTreeEntity),
    /// A Windows process
    WindowsProcess(WindowsProcessEntity),
    /// A Network connection
    NetworkConnection(NetworkConnection),
    /// A sigma rule to apply to data
    SigmaRule(SigmaRule),
    /// A flag on some suspicious data
    Flag(Flag),
    /// An occurence or incident
    Incident(Incident),
    /// A compiled function
    CompiledFunction(CompiledFunction),
    /// A decompiled function
    DecompiledFunction(DecompiledFunction),
    /// A section within a PE/binary
    PeSection(PeSectionEntity),
    /// A library imported by a PE/binary and its functions
    PeImport(PeImportEntity),
    /// An entity that can't be described by any of the other variants
    ///
    /// Backups archive each kind by its position in this enum, so kinds keep their place and
    /// new kinds go at the end
    #[strum_discriminants(default)]
    Other,
    /// An editable tool whose versions are tool instances
    Tool(ToolEntity),
    /// A single immutable version of a tool
    ToolInstance(ToolInstanceEntity),
    /// A configured AI agent
    Agent(AgentEntity),
    /// A named group of agentic tools that agents attach as one unit
    ToolSet(ToolSetEntity),
    /// A constant snapshot of a toolset's resolved members
    ToolSetInstance(toolsets::ToolSetInstanceEntity),
    /// The editable root that wires agents into a workflow
    WorkflowCatalog(WorkflowCatalogEntity),
    /// A constant, fully resolved snapshot of a workflow catalog's graph
    WorkflowInstance(WorkflowInstanceEntity),
    /// A connection to a model provider and the models it serves
    ModelEndpoint(ModelEndpointEntity),
    /// A named way to call one model on one endpoint
    ModelProfile(ModelProfileEntity),
    /// A credential model endpoints authenticate with
    ModelCredential(ModelCredentialEntity),
}

impl EntityMetadata {
    /// Hash the identifying info for this entity
    ///
    /// # Arguments
    ///
    /// * `hasher` - The hasher to use to hash any identifying data
    pub fn hash_identifying(&self, hasher: &mut GxHasher) {
        // get this entities identifying info
        let identifying = IdentifyingEntityInfo::from(self);
        // hash our identifying info
        identifying.hash(hasher);
    }
}

/// The specific kind an entity is, including any data unique to its kind
///
/// The derived serde code is generated as inherent functions and wrapped by the trait impls
/// below, so requests that name a process tree without data still read.
#[derive(Debug, Clone, Serialize, Deserialize, Hash)]
#[serde(remote = "Self")]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum EntityMetadataRequest {
    /// A device entity
    Device(DeviceEntityRequest),
    /// A vendor entity
    Vendor(VendorEntityRequest),
    /// A request to create a collection entity
    Collection(CollectionEntityRequest),
    /// A filesystem entity
    FileSystem(FileSystemEntity),
    /// A filesystem folder entity
    Folder(FileSystemFolderEntity),
    /// A windows process tree and the tool instances that dumped it
    WindowsProcessTree(WindowsProcessTreeEntity),
    /// A windows process
    WindowsProcess(WindowsProcessEntity),
    /// A network connection
    NetworkConnection(NetworkConnection),
    /// A sigma rule to apply to data
    SigmaRule(SigmaRule),
    /// A flag on some suspicious data
    Flag(Flag),
    /// An occurence or incident
    Incident(IncidentRequest),
    /// A compiled function
    CompiledFunction(CompiledFunction),
    /// A decompild function
    DecompiledFunction(DecompiledFunction),
    /// A section within a PE/binary
    PeSection(PeSectionEntity),
    /// A library imported by a PE/binary and its functions
    PeImport(PeImportEntity),
    /// A tool and its first tool instance
    Tool(ToolEntityRequest),
    /// A tool instance published directly for an existing tool
    ToolInstance(ToolInstanceEntityRequest),
    /// A configured AI agent and its graph edges
    Agent(AgentEntityRequest),
    /// A toolset and its `Includes` edges
    ToolSet(ToolSetEntityRequest),
    /// A submitted toolset instance
    ToolSetInstance(ToolSetInstanceEntityRequest),
    /// A workflow catalog and its `Orchestrates` edges
    WorkflowCatalog(WorkflowCatalogEntityRequest),
    /// A submitted workflow instance
    WorkflowInstance(WorkflowInstanceEntityRequest),
    /// A connection to a model provider and the models it serves
    ModelEndpoint(ModelEndpointEntityRequest),
    /// A named way to call one model on one endpoint
    ModelProfile(ModelProfileEntityRequest),
    /// A credential model endpoints authenticate with
    ModelCredential(ModelCredentialEntityRequest),
    /// An entity that can't be described by any of the other variants
    Other,
}

impl serde::Serialize for EntityMetadataRequest {
    /// Serialize this request in its derived shape
    ///
    /// # Arguments
    ///
    /// * `serializer` - The serializer to write to
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // write the request with the derived serializer
        EntityMetadataRequest::serialize(self, serializer)
    }
}

impl<'de> serde::Deserialize<'de> for EntityMetadataRequest {
    /// Deserialize a request, reading a bare process tree name as a process tree with no tools
    ///
    /// # Arguments
    ///
    /// * `deserializer` - The deserializer to read from
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // read either a bare variant name or a variant with data
        deserializer.deserialize_any(EntityMetadataRequestVisitor)
    }
}

/// Reads entity metadata requests, accepting the bare process tree name tools and stored
/// results wrote before process trees listed tool instances
struct EntityMetadataRequestVisitor;

impl<'de> serde::de::Visitor<'de> for EntityMetadataRequestVisitor {
    type Value = EntityMetadataRequest;

    /// Describe what this visitor reads
    ///
    /// # Arguments
    ///
    /// * `formatter` - The formatter to write the description to
    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("an entity metadata request")
    }

    /// Read a bare variant name
    ///
    /// # Arguments
    ///
    /// * `name` - The variant name
    fn visit_str<E: serde::de::Error>(self, name: &str) -> Result<Self::Value, E> {
        // a bare process tree name is a process tree with no tools
        if name == "WindowsProcessTree" {
            return Ok(EntityMetadataRequest::WindowsProcessTree(
                WindowsProcessTreeEntity::default(),
            ));
        }
        // read any other name as the derived unit variant it names
        EntityMetadataRequest::deserialize(serde::de::value::StrDeserializer::<E>::new(name))
    }

    /// Read a variant with data
    ///
    /// # Arguments
    ///
    /// * `map` - The map holding the variant name and its data
    fn visit_map<A: serde::de::MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
        // read the variant with the derived deserializer
        EntityMetadataRequest::deserialize(serde::de::value::MapAccessDeserializer::new(map))
    }
}

impl EntityMetadataRequest {
    /// Add this entity metadata to a form
    #[cfg(feature = "client")]
    pub fn add_to_form(
        self,
        form: reqwest::multipart::Form,
    ) -> Result<reqwest::multipart::Form, crate::Error> {
        // add our metadata
        match self {
            EntityMetadataRequest::Device(device) => device.add_to_form(form),
            EntityMetadataRequest::Vendor(vendor) => vendor.add_to_form(form),
            EntityMetadataRequest::Collection(collection) => collection.add_to_form(form),
            EntityMetadataRequest::FileSystem(fs) => fs.add_to_form(form),
            EntityMetadataRequest::Folder(folder) => folder.add_to_form(form),
            EntityMetadataRequest::WindowsProcessTree(tree) => tree.add_to_form(form),
            EntityMetadataRequest::WindowsProcess(process) => process.add_to_form(form),
            EntityMetadataRequest::NetworkConnection(conn) => conn.add_to_form(form),
            EntityMetadataRequest::SigmaRule(rule) => rule.add_to_form(form),
            EntityMetadataRequest::Flag(flag) => flag.add_to_form(form),
            EntityMetadataRequest::Incident(incident) => incident.add_to_form(form),
            EntityMetadataRequest::CompiledFunction(func) => func.add_to_form(form),
            EntityMetadataRequest::DecompiledFunction(decomp) => decomp.add_to_form(form),
            EntityMetadataRequest::PeSection(section) => section.add_to_form(form),
            EntityMetadataRequest::PeImport(import) => import.add_to_form(form),
            EntityMetadataRequest::Tool(tool) => tool.add_to_form(form),
            EntityMetadataRequest::ToolInstance(instance) => instance.add_to_form(form),
            EntityMetadataRequest::Agent(agent) => agent.add_to_form(form),
            EntityMetadataRequest::ToolSet(tool_set) => Ok(tool_set.add_to_form(form)),
            EntityMetadataRequest::ToolSetInstance(instance) => Ok(instance.add_to_form(form)),
            EntityMetadataRequest::WorkflowCatalog(workflow) => workflow.add_to_form(form),
            EntityMetadataRequest::WorkflowInstance(instance) => Ok(instance.add_to_form(form)),
            EntityMetadataRequest::ModelEndpoint(endpoint) => endpoint.add_to_form(form),
            EntityMetadataRequest::ModelProfile(profile) => profile.add_to_form(form),
            EntityMetadataRequest::ModelCredential(credential) => credential.add_to_form(form),
            // just set our kind to other
            EntityMetadataRequest::Other => Ok(form.text("kind", EntityKinds::Other.as_str())),
        }
    }

    /// Get our entity kind
    #[must_use]
    pub fn kind(&self) -> EntityKinds {
        match self {
            EntityMetadataRequest::Device(_) => EntityKinds::Device,
            EntityMetadataRequest::Vendor(_) => EntityKinds::Vendor,
            EntityMetadataRequest::Collection(_) => EntityKinds::Collection,
            EntityMetadataRequest::FileSystem(_) => EntityKinds::FileSystem,
            EntityMetadataRequest::Folder(_) => EntityKinds::Folder,
            EntityMetadataRequest::WindowsProcessTree(_) => EntityKinds::WindowsProcessTree,
            EntityMetadataRequest::WindowsProcess(_) => EntityKinds::WindowsProcess,
            EntityMetadataRequest::NetworkConnection(_) => EntityKinds::NetworkConnection,
            EntityMetadataRequest::SigmaRule(_) => EntityKinds::SigmaRule,
            EntityMetadataRequest::Flag(_) => EntityKinds::Flag,
            EntityMetadataRequest::Incident(_) => EntityKinds::Incident,
            EntityMetadataRequest::CompiledFunction(_) => EntityKinds::CompiledFunction,
            EntityMetadataRequest::DecompiledFunction(_) => EntityKinds::DecompiledFunction,
            EntityMetadataRequest::PeSection(_) => EntityKinds::PeSection,
            EntityMetadataRequest::PeImport(_) => EntityKinds::PeImport,
            EntityMetadataRequest::Tool(_) => EntityKinds::Tool,
            EntityMetadataRequest::ToolInstance(_) => EntityKinds::ToolInstance,
            EntityMetadataRequest::Agent(_) => EntityKinds::Agent,
            EntityMetadataRequest::ToolSet(_) => EntityKinds::ToolSet,
            EntityMetadataRequest::ToolSetInstance(_) => EntityKinds::ToolSetInstance,
            EntityMetadataRequest::WorkflowCatalog(_) => EntityKinds::WorkflowCatalog,
            EntityMetadataRequest::WorkflowInstance(_) => EntityKinds::WorkflowInstance,
            EntityMetadataRequest::ModelEndpoint(_) => EntityKinds::ModelEndpoint,
            EntityMetadataRequest::ModelProfile(_) => EntityKinds::ModelProfile,
            EntityMetadataRequest::ModelCredential(_) => EntityKinds::ModelCredential,
            EntityMetadataRequest::Other => EntityKinds::Other,
        }
    }

    /// Deserialize a list of metadata requests from disk
    ///
    /// # Arguments
    ///
    /// * `path` - The path to read
    /// * `kind` - The kind of metadata request to read
    #[cfg(feature = "client")]
    pub async fn load_all<P: AsRef<std::path::Path>>(path: P) -> Result<Vec<Self>, crate::Error> {
        // read our target file from disk
        let data = tokio::fs::read(path.as_ref()).await?;
        // try to parse this to a list of entities
        let parsed = serde_json::from_slice(&data)?;
        Ok(parsed)
    }

    /// Convert this request into something that can be scanned by a sigma rule
    ///
    /// Will return None if an entity is not scannable by sigma rules.
    #[cfg(feature = "client")]
    pub fn to_sigma_scannable(&self) -> Result<Option<String>, crate::Error> {
        // unwrap the outer enum so we don't needlessly nest things
        match self {
            Self::Device(device) => Ok(Some(serde_json::to_string(device)?)),
            Self::Vendor(vendor) => Ok(Some(serde_json::to_string(vendor)?)),
            Self::Collection(collection) => Ok(Some(serde_json::to_string(collection)?)),
            Self::FileSystem(filesystem) => Ok(Some(serde_json::to_string(filesystem)?)),
            Self::Folder(folder) => Ok(Some(serde_json::to_string(folder)?)),
            Self::WindowsProcess(proc) => Ok(Some(serde_json::to_string(proc)?)),
            Self::NetworkConnection(conn) => Ok(Some(serde_json::to_string(conn)?)),
            Self::Flag(flag) => Ok(Some(serde_json::to_string(flag)?)),
            Self::Incident(incident) => Ok(Some(serde_json::to_string(incident)?)),
            Self::CompiledFunction(func) => Ok(Some(serde_json::to_string(func)?)),
            Self::DecompiledFunction(decomp) => Ok(Some(serde_json::to_string(decomp)?)),
            // PE sections/imports, tools, and model configuration are storage/display only and not
            // scanned with sigma rules
            Self::WindowsProcessTree(_)
            | Self::PeSection(_)
            | Self::PeImport(_)
            | Self::SigmaRule(_)
            | Self::Tool(_)
            | Self::ToolInstance(_)
            | Self::Agent(_)
            | Self::ToolSet(_)
            | Self::ToolSetInstance(_)
            | Self::WorkflowCatalog(_)
            | Self::WorkflowInstance(_)
            | Self::ModelEndpoint(_)
            | Self::ModelProfile(_)
            | Self::ModelCredential(_)
            | Self::Other => Ok(None),
        }
    }

    /// Hash the identifying info for this entity
    ///
    /// The identifying info is a subset entity data and metadata that combined
    /// allows users to effectively dedupe entities. This is not all info as that
    /// would mean that an entity with the same info (but different entity ids) in
    ///  group A would be considered different then an entity in group B. This is
    /// primarily used to prevent endless recreating the same entities when rescanning
    /// entities with rules. This is not the same as the tree hash.
    ///
    /// # Arguments
    ///
    /// * `hasher` - The hasher to use to hash any identifying data
    pub fn hash_identifying(&self, hasher: &mut GxHasher) {
        // get this entities identifying info
        let identifying = IdentifyingEntityInfo::from(self);
        // hash our identifying info
        identifying.hash(hasher);
    }
}

/// The different info that identifies approximately identical entities
///
/// This is intended to be used to prevent duplicate entities from being created when
/// rerunning tools.
/// The specific kind an entity is, including any data unique to its kind
#[derive(Debug, Clone, Hash)]
pub enum IdentifyingEntityInfo<'a> {
    /// A collection entity
    ///
    /// Collections are dynamic lists of items in Thorium (e.g. samples, repos, etc.)
    /// based on search parameters like tags
    Collection { kind: CollectionKind },
    /// A filesystem entity
    FileSystem { sha256: &'a String },
    /// A folder within a filesystem entity
    Folder { all_sha256: &'a String },
    /// A Windows process
    WindowsProcess { pid: u64 },
    /// A Network connection
    NetworkConnection {
        source: &'a IpAddr,
        destination: &'a IpAddr,
        destination_port: u16,
    },
    /// A sigma rule to apply to data
    SigmaRule { rule: &'a String },
    /// A flag on some suspicious data
    Flag {
        suspicion: i64,
        confidence: Confidence,
        reasoning: &'a String,
    },
    /// A compiled function
    CompiledFunction {
        address: u64,
        disassembly: &'a Vec<CompiledInstruction>,
    },
    /// A decompiled function
    DecompiledFunction { address: u64, content: &'a String },
    /// A tool instance, which is identified by its tool and the content hash of its spec
    ToolInstance {
        /// The tool this is an instance of
        tool: &'a Uuid,
        /// The content hash of this instance's spec
        spec_hash: &'a String,
    },
    /// An entity that cannot be accurately/usefully identified to prevent duplicates
    Unidentifiable,
}

impl<'a> From<&'a EntityMetadata> for IdentifyingEntityInfo<'a> {
    fn from(meta: &'a EntityMetadata) -> Self {
        match meta {
            EntityMetadata::Collection(collection) => Self::Collection {
                kind: collection.collection_kind,
            },
            EntityMetadata::FileSystem(fs) => Self::FileSystem { sha256: &fs.sha256 },
            EntityMetadata::Folder(folder) => Self::Folder {
                all_sha256: &folder.all_sha256,
            },
            EntityMetadata::WindowsProcess(proc) => Self::WindowsProcess { pid: proc.pid },
            EntityMetadata::NetworkConnection(conn) => Self::NetworkConnection {
                source: &conn.source,
                destination: &conn.destination,
                destination_port: conn.destination_port,
            },
            EntityMetadata::SigmaRule(rule) => Self::SigmaRule { rule: &rule.rule },
            EntityMetadata::Flag(flag) => Self::Flag {
                suspicion: flag.suspicion,
                confidence: flag.confidence,
                reasoning: &flag.reasoning,
            },
            EntityMetadata::CompiledFunction(func) => Self::CompiledFunction {
                address: func.address,
                disassembly: &func.disassembly,
            },
            EntityMetadata::DecompiledFunction(decomp) => Self::DecompiledFunction {
                address: decomp.address,
                content: &decomp.content,
            },
            EntityMetadata::ToolInstance(instance) => Self::ToolInstance {
                tool: &instance.tool,
                spec_hash: &instance.spec_hash,
            },
            // These entities have no useful identifying info
            EntityMetadata::Device(_)
            | EntityMetadata::Vendor(_)
            | EntityMetadata::WindowsProcessTree(_)
            | EntityMetadata::Incident(_)
            | EntityMetadata::PeSection(_)
            | EntityMetadata::PeImport(_)
            | EntityMetadata::Tool(_)
            | EntityMetadata::Agent(_)
            | EntityMetadata::ToolSet(_)
            | EntityMetadata::ToolSetInstance(_)
            | EntityMetadata::WorkflowCatalog(_)
            | EntityMetadata::WorkflowInstance(_)
            | EntityMetadata::ModelEndpoint(_)
            | EntityMetadata::ModelProfile(_)
            | EntityMetadata::ModelCredential(_)
            | EntityMetadata::Other => Self::Unidentifiable,
        }
    }
}

impl<'a> From<&'a EntityMetadataRequest> for IdentifyingEntityInfo<'a> {
    fn from(meta: &'a EntityMetadataRequest) -> Self {
        match meta {
            EntityMetadataRequest::Collection(collection) => Self::Collection {
                kind: collection.collection_kind,
            },
            EntityMetadataRequest::FileSystem(fs) => Self::FileSystem { sha256: &fs.sha256 },
            EntityMetadataRequest::Folder(folder) => Self::Folder {
                all_sha256: &folder.all_sha256,
            },
            EntityMetadataRequest::WindowsProcess(proc) => Self::WindowsProcess { pid: proc.pid },
            EntityMetadataRequest::NetworkConnection(conn) => Self::NetworkConnection {
                source: &conn.source,
                destination: &conn.destination,
                destination_port: conn.destination_port,
            },
            EntityMetadataRequest::SigmaRule(rule) => Self::SigmaRule { rule: &rule.rule },
            EntityMetadataRequest::Flag(flag) => Self::Flag {
                suspicion: flag.suspicion,
                confidence: flag.confidence,
                reasoning: &flag.reasoning,
            },
            EntityMetadataRequest::CompiledFunction(func) => Self::CompiledFunction {
                address: func.address,
                disassembly: &func.disassembly,
            },
            EntityMetadataRequest::DecompiledFunction(decomp) => Self::DecompiledFunction {
                address: decomp.address,
                content: &decomp.content,
            },
            // These entities have no useful identifying info; a tool instance request has no
            // spec hash until the API computes it
            EntityMetadataRequest::Device(_)
            | EntityMetadataRequest::Vendor(_)
            | EntityMetadataRequest::WindowsProcessTree(_)
            | EntityMetadataRequest::Incident(_)
            | EntityMetadataRequest::PeSection(_)
            | EntityMetadataRequest::PeImport(_)
            | EntityMetadataRequest::Tool(_)
            | EntityMetadataRequest::ToolInstance(_)
            | EntityMetadataRequest::Agent(_)
            | EntityMetadataRequest::ToolSet(_)
            | EntityMetadataRequest::ToolSetInstance(_)
            | EntityMetadataRequest::WorkflowCatalog(_)
            | EntityMetadataRequest::WorkflowInstance(_)
            | EntityMetadataRequest::ModelEndpoint(_)
            | EntityMetadataRequest::ModelProfile(_)
            | EntityMetadataRequest::ModelCredential(_)
            | EntityMetadataRequest::Other => Self::Unidentifiable,
        }
    }
}

impl EntityKinds {
    /// Gets a str representation of the entity kind name
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.as_ref()
    }

    /// Convert this entity kind to a [`SigmaRuleAppliesTo`] if possible
    ///
    /// Not all entity kinds can be converted and any that can't will return [`None`]
    #[must_use]
    pub fn to_sigma_applies_to(&self) -> Option<SigmaRuleAppliesTo> {
        match self {
            Self::WindowsProcess => Some(SigmaRuleAppliesTo::WindowsProcesses),
            Self::NetworkConnection => Some(SigmaRuleAppliesTo::NetworkConnections),
            Self::CompiledFunction => Some(SigmaRuleAppliesTo::CompiledFunctions),
            Self::DecompiledFunction => Some(SigmaRuleAppliesTo::DecompiledFunctions),
            // all other entity kinds cannot be scanned with sigma rules
            Self::Device
            | Self::Vendor
            | Self::Collection
            | Self::FileSystem
            | Self::Folder
            | Self::WindowsProcessTree
            | Self::PeSection
            | Self::PeImport
            | Self::SigmaRule
            | Self::Flag
            | Self::Incident
            | Self::Tool
            | Self::ToolInstance
            | Self::Agent
            | Self::ToolSet
            | Self::ToolSetInstance
            | Self::WorkflowCatalog
            | Self::WorkflowInstance
            | Self::ModelEndpoint
            | Self::ModelProfile
            | Self::ModelCredential
            | Self::Other => None,
        }
    }

    /// Get how this kind's copies in several groups are kept and checked
    ///
    /// A kind that keeps one shared definition across its groups is `Contained`; every other kind
    /// keeps independent copies and is `Overlap`.
    #[must_use]
    pub fn group_policy(&self) -> GroupPolicy {
        match self {
            // tools, agents, toolsets, workflows, and model configuration keep one shared
            // definition across their groups
            Self::Tool
            | Self::ToolInstance
            | Self::Agent
            | Self::ToolSet
            | Self::ToolSetInstance
            | Self::WorkflowCatalog
            | Self::WorkflowInstance
            | Self::ModelEndpoint
            | Self::ModelProfile
            | Self::ModelCredential => GroupPolicy::Contained,
            // these kinds keep independent copies per group
            Self::Device
            | Self::Vendor
            | Self::Collection
            | Self::FileSystem
            | Self::Folder
            | Self::WindowsProcessTree
            | Self::WindowsProcess
            | Self::NetworkConnection
            | Self::SigmaRule
            | Self::Flag
            | Self::Incident
            | Self::CompiledFunction
            | Self::DecompiledFunction
            | Self::PeSection
            | Self::PeImport
            | Self::Other => GroupPolicy::Overlap,
        }
    }

    /// Get any root entity kinds if any exist
    ///
    /// Root kinds or root nodes are for entities with a tree like structure that require
    /// a root. For instance a filesystem starts from a root `FileSystem` entity.
    #[must_use]
    pub fn root_kinds(&self) -> &[EntityKinds] {
        match self {
            Self::WindowsProcess => &[Self::WindowsProcessTree],
            Self::Folder => &[Self::FileSystem],
            // all other entity kinds do not have required root kinds
            Self::Device
            | Self::Vendor
            | Self::Collection
            | Self::FileSystem
            | Self::WindowsProcessTree
            | Self::NetworkConnection
            | Self::PeSection
            | Self::PeImport
            | Self::SigmaRule
            | Self::Flag
            | Self::Incident
            | Self::CompiledFunction
            | Self::DecompiledFunction
            | Self::Tool
            | Self::ToolInstance
            | Self::Agent
            | Self::ToolSet
            | Self::ToolSetInstance
            | Self::WorkflowCatalog
            | Self::WorkflowInstance
            | Self::ModelEndpoint
            | Self::ModelProfile
            | Self::ModelCredential
            | Self::Other => &[],
        }
    }
}

/// How an entity kind's copies in several groups are kept and checked
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupPolicy {
    /// One definition shared by every group it's in: edits and deletes need access to all of its
    /// groups and rewrite every copy, graph edges need the target to cover the source's groups,
    /// and instances carry their root's groups
    Contained,
    /// Independent copies per group: reads, writes, and deletes touch only the caller's visible
    /// groups, and copies may diverge
    Overlap,
}

impl From<SigmaRuleAppliesTo> for EntityKinds {
    fn from(applies_to: SigmaRuleAppliesTo) -> Self {
        // map what a sigma rule applies to to an entity kind
        match applies_to {
            SigmaRuleAppliesTo::WindowsProcesses => EntityKinds::WindowsProcess,
            SigmaRuleAppliesTo::NetworkConnections => EntityKinds::NetworkConnection,
            SigmaRuleAppliesTo::CompiledFunctions => EntityKinds::CompiledFunction,
            SigmaRuleAppliesTo::DecompiledFunctions => EntityKinds::DecompiledFunction,
        }
    }
}

impl From<&SigmaRuleAppliesTo> for EntityKinds {
    fn from(applies_to: &SigmaRuleAppliesTo) -> Self {
        EntityKinds::from(*applies_to)
    }
}

/// A request to create an entity
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct EntityRequest {
    /// The entity's name
    pub name: String,
    /// The metadata for a specific kind of entity
    pub metadata: EntityMetadataRequest,
    /// The groups this entity should be in
    pub groups: Vec<String>,
    /// The tags for this entity
    pub tags: HashMap<String, HashSet<String>>,
    /// A description of this entity
    pub description: Option<String>,
}

impl EntityRequest {
    /// Create a new entity request
    pub fn new<I>(name: impl Into<String>, metadata: EntityMetadataRequest, groups: I) -> Self
    where
        I: IntoIterator,
        I::Item: Into<String>,
    {
        // convert our groups to a list of strings
        let groups = groups.into_iter().map(Into::into).collect();
        EntityRequest {
            name: name.into(),
            metadata,
            groups,
            tags: HashMap::default(),
            description: None,
        }
    }

    /// Add a tag to this entity request
    ///
    /// # Arguments
    ///
    /// * `key` - The key of the tag to add
    /// * `value` - The value for the tag to add
    #[must_use]
    pub fn tag(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        // get an entry into this tags key list
        let entry = self.tags.entry(key.into()).or_default();
        // add our value
        entry.insert(value.into());
        self
    }

    /// Cast this entity request into a form
    #[cfg(feature = "client")]
    pub fn to_form(mut self) -> Result<reqwest::multipart::Form, crate::Error> {
        // build the form we are going to send
        // disable percent encoding, as the API natively supports UTF-8
        let form = reqwest::multipart::Form::new().percent_encode_noop();
        // add the name of this entity
        let form = form.text("name", self.name);
        // add our entity metadata
        let form = self.metadata.add_to_form(form)?;
        // add our groups
        let mut form = multipart_list!(form, "groups[]", self.groups);
        // add any tags to this form
        for (key, mut values) in self.tags {
            // build the tag key to for this tag
            let tag_key = format!("tags[{key}][]");
            // add this tags list of values to our form
            form = multipart_set!(form, &tag_key, values);
        }
        // add our description to this requet
        let form = multipart_text!(form, "description", self.description);
        Ok(form)
    }

    /// Get our entity kind
    #[must_use]
    pub fn kind(&self) -> EntityKinds {
        // get our entity kind based on our metadata
        self.metadata.kind()
    }

    /// Get the parent info for this entity request if it has a parent
    pub fn parent_info(&self) -> Option<EntityParentInfo> {
        // get the identifying info for any parent processes if it exists
        match &self.metadata {
            EntityMetadataRequest::WindowsProcess(proc) => {
                // if we have a parent pid set then use that
                proc.parent_pid.map(EntityParentInfo::WindowsParentProcess)
            }
            _ => None,
        }
    }

    /// Get this entities root node kind
    #[must_use]
    pub fn root_kind(&self) -> Option<EntityKinds> {
        // only certain kinds of entities use a root node
        match &self.metadata {
            EntityMetadataRequest::WindowsProcess(_) => Some(EntityKinds::WindowsProcessTree),
            EntityMetadataRequest::Folder(_) => Some(EntityKinds::FileSystem),
            // all other entity types do not have a root node
            EntityMetadataRequest::Device(_)
            | EntityMetadataRequest::Vendor(_)
            | EntityMetadataRequest::Collection(_)
            | EntityMetadataRequest::FileSystem(_)
            | EntityMetadataRequest::WindowsProcessTree(_)
            | EntityMetadataRequest::NetworkConnection(_)
            | EntityMetadataRequest::PeSection(_)
            | EntityMetadataRequest::PeImport(_)
            | EntityMetadataRequest::SigmaRule(_)
            | EntityMetadataRequest::Flag(_)
            | EntityMetadataRequest::Incident(_)
            | EntityMetadataRequest::CompiledFunction(_)
            | EntityMetadataRequest::DecompiledFunction(_)
            | EntityMetadataRequest::Tool(_)
            | EntityMetadataRequest::ToolInstance(_)
            | EntityMetadataRequest::Agent(_)
            | EntityMetadataRequest::ToolSet(_)
            | EntityMetadataRequest::ToolSetInstance(_)
            | EntityMetadataRequest::WorkflowCatalog(_)
            | EntityMetadataRequest::WorkflowInstance(_)
            | EntityMetadataRequest::ModelEndpoint(_)
            | EntityMetadataRequest::ModelProfile(_)
            | EntityMetadataRequest::ModelCredential(_)
            | EntityMetadataRequest::Other => None,
        }
    }

    /// Hash the identifying info for this entity
    ///
    /// The identifying info is a subset entity data and metadata that combined
    /// allows users to effectively dedupe entities. This is not all info as that
    /// would mean that an entity with the same info (but different entity ids) in
    ///  group A would be considered different then an entity in group B. This is
    /// primarily used to prevent endless recreating the same entities when rescanning
    /// entities with rules.
    #[must_use]
    pub fn hash_identifying(&self) -> u64 {
        // Create a hasher with a static seed
        let mut hasher = GxHasher::with_seed(1234);
        // hash any required identifying info thats not metadata
        hasher.write(self.name.as_bytes());
        self.metadata.kind().hash(&mut hasher);
        // hash our identifying metadata
        self.metadata.hash_identifying(&mut hasher);
        // get this entities identifying hash
        hasher.finish()
    }
}

/// The response from an entity creation
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct EntityResponse {
    /// The ID of the created entity
    pub id: Uuid,
    /// The name of this entity
    pub name: String,
    /// The ID of the first tool instance created with a tool
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_instance: Option<Uuid>,
    /// How portable a created agentic tool instance's spec is across model APIs
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub portability: Option<tools::ToolPortabilityReport>,
}

impl EntityResponse {
    /// Create a new entity response
    ///
    /// # Arguments
    ///
    /// * `id` - The ID of the created entity
    #[must_use]
    pub fn new(id: Uuid, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            tool_instance: None,
            portability: None,
        }
    }
}

/// Set default for the entity list limit
fn default_list_limit() -> usize {
    50
}

/// Set the default for listing entities by kind
fn default_entity_kinds() -> Vec<EntityKinds> {
    // list all entities by default
    EntityKinds::iter().collect()
}

/// The options that you can set when listing entities in Thorium
#[derive(Debug, Clone)]
pub struct EntityListOpts {
    /// The cursor to use to continue this search
    pub cursor: Option<Uuid>,
    /// The latest date to start listing entities from
    pub start: Option<DateTime<Utc>>,
    /// The oldest date to stop listing entities from
    pub end: Option<DateTime<Utc>>,
    /// The max number of objects to retrieve on a single page
    pub page_size: usize,
    /// The total number of objects to return with this cursor
    pub limit: Option<usize>,
    /// The groups limit our search to
    pub groups: Vec<String>,
    /// The tags to filter on
    pub tags: HashMap<String, Vec<String>>,
    /// Whether matching on tags should be case-insensitive
    pub tags_case_insensitive: bool,
    /// The different kinds of entities to list
    pub kinds: Vec<EntityKinds>,
}

impl Default for EntityListOpts {
    /// Build a default search
    fn default() -> Self {
        EntityListOpts {
            start: None,
            cursor: None,
            end: None,
            page_size: 50,
            limit: None,
            groups: Vec::default(),
            tags: HashMap::default(),
            tags_case_insensitive: false,
            kinds: Vec::default(),
        }
    }
}

impl EntityListOpts {
    /// Restrict the entity search to start at a specific date
    ///
    /// # Arguments
    ///
    /// * `start` - The date to start listing entities from
    #[must_use]
    pub fn start(mut self, start: DateTime<Utc>) -> Self {
        // set the date to start listing entities at
        self.start = Some(start);
        self
    }

    /// Set the cursor to use when continuing this search
    ///
    /// # Arguments
    ///
    /// * `cursor` - The cursor id to use for this search
    #[must_use]
    pub fn cursor(mut self, cursor: Uuid) -> Self {
        // set cursor for this search
        self.cursor = Some(cursor);
        self
    }

    /// Restrict the entity search to stop at a specific date
    ///
    /// # Arguments
    ///
    /// * `end` - The date to stop listing entites at
    #[must_use]
    pub fn end(mut self, end: DateTime<Utc>) -> Self {
        // set the date to end listing entities at
        self.end = Some(end);
        self
    }

    /// The max number of entities to retrieve in a single page
    ///
    /// # Arguments
    ///
    /// * `page_size` - The max number of documents to return in a single request
    #[must_use]
    pub fn page_size(mut self, page_size: usize) -> Self {
        // set the date to end listing entities at
        self.page_size = page_size;
        self
    }

    /// Limit how many entities this search can return at once
    ///
    /// # Arguments
    ///
    /// * `limit` - The max number of objects to return over the lifetime of this cursor
    #[must_use]
    pub fn limit(mut self, limit: usize) -> Self {
        // set the date to end listing entities at
        self.limit = Some(limit);
        self
    }

    /// Limit what groups we search in
    ///
    /// # Arguments
    ///
    /// * `groups` - The groups to restrict our search to
    #[must_use]
    pub fn groups<T: Into<String>>(mut self, groups: Vec<T>) -> Self {
        // add the groups to restrict returned data too
        self.groups
            .extend(groups.into_iter().map(std::convert::Into::into));
        self
    }

    /// List entities that match a specific tag
    ///
    /// # Arguments
    ///
    /// * `key` - The tag key to match against
    /// * `value` - The tag value to match against
    #[must_use]
    pub fn tag<K: Into<String>, V: Into<String>>(mut self, key: K, value: V) -> Self {
        // get an entry into this tags value list
        let entry = self.tags.entry(key.into()).or_default();
        // add this tags value
        entry.push(value.into());
        self
    }

    /// List entities that match a specific tag by ref
    ///
    /// # Arguments
    ///
    /// * `key` - The tag key to match against
    /// * `value` - The tag value to match against
    pub fn tag_ref<K: Into<String>, V: Into<String>>(&mut self, key: K, value: V) {
        // get an entry into this tags value list
        let entry = self.tags.entry(key.into()).or_default();
        // add this tags value
        entry.push(value.into());
    }

    /// Set for matching on tags to be case-insensitive
    #[must_use]
    pub fn tags_case_insensitive(mut self) -> Self {
        self.tags_case_insensitive = true;
        self
    }

    /// Limit what kinds of entities are returned
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of entities to restrict our list to
    #[must_use]
    pub fn kind(mut self, kind: EntityKinds) -> Self {
        // add this kind to our option struct
        self.kinds.push(kind);
        self
    }

    /// Limit what kinds of entities are returned
    ///
    /// # Arguments
    ///
    /// * `kinds` - The kinds of entities to restrict our list to
    #[must_use]
    pub fn kinds(mut self, kinds: impl IntoIterator<Item = EntityKinds>) -> Self {
        // add these entity kinds to our options
        self.kinds.extend(kinds);
        self
    }
}

/// The params for listing entities
#[derive(Deserialize, Debug)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "api", derive(utoipa::IntoParams))]
#[cfg_attr(feature = "api", into_params(parameter_in = Query))]
pub struct EntityListParams {
    /// The groups to list data from
    #[serde(default)]
    pub groups: Vec<String>,
    /// When to start listing data at
    #[serde(default = "Utc::now")]
    pub start: DateTime<Utc>,
    /// When to stop listing data at
    pub end: Option<DateTime<Utc>>,
    /// The tags to filter on
    #[serde(default)]
    pub tags: HashMap<String, Vec<String>>,
    /// The cursor id to use if one exists
    pub cursor: Option<Uuid>,
    /// The max number of items to return in this response
    #[serde(default = "default_list_limit")]
    pub limit: usize,
    /// The different kinds of entities to list
    #[serde(default = "default_entity_kinds")]
    pub kinds: Vec<EntityKinds>,
}

impl Default for EntityListParams {
    /// Create default entity list params
    fn default() -> Self {
        Self {
            groups: Vec::default(),
            start: Utc::now(),
            end: None,
            tags: HashMap::default(),
            cursor: None,
            limit: default_list_limit(),
            kinds: default_entity_kinds(),
        }
    }
}

impl EntityListParams {
    /// Get the end timestamp or get a sane default
    #[cfg(feature = "api")]
    pub fn end(&self, shared: &crate::utils::Shared) -> Result<DateTime<Utc>, ApiError> {
        match self.end {
            Some(end) => Ok(end),
            None => match Utc.timestamp_opt(shared.config.thorium.entities.earliest, 0) {
                chrono::LocalResult::Single(default_end) => Ok(default_end),
                _ => crate::internal_err!(format!(
                    "default earliest repos timestamp is invalid or ambiguous - {}",
                    shared.config.thorium.entities.earliest
                )),
            },
        }
    }
}

// A single entity line missing supplementary data like name and kind
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct EntityListLine {
    /// The group this entity is apart of (used only for cursor generation)
    #[serde(skip_serializing, skip_deserializing)]
    pub groups: HashSet<String>,
    /// The entity's unique ID
    pub id: Uuid,
    /// The entity's name
    pub name: String,
    /// The kind of entity this is (without the kind's data)
    pub kind: EntityKinds,
    /// The time this entity was created
    pub created: DateTime<Utc>,
}

/// An update to apply to an entity
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct EntityUpdate {
    /// The new name to set
    pub name: Option<String>,
    /// The groups to add to this entity
    pub add_groups: Vec<String>,
    /// The groups to remove form this entity
    pub remove_groups: Vec<String>,
    /// The decsription to set for this entity
    pub description: Option<String>,
    /// Clear this entities description
    pub clear_description: bool,
    /// The kind-specific metadata update to apply, if any
    pub metadata: Option<EntityMetadataUpdate>,
}

impl EntityUpdate {
    /// Change the name for this entity
    ///
    /// # Arguments
    ///
    /// * `name` - The name to change too
    #[must_use]
    pub fn name(mut self, name: impl Into<String>) -> Self {
        // update our name
        self.name = Some(name.into());
        self
    }

    /// Add a group to this entity
    ///
    /// # Arguments
    ///
    /// * `group` - The group to add
    #[must_use]
    pub fn group(mut self, group: impl Into<String>) -> Self {
        // add this group
        self.add_groups.push(group.into());
        self
    }

    /// Remove a group from this entity
    ///
    /// # Arguments
    ///
    /// * `group` - The group to remove
    #[must_use]
    pub fn remove_group(mut self, group: impl Into<String>) -> Self {
        // add this group to our remove list
        self.remove_groups.push(group.into());
        self
    }

    /// Change the description for this entity
    ///
    /// # Arguments
    ///
    /// * `description` - The description to change too
    #[must_use]
    pub fn description(mut self, description: impl Into<String>) -> Self {
        // update our description
        self.description = Some(description.into());
        self
    }

    /// Clear this entities description
    #[must_use]
    pub fn clear_description(mut self) -> Self {
        self.clear_description = true;
        self
    }

    /// Set the kind-specific metadata update to apply
    ///
    /// # Arguments
    ///
    /// * `metadata` - The kind-specific metadata update to apply
    #[must_use]
    pub fn metadata(mut self, metadata: EntityMetadataUpdate) -> Self {
        // set our kind-specific metadata update
        self.metadata = Some(metadata);
        self
    }

    /// Convert this update to a multipart form
    #[cfg(feature = "client")]
    pub fn to_form(mut self) -> Result<reqwest::multipart::Form, crate::Error> {
        // build a form object to add our form data too
        let form = reqwest::multipart::Form::new()
            // disable percent encoding, as the API natively supports UTF-8
            .percent_encode_noop()
            // always set our clear description field
            .text("clear_description", self.clear_description.to_string());
        // set our name form field
        let form = multipart_text!(form, "name", self.name);
        // add the groups to add/remove to this form (list fields require a trailing `[]`)
        let form = multipart_list!(form, "add_groups[]", self.add_groups);
        let form = multipart_list!(form, "remove_groups[]", self.remove_groups);
        // set our description form field
        let form = multipart_text!(form, "description", self.description);
        // add any kind-specific metadata update fields
        let form = match self.metadata {
            Some(metadata) => metadata.add_to_form(form)?,
            None => form,
        };
        Ok(form)
    }
}

/// A kind-specific metadata update for an entity
///
/// Each variant carries only the fields that are valid to update for that entity
/// kind, so invalid field/kind combinations are rejected at compile time. Fields
/// default to empty (no change) so callers only set what they intend to change.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum EntityMetadataUpdate {
    /// Updates for a device entity
    Device {
        /// The urls to add to this device
        add_urls: Vec<String>,
        /// The urls to remove from this device
        remove_urls: Vec<String>,
        /// Whether this device is a critical system
        critical_system: Option<bool>,
        /// Clear this devices critical system flag
        clear_critical_system: Option<bool>,
        /// Whether this device is in a sensitive location
        sensitive_location: Option<bool>,
        /// Clear this devices sensitive location flag
        clear_sensitive_location: Option<bool>,
        /// The critical sectors to add to this device
        add_critical_sectors: Vec<shared::CriticalSector>,
        /// The critical sectors to remove from this device
        remove_critical_sectors: Vec<shared::CriticalSector>,
    },
    /// Updates for a vendor entity
    Vendor {
        /// The countries (alpha-2 codes) to add to this vendor
        add_countries: Vec<String>,
        /// The countries (alpha-2 codes) to remove from this vendor
        remove_countries: Vec<String>,
        /// The critical sectors to add to this vendor
        add_critical_sectors: Vec<shared::CriticalSector>,
        /// The critical sectors to remove from this vendor
        remove_critical_sectors: Vec<shared::CriticalSector>,
    },
    /// Updates for a collection entity
    Collection {
        /// The collection tags to add, keyed by tag key
        add_collection_tags: std::collections::HashMap<String, HashSet<String>>,
        /// The collection tags to delete, keyed by tag key
        delete_collection_tags: std::collections::HashMap<String, HashSet<String>>,
        /// Whether tag matching for this collection should be case-insensitive
        tags_case_insensitive: Option<bool>,
        /// Whether this collection should ignore group filtering
        ignore_groups: Option<bool>,
        /// When to start listing data for this collection
        start: Option<DateTime<Utc>>,
        /// When to stop listing data for this collection
        end: Option<DateTime<Utc>>,
        /// Clear this collections start point
        clear_start: Option<bool>,
        /// Clear this collections end point
        clear_end: Option<bool>,
    },
    /// Updates for a filesystem entity
    FileSystem {
        /// The tool instances to add to this filesystem
        add_tools: Vec<Uuid>,
        /// The tool instances to remove from this filesystem
        remove_tools: Vec<Uuid>,
        /// The legacy tool names to remove from this filesystem
        #[serde(default)]
        remove_legacy_tools: Vec<String>,
    },
    /// Updates for a windows process tree entity
    WindowsProcessTree {
        /// The tool instances to add to this process tree
        add_tools: Vec<Uuid>,
        /// The tool instances to remove from this process tree
        remove_tools: Vec<Uuid>,
        /// The legacy tool names to remove from this process tree
        #[serde(default)]
        remove_legacy_tools: Vec<String>,
    },
    /// Updates for a windows process entity
    WindowsProcess {
        /// The process name to set
        name: Option<String>,
        /// The image path to set
        image_path: Option<String>,
        /// The command line to set
        command: Option<String>,
        /// The offset to set
        offset: Option<u64>,
        /// The thread count to set
        threads: Option<u32>,
        /// The handle count to set
        handles: Option<u32>,
        /// Whether this process is wow64
        is_wow64: Option<bool>,
        /// The session id to set
        session_id: Option<u32>,
        /// The create time to set
        create_time: Option<DateTime<Utc>>,
        /// The exit time to set
        exit_time: Option<DateTime<Utc>>,
    },
    /// Updates for a network connection entity
    NetworkConnection {
        /// The transport layer protocol to set
        protocol: Option<network_activity::TransportLayerProtocol>,
        /// The source address to set
        #[cfg_attr(feature = "api", schema(value_type = Option<String>))]
        source: Option<IpAddr>,
        /// The source port to set
        source_port: Option<u16>,
        /// The destination address to set
        #[cfg_attr(feature = "api", schema(value_type = Option<String>))]
        destination: Option<IpAddr>,
        /// The destination port to set
        destination_port: Option<u16>,
        /// The connection state to set
        state: Option<network_activity::NetConState>,
        /// The owning process id to set
        pid: Option<u64>,
        /// The owning process name to set
        process: Option<String>,
        /// The create time to set
        create_time: Option<DateTime<Utc>>,
    },
    /// Updates for a sigma rule entity
    SigmaRule {
        /// The new sigma rule to set
        sigma_rule: Option<String>,
        /// The new score to set
        score: Option<i64>,
        /// The things this rule should also apply to
        add_applies_to: Vec<SigmaRuleAppliesTo>,
        /// The things this rule should no longer apply to
        remove_applies_to: Vec<SigmaRuleAppliesTo>,
        /// The actions to add to this rule
        add_actions: Vec<rules::SigmaActionToTake>,
        /// The action indices to remove from this rule
        remove_actions: std::collections::BTreeSet<usize>,
    },
    /// Updates for a flag entity
    Flag {
        /// The suspicion score to set
        suspicion: Option<i64>,
        /// The confidence to set
        confidence: Option<Confidence>,
        /// The reasoning to set
        reasoning: Option<String>,
        /// The content to set
        content: Option<String>,
    },
    /// Updates for an incident entity
    Incident {
        /// The cover term to set
        cover_term: Option<String>,
        /// The mission teams to add
        add_mission_teams: Vec<String>,
        /// The mission teams to remove
        remove_mission_teams: Vec<String>,
        /// The networks to add
        add_networks: Vec<String>,
        /// The networks to remove
        remove_networks: Vec<String>,
        /// The machines to add
        add_machines: Vec<String>,
        /// The machines to remove
        remove_machines: Vec<String>,
        /// The locations to add
        add_locations: Vec<String>,
        /// The locations to remove
        remove_locations: Vec<String>,
    },
    /// Updates for a compiled function entity
    CompiledFunction {
        /// The function address to set
        function_address: Option<u64>,
        /// The disassembly to replace this functions disassembly with
        disassembly: Vec<CompiledInstruction>,
    },
    /// Updates for a decompiled function entity
    DecompiledFunction {
        /// The function address to set
        function_address: Option<u64>,
        /// The decompilation content to set
        decompilation_content: Option<String>,
        /// The tool instances to add to this function
        add_tools: Vec<Uuid>,
        /// The tool instances to remove from this function
        remove_tools: Vec<Uuid>,
        /// The legacy tool names to remove from this function
        #[serde(default)]
        remove_legacy_tools: Vec<String>,
    },
    /// Updates for a PE section entity
    PeSection {
        /// The MD5 of this sections raw data
        md5: Option<String>,
        /// The raw (on disk) size of this section
        raw_size: Option<u64>,
        /// The virtual (in memory) size of this section
        virtual_size: Option<u64>,
        /// The Shannon entropy of this sections data
        entropy: Option<f64>,
    },
    /// Updates for a PE import entity
    PeImport {
        /// The imported functions to replace this librarys functions with
        functions: Vec<String>,
    },
    /// Updates for a tool entity (setting a spec publishes a new tool instance)
    Tool {
        /// The version label for the new tool instance (required when `spec` is set)
        version: Option<String>,
        /// The JSON encoded `ToolSpec` for the new tool instance
        spec: Option<String>,
        /// The native agentic tool instances the new instance provides (Harness tools only, with
        /// `spec`)
        #[serde(default)]
        provides: Vec<Uuid>,
        /// The new JSON encoded `HarnessEnv` for this tool (Harness tools only)
        #[serde(default)]
        harness_env: Option<String>,
        /// Clear this tool's `harness_env`
        #[serde(default)]
        clear_harness_env: Option<bool>,
        /// Model ids to add to this harness's `disabled_models`
        #[serde(default)]
        disable_models: std::collections::BTreeSet<String>,
        /// Model ids to remove from this harness's `disabled_models`
        #[serde(default)]
        enable_models: std::collections::BTreeSet<String>,
    },
    /// Updates for a tool instance entity
    ToolInstance {
        /// Whether this tool instance is deprecated
        deprecated: Option<bool>,
    },
    /// Updates for an agent entity
    Agent {
        /// The new configuration for this agent, replacing the whole config
        config: Option<agents::AgentConfig>,
        /// Which levels may supply model auth for this agent (does not change `config_hash`)
        model_auth: Option<models::ModelAuthSource>,
        /// The harness to set for this agent (Tool ID = latest, `ToolInstance` ID = pinned)
        harness: Option<Uuid>,
        /// Clear this agent's harness
        clear_harness: Option<bool>,
        /// The tools to add to this agent (Tool IDs = latest, `ToolInstance` IDs = pinned)
        #[serde(default)]
        add_tools: Vec<Uuid>,
        /// The tools to remove from this agent
        #[serde(default)]
        remove_tools: Vec<Uuid>,
        /// The toolsets to add to this agent (`ToolSet` IDs = latest, `ToolSetInstance` IDs =
        /// pinned)
        #[serde(default)]
        add_tool_sets: Vec<Uuid>,
        /// The toolsets to remove from this agent
        #[serde(default)]
        remove_tool_sets: Vec<Uuid>,
        /// The sub agents to add to this agent
        #[serde(default)]
        add_sub_agents: Vec<Uuid>,
        /// The sub agents to remove from this agent
        #[serde(default)]
        remove_sub_agents: Vec<Uuid>,
        /// Whether to add the agent's groups that an added edge's target is missing to that
        /// target instead of rejecting the edge
        #[serde(default)]
        extend_target_groups: bool,
    },
    /// Updates for a toolset entity
    ToolSet {
        /// Whether only read-only tools (whose instance declares no effects) are included
        read_only: Option<bool>,
        /// The new tag selector for this toolset
        selector: Option<toolsets::ToolSetSelector>,
        /// Clear this toolset's tag selector
        clear_selector: Option<bool>,
        /// Whether the event handler should create new instances automatically
        auto_instance: Option<bool>,
        /// The tools to include (Tool IDs = latest, `ToolInstance` IDs = pinned)
        #[serde(default)]
        add_tools: Vec<Uuid>,
        /// The tools to stop including
        #[serde(default)]
        remove_tools: Vec<Uuid>,
        /// Whether to add the toolset's groups that an added tool is missing to that tool
        /// instead of rejecting the edge
        #[serde(default)]
        extend_target_groups: bool,
    },
    /// Updates for a workflow catalog entity
    WorkflowCatalog {
        /// The new entry agent for this workflow
        entry_agent: Option<Uuid>,
        /// Clear this workflow's entry agent
        clear_entry_agent: Option<bool>,
        /// Whether the event handler should create new instances automatically
        auto_instance: Option<bool>,
        /// The new run-wide limits for this workflow
        limits: Option<workflows::WorkflowLimits>,
        /// The new default models for agents whose own list is empty (replaces the whole list)
        default_models: Option<Vec<models::ModelMatcher>>,
        /// Which levels may supply model auth for this workflow's runs (does not change the
        /// digest)
        model_auth: Option<models::ModelAuthSource>,
        /// The agents to add to this workflow
        #[serde(default)]
        add_agents: Vec<Uuid>,
        /// The agents to remove from this workflow
        #[serde(default)]
        remove_agents: Vec<Uuid>,
        /// Whether to add the workflow's groups that an added agent is missing to that agent
        /// instead of rejecting the edge
        #[serde(default)]
        extend_target_groups: bool,
    },
    /// Updates for a model endpoint entity
    ModelEndpoint {
        /// The new provider connection
        provider: Option<models::ModelProvider>,
        /// The group credential to authenticate with (moves the `AuthenticatesWith` association)
        credential: Option<Uuid>,
        /// Clear this endpoint's group credential
        clear_credential: Option<bool>,
        /// Catalog entries to add, or to replace by `model_id` (stored as `Manual`)
        add_models: Vec<models::ModelCatalogEntryRequest>,
        /// Catalog entries to remove by `model_id`
        remove_models: Vec<String>,
        /// Bans to add to or remove from this endpoint
        bans: models::ModelBanUpdate,
        /// Bans to add to or remove from catalog entries, by `model_id`
        model_bans: std::collections::BTreeMap<String, models::ModelBanUpdate>,
        /// Model ids to add to `disabled_models`
        disable_models: std::collections::BTreeSet<String>,
        /// Model ids to remove from `disabled_models`
        enable_models: std::collections::BTreeSet<String>,
        /// A listing result to merge into the catalog (admins only)
        refresh_report: Option<models::ModelRefreshReport>,
    },
    /// Updates for a model profile entity
    ModelProfile {
        /// The endpoint to call (moves the `Serves` association)
        endpoint: Option<Uuid>,
        /// The exact model id to call
        model_id: Option<String>,
        /// The protocol to call the model with
        api: Option<models::ModelApi>,
        /// The new parameters
        parameters: Option<models::ModelParameters>,
        /// Limits that replace the catalog entry's
        limits: Option<models::ModelLimits>,
        /// Clear this profile's limits
        clear_limits: Option<bool>,
        /// Capabilities that replace the catalog entry's
        capabilities: Option<models::ModelCapabilities>,
        /// Clear this profile's capabilities
        clear_capabilities: Option<bool>,
        /// Bans to add to or remove from this profile
        bans: models::ModelBanUpdate,
    },
    /// Updates for a model credential entity (setting `secret` rotates it)
    ModelCredential {
        /// The new primary secret
        secret: Option<models::ModelSecret>,
        /// The new AWS session token
        session_token: Option<models::ModelSecret>,
        /// Clear the AWS session token
        clear_session_token: Option<bool>,
        /// When this credential stops working
        expires: Option<DateTime<Utc>>,
        /// Clear this credential's expiration
        clear_expires: Option<bool>,
        /// Bans to add to or remove from this credential
        bans: models::ModelBanUpdate,
    },
}

impl EntityMetadataUpdate {
    /// Add this kind-specific metadata update to a multipart form
    ///
    /// All fields are nested under `metadata[..]` and list fields use a trailing
    /// `[]`, matching the server's `EntityMetadataUpdateForm` parser.
    ///
    /// # Arguments
    ///
    /// * `form` - The form to add our metadata update fields to
    #[cfg(feature = "client")]
    #[allow(clippy::too_many_lines)]
    pub fn add_to_form(
        self,
        form: reqwest::multipart::Form,
    ) -> Result<reqwest::multipart::Form, crate::Error> {
        // allow the form to be reassigned as we add each field
        let mut form = form;
        // add the fields specific to our kind
        match self {
            EntityMetadataUpdate::Device {
                mut add_urls,
                mut remove_urls,
                critical_system,
                clear_critical_system,
                sensitive_location,
                clear_sensitive_location,
                add_critical_sectors,
                remove_critical_sectors,
            } => {
                // add/remove this device's urls
                form = crate::multipart_list!(form, "metadata[add_urls][]", add_urls);
                form = crate::multipart_list!(form, "metadata[remove_urls][]", remove_urls);
                // set or clear the critical system flag
                form = crate::multipart_text_to_string!(
                    form,
                    "metadata[critical_system]",
                    critical_system
                );
                form = crate::multipart_text_to_string!(
                    form,
                    "metadata[clear_critical_system]",
                    clear_critical_system
                );
                // set or clear the sensitive location flag
                form = crate::multipart_text_to_string!(
                    form,
                    "metadata[sensitive_location]",
                    sensitive_location
                );
                form = crate::multipart_text_to_string!(
                    form,
                    "metadata[clear_sensitive_location]",
                    clear_sensitive_location
                );
                // add/remove this device's critical sectors (sent as their string representation)
                for sector in add_critical_sectors {
                    form = form.text("metadata[add_critical_sectors][]", sector.to_string());
                }
                for sector in remove_critical_sectors {
                    form = form.text("metadata[remove_critical_sectors][]", sector.to_string());
                }
            }
            EntityMetadataUpdate::Vendor {
                add_countries,
                remove_countries,
                add_critical_sectors,
                remove_critical_sectors,
            } => {
                // add/remove this vendor's countries (alpha-2 codes)
                for country in add_countries {
                    form = form.text("metadata[add_countries][]", country);
                }
                for country in remove_countries {
                    form = form.text("metadata[remove_countries][]", country);
                }
                // add/remove this vendor's critical sectors
                for sector in add_critical_sectors {
                    form = form.text("metadata[add_critical_sectors][]", sector.to_string());
                }
                for sector in remove_critical_sectors {
                    form = form.text("metadata[remove_critical_sectors][]", sector.to_string());
                }
            }
            EntityMetadataUpdate::Collection {
                add_collection_tags,
                delete_collection_tags,
                tags_case_insensitive,
                ignore_groups,
                start,
                end,
                clear_start,
                clear_end,
            } => {
                // collection tags are nested by their tag key
                for (key, values) in add_collection_tags {
                    let field_key = format!("metadata[add_collection_tags][{key}][]");
                    for value in values {
                        form = form.text(field_key.clone(), value);
                    }
                }
                for (key, values) in delete_collection_tags {
                    let field_key = format!("metadata[delete_collection_tags][{key}][]");
                    for value in values {
                        form = form.text(field_key.clone(), value);
                    }
                }
                // set the tag matching/group filtering behavior flags
                form = crate::multipart_text_to_string!(
                    form,
                    "metadata[collection_tags_case_insensitive]",
                    tags_case_insensitive
                );
                form = crate::multipart_text_to_string!(
                    form,
                    "metadata[collection_ignore_groups]",
                    ignore_groups
                );
                // set the collection's start/end window
                form = crate::multipart_date!(form, "metadata[collection_start]", start);
                form = crate::multipart_date!(form, "metadata[collection_end]", end);
                // clear the collection's start/end window if requested
                form = crate::multipart_text_to_string!(
                    form,
                    "metadata[clear_collection_start]",
                    clear_start
                );
                form = crate::multipart_text_to_string!(
                    form,
                    "metadata[clear_collection_end]",
                    clear_end
                );
            }
            EntityMetadataUpdate::FileSystem {
                add_tools,
                remove_tools,
                remove_legacy_tools,
            }
            | EntityMetadataUpdate::WindowsProcessTree {
                add_tools,
                remove_tools,
                remove_legacy_tools,
            } => {
                // add/remove the tool instances that dumped this entity
                form = provenance_update_fields(&add_tools, &remove_tools, &remove_legacy_tools)
                    .into_iter()
                    .fold(form, |form, (name, value)| form.text(name, value));
            }
            EntityMetadataUpdate::WindowsProcess {
                mut name,
                mut image_path,
                mut command,
                offset,
                threads,
                handles,
                is_wow64,
                session_id,
                create_time,
                exit_time,
            } => {
                // set the process's descriptive string fields
                form = crate::multipart_text!(form, "metadata[name]", name);
                form = crate::multipart_text!(form, "metadata[image_path]", image_path);
                form = crate::multipart_text!(form, "metadata[command]", command);
                // set the process's numeric/boolean fields
                form = crate::multipart_text_to_string!(form, "metadata[offset]", offset);
                form = crate::multipart_text_to_string!(form, "metadata[threads]", threads);
                form = crate::multipart_text_to_string!(form, "metadata[handles]", handles);
                form = crate::multipart_text_to_string!(form, "metadata[is_wow64]", is_wow64);
                form = crate::multipart_text_to_string!(form, "metadata[session_id]", session_id);
                // set the process's create/exit timestamps
                form = crate::multipart_date!(form, "metadata[create_time]", create_time);
                form = crate::multipart_date!(form, "metadata[exit_time]", exit_time);
            }
            EntityMetadataUpdate::NetworkConnection {
                protocol,
                source,
                source_port,
                destination,
                destination_port,
                state,
                pid,
                mut process,
                create_time,
            } => {
                // set the protocol and source/destination endpoints
                form = crate::multipart_text_to_string!(form, "metadata[protocol]", protocol);
                form = crate::multipart_text_to_string!(form, "metadata[source]", source);
                form = crate::multipart_text_to_string!(form, "metadata[source_port]", source_port);
                form = crate::multipart_text_to_string!(form, "metadata[destination]", destination);
                form = crate::multipart_text_to_string!(
                    form,
                    "metadata[destination_port]",
                    destination_port
                );
                // set the connection state and owning process info
                form = crate::multipart_text_to_string!(form, "metadata[state]", state);
                form = crate::multipart_text_to_string!(form, "metadata[pid]", pid);
                form = crate::multipart_text!(form, "metadata[process]", process);
                // set the connection's create timestamp
                form = crate::multipart_date!(form, "metadata[create_time]", create_time);
            }
            EntityMetadataUpdate::SigmaRule {
                mut sigma_rule,
                score,
                mut add_applies_to,
                mut remove_applies_to,
                add_actions,
                remove_actions,
            } => {
                // set the rule body and score
                form = crate::multipart_text!(form, "metadata[sigma_rule]", sigma_rule);
                form = crate::multipart_text_to_string!(form, "metadata[score]", score);
                // add/remove what this rule applies to (sent as their string representation)
                form = crate::multipart_list_conv!(
                    form,
                    "metadata[add_sigma_applies_to][]",
                    add_applies_to
                );
                form = crate::multipart_list_conv!(
                    form,
                    "metadata[remove_sigma_applies_to][]",
                    remove_applies_to
                );
                // sigma actions are serialized to json per element
                crate::multipart_list_serialize!(
                    form,
                    "metadata[add_sigma_actions][]",
                    add_actions
                );
                // action indices to remove are sent as plain integers
                for index in remove_actions {
                    form = form.text("metadata[remove_sigma_actions][]", index.to_string());
                }
            }
            EntityMetadataUpdate::Flag {
                suspicion,
                confidence,
                mut reasoning,
                mut content,
            } => {
                // set this flag's suspicion score and confidence
                form = crate::multipart_text_to_string!(form, "metadata[suspicion]", suspicion);
                form = crate::multipart_text_to_string!(form, "metadata[confidence]", confidence);
                // set this flag's reasoning and content
                form = crate::multipart_text!(form, "metadata[reasoning]", reasoning);
                form = crate::multipart_text!(form, "metadata[content]", content);
            }
            EntityMetadataUpdate::Incident {
                mut cover_term,
                mut add_mission_teams,
                mut remove_mission_teams,
                mut add_networks,
                mut remove_networks,
                mut add_machines,
                mut remove_machines,
                mut add_locations,
                mut remove_locations,
            } => {
                // set this incident's cover term
                form = crate::multipart_text!(form, "metadata[cover_term]", cover_term);
                // add/remove this incident's mission teams
                form = crate::multipart_list!(
                    form,
                    "metadata[add_mission_teams][]",
                    add_mission_teams
                );
                form = crate::multipart_list!(
                    form,
                    "metadata[remove_mission_teams][]",
                    remove_mission_teams
                );
                // add/remove this incident's networks
                form = crate::multipart_list!(form, "metadata[add_networks][]", add_networks);
                form = crate::multipart_list!(form, "metadata[remove_networks][]", remove_networks);
                // add/remove this incident's machines
                form = crate::multipart_list!(form, "metadata[add_machines][]", add_machines);
                form = crate::multipart_list!(form, "metadata[remove_machines][]", remove_machines);
                // add/remove this incident's locations
                form = crate::multipart_list!(form, "metadata[add_locations][]", add_locations);
                form =
                    crate::multipart_list!(form, "metadata[remove_locations][]", remove_locations);
            }
            EntityMetadataUpdate::CompiledFunction {
                function_address,
                disassembly,
            } => {
                // set this function's address
                form = crate::multipart_text_to_string!(
                    form,
                    "metadata[function_address]",
                    function_address
                );
                // disassembly is serialized to json per element
                crate::multipart_list_serialize!(form, "metadata[disassembly][]", disassembly);
            }
            EntityMetadataUpdate::DecompiledFunction {
                function_address,
                mut decompilation_content,
                add_tools,
                remove_tools,
                remove_legacy_tools,
            } => {
                // set this function's address and decompilation content
                form = crate::multipart_text_to_string!(
                    form,
                    "metadata[function_address]",
                    function_address
                );
                form = crate::multipart_text!(
                    form,
                    "metadata[decompilation_content]",
                    decompilation_content
                );
                // add/remove the tool instances that decompiled this function
                form = provenance_update_fields(&add_tools, &remove_tools, &remove_legacy_tools)
                    .into_iter()
                    .fold(form, |form, (name, value)| form.text(name, value));
            }
            EntityMetadataUpdate::PeSection {
                mut md5,
                raw_size,
                virtual_size,
                entropy,
            } => {
                // set this section's content hash and sizes
                form = crate::multipart_text!(form, "metadata[md5]", md5);
                form = crate::multipart_text_to_string!(form, "metadata[raw_size]", raw_size);
                form =
                    crate::multipart_text_to_string!(form, "metadata[virtual_size]", virtual_size);
                // set this section's entropy
                form = crate::multipart_text_to_string!(form, "metadata[entropy]", entropy);
            }
            EntityMetadataUpdate::PeImport { mut functions } => {
                // replace this library's imported functions
                form = crate::multipart_list!(form, "metadata[functions][]", functions);
            }
            EntityMetadataUpdate::Tool {
                version,
                spec,
                provides,
                harness_env,
                clear_harness_env,
                disable_models,
                enable_models,
            } => {
                // add the instance to publish and the harness settings that are set
                let fields = tools::ToolUpdateFields {
                    version: version.as_ref(),
                    spec: spec.as_ref(),
                    provides: &provides,
                    harness_env: harness_env.as_ref(),
                    clear_harness_env,
                    disable_models: Some(&disable_models),
                    enable_models: Some(&enable_models),
                };
                form = tools::tool_update_form_fields(fields)
                    .into_iter()
                    .fold(form, |form, (name, value)| form.text(name, value));
            }
            EntityMetadataUpdate::ToolInstance { deprecated } => {
                // set whether this instance is deprecated
                form = tools::tool_instance_update_form_fields(deprecated)
                    .into_iter()
                    .fold(form, |form, (name, value)| form.text(name, value));
            }
            EntityMetadataUpdate::Agent {
                config,
                model_auth,
                harness,
                clear_harness,
                add_tools,
                remove_tools,
                add_tool_sets,
                remove_tool_sets,
                add_sub_agents,
                remove_sub_agents,
                extend_target_groups,
            } => {
                // add the agent fields that are set
                let fields = agents::AgentUpdateFields {
                    config: config.as_ref(),
                    model_auth: model_auth.as_ref(),
                    harness: harness.as_ref(),
                    clear_harness: clear_harness.as_ref(),
                    add_tools: &add_tools,
                    remove_tools: &remove_tools,
                    add_tool_sets: &add_tool_sets,
                    remove_tool_sets: &remove_tool_sets,
                    add_sub_agents: &add_sub_agents,
                    remove_sub_agents: &remove_sub_agents,
                    extend_target_groups,
                };
                form = fields
                    .form_fields()?
                    .into_iter()
                    .fold(form, |form, (name, value)| form.text(name, value));
            }
            EntityMetadataUpdate::ToolSet {
                read_only,
                selector,
                clear_selector,
                auto_instance,
                add_tools,
                remove_tools,
                extend_target_groups,
            } => {
                // add the toolset fields that are set
                let fields = toolsets::ToolSetUpdateFields {
                    read_only: read_only.as_ref(),
                    selector: selector.as_ref(),
                    clear_selector: clear_selector.as_ref(),
                    auto_instance: auto_instance.as_ref(),
                    add_tools: &add_tools,
                    remove_tools: &remove_tools,
                    extend_target_groups,
                };
                form = fields
                    .form_fields()
                    .into_iter()
                    .fold(form, |form, (name, value)| form.text(name, value));
            }
            EntityMetadataUpdate::WorkflowCatalog {
                entry_agent,
                clear_entry_agent,
                auto_instance,
                limits,
                default_models,
                model_auth,
                add_agents,
                remove_agents,
                extend_target_groups,
            } => {
                // add the workflow fields that are set
                let fields = workflows::WorkflowCatalogUpdateFields {
                    entry_agent: entry_agent.as_ref(),
                    clear_entry_agent: clear_entry_agent.as_ref(),
                    auto_instance: auto_instance.as_ref(),
                    limits: limits.as_ref(),
                    default_models: default_models.as_deref(),
                    model_auth: model_auth.as_ref(),
                    add_agents: &add_agents,
                    remove_agents: &remove_agents,
                    extend_target_groups,
                };
                form = fields
                    .form_fields()?
                    .into_iter()
                    .fold(form, |form, (name, value)| form.text(name, value));
            }
            update @ (EntityMetadataUpdate::ModelEndpoint { .. }
            | EntityMetadataUpdate::ModelProfile { .. }
            | EntityMetadataUpdate::ModelCredential { .. }) => {
                // add the model fields that are set
                form = models::update_form_fields(&update)?
                    .into_iter()
                    .fold(form, |form, (name, value)| form.text(name, value));
            }
        }
        Ok(form)
    }
}

/// The differnt kinds of parent info for entities
pub enum EntityParentInfo {
    /// The pid for a parent process in Windows
    WindowsParentProcess(u64),
}

impl EntityParentInfo {
    /// Get the type of sigma applies to  this parent info points to
    #[must_use]
    pub fn sigma_applies_to(&self) -> SigmaRuleAppliesTo {
        match self {
            Self::WindowsParentProcess(_) => SigmaRuleAppliesTo::WindowsProcesses,
        }
    }

    /// Get the kind of entity this parent info is for
    #[must_use]
    pub fn entity_kind(&self) -> EntityKinds {
        match self {
            Self::WindowsParentProcess(_) => EntityKinds::WindowsProcess,
        }
    }

    /// Check if an entity is the parent we are looking for
    ///
    /// # Arguments
    ///
    /// * `child` - The potential child entity request to check against
    #[must_use]
    pub fn is_parent(&self, child: &EntityRequest) -> bool {
        match (self, &child.metadata) {
            (Self::WindowsParentProcess(ppid), EntityMetadataRequest::WindowsProcess(proc)) => {
                proc.pid == *ppid
            }
            _ => false,
        }
    }
}

/// The number of entities in one partition of the entities table, read to rebuild its census
#[derive(Debug)]
#[cfg_attr(
    feature = "scylla-utils",
    derive(crate::models::schema::ScyllaProjection)
)]
#[cfg_attr(
    feature = "scylla-utils",
    thorium_projection(of = crate::models::schema::scylla::EntitiesTable)
)]
pub struct EntityCensusRow {
    /// The kind of the partition's entities
    pub kind: String,
    /// The group the partition's entities are in
    pub group: String,
    /// The year the partition's entities were created in
    pub year: i32,
    /// The partition's bucket within its year
    pub bucket: i32,
    /// How many entities the partition holds
    #[cfg_attr(feature = "scylla-utils", thorium(expr = "count(*)"))]
    pub count: i64,
}

#[cfg(feature = "scylla-utils")]
impl super::CensusSupport for Entity {
    /// The type returned by our prepared statement
    type Row = EntityCensusRow;

    /// The generated census statement's text for a namespace
    ///
    /// # Arguments
    ///
    /// * `ns` - The namespace the table is in
    fn census_cql(ns: &str) -> String {
        // the census statement generated on the table's queries struct
        crate::models::schema::queries::EntitiesQueries::census_cql(ns)
    }

    /// Get the count for this partition
    ///
    /// # Arguments
    ///
    /// * `row` - The partition's census row
    fn get_count(row: &EntityCensusRow) -> i64 {
        row.count
    }

    /// Get the bucket for this partition
    ///
    /// # Arguments
    ///
    /// * `row` - The partition's census row
    fn get_bucket(row: &EntityCensusRow) -> i32 {
        row.bucket
    }

    /// Build the sorted set key for this census operation; every kind of a group and year shares
    /// one stream, as when entities are created
    ///
    /// # Arguments
    ///
    /// * `namespace` - The namespace the census is in
    /// * `row` - The partition's census row
    fn stream_key_from_row(namespace: &str, row: &Self::Row) -> String {
        // build the stream key for this row
        format!(
            "{namespace}:census:entities:stream:{group}:{year}",
            group = row.group,
            year = row.year,
        )
    }
}

/// Get the form fields of a provenance update, by name
///
/// No field adds a legacy tool name; legacy names can only be removed.
///
/// # Arguments
///
/// * `add_tools` - The tool instances to add
/// * `remove_tools` - The tool instances to remove
/// * `remove_legacy_tools` - The legacy tool names to remove
#[must_use]
pub(crate) fn provenance_update_fields(
    add_tools: &[Uuid],
    remove_tools: &[Uuid],
    remove_legacy_tools: &[String],
) -> Vec<(&'static str, String)> {
    // the tool instances to add, then those to remove
    let adds = add_tools
        .iter()
        .map(|tool| ("metadata[add_tool_ids][]", tool.to_string()));
    let removes = remove_tools
        .iter()
        .map(|tool| ("metadata[remove_tool_ids][]", tool.to_string()));
    // the legacy tool names to remove
    let legacy = remove_legacy_tools
        .iter()
        .map(|tool| ("metadata[remove_legacy_tools][]", tool.clone()));
    adds.chain(removes).chain(legacy).collect()
}

#[cfg(test)]
mod tests {
    use strum::IntoEnumIterator;

    use super::{EntityKinds, EntityMetadata, EntityMetadataRequest, EntityRequest, GroupPolicy};

    /// The kinds whose entities are `Contained`
    const CONTAINED: &[EntityKinds] = &[
        EntityKinds::Tool,
        EntityKinds::ToolInstance,
        EntityKinds::Agent,
        EntityKinds::ToolSet,
        EntityKinds::ToolSetInstance,
        EntityKinds::WorkflowCatalog,
        EntityKinds::WorkflowInstance,
        EntityKinds::ModelEndpoint,
        EntityKinds::ModelProfile,
        EntityKinds::ModelCredential,
    ];

    /// `EntityKinds::group_policy` is `Contained` for the listed kinds and `Overlap` for every
    /// other kind
    #[test]
    fn entity_kind_group_policies() {
        // check every kind's policy
        for kind in EntityKinds::iter() {
            // get the policy this kind should have
            let expected = if CONTAINED.contains(&kind) {
                GroupPolicy::Contained
            } else {
                GroupPolicy::Overlap
            };
            // make sure the kind has that policy
            assert_eq!(kind.group_policy(), expected, "{kind}");
        }
    }

    /// Stored kind data for every entity kind that existed before tool instances, as the API
    /// wrote it, with one field of each and the JSON value it must read as
    const OLD_ROWS: &[(EntityKinds, &str, &str, &str)] = &[
        (
            EntityKinds::Device,
            r#"{"Device":{"urls":["https://corn.farm"],"vendors":[],"critical_system":true,"sensitive_location":null,"critical_sectors":["Energy"]}}"#,
            "critical_sectors",
            r#"["Energy"]"#,
        ),
        (
            EntityKinds::Vendor,
            r#"{"Vendor":{"countries":[],"critical_sectors":["Dams"]}}"#,
            "critical_sectors",
            r#"["Dams"]"#,
        ),
        (
            EntityKinds::Collection,
            r#"{"Collection":{"collection_kind":"Files","collection_tags":{"Corn":["Yellow"]},"tags_case_insensitive":false,"ignore_groups":false,"start":null,"end":null}}"#,
            "collection_tags",
            r#"{"Corn":["Yellow"]}"#,
        ),
        (
            EntityKinds::FileSystem,
            r#"{"FileSystem":{"sha256":"abc","tools":["binwalk"]}}"#,
            "legacy_tools",
            r#"["binwalk"]"#,
        ),
        (
            EntityKinds::Folder,
            r#"{"Folder":{"filesystem_id":"5f0c3c64-3c8b-4c1d-9a35-58f7c5a8e9b1","names_sha256":"a","data_sha256":"b","all_sha256":"c"}}"#,
            "all_sha256",
            r#""c""#,
        ),
        (
            EntityKinds::WindowsProcessTree,
            r#"{"WindowsProcessTree":{"tools":["volatility"]}}"#,
            "legacy_tools",
            r#"["volatility"]"#,
        ),
        (
            EntityKinds::WindowsProcess,
            r#"{"WindowsProcess":{"pid":4,"parent_pid":null,"name":"System","image_path":null,"command":null,"offset":null,"threads":12,"handles":null,"is_wow64":false,"session_id":null,"create_time":null,"exit_time":null}}"#,
            "threads",
            "12",
        ),
        (
            EntityKinds::NetworkConnection,
            r#"{"NetworkConnection":{"protocol":null,"source":"10.0.0.1","source_port":443,"destination":"10.0.0.2","destination_port":8080,"state":null,"pid":null,"process":null,"create_time":null}}"#,
            "destination_port",
            "8080",
        ),
        (
            EntityKinds::SigmaRule,
            r#"{"SigmaRule":{"rule":"title: corn","applies_to":[],"score":5,"actions":[]}}"#,
            "rule",
            r#""title: corn""#,
        ),
        (
            EntityKinds::Flag,
            r#"{"Flag":{"suspicion":3,"confidence":"Likely","content":null,"reasoning":"odd"}}"#,
            "reasoning",
            r#""odd""#,
        ),
        (
            EntityKinds::Incident,
            r#"{"Incident":{"cover_term":"husk","mission_teams":["a"],"networks":[],"machines":[],"locations":[]}}"#,
            "mission_teams",
            r#"["a"]"#,
        ),
        (
            EntityKinds::CompiledFunction,
            r#"{"CompiledFunction":{"address":16,"disassembly":[{"address":16,"instruction":"ret"}]}}"#,
            "disassembly",
            r#"[{"address":16,"instruction":"ret"}]"#,
        ),
        (
            EntityKinds::DecompiledFunction,
            r#"{"DecompiledFunction":{"address":16,"tools":["ghidra"],"content":"x"}}"#,
            "legacy_tools",
            r#"["ghidra"]"#,
        ),
        (
            EntityKinds::PeSection,
            r#"{"PeSection":{"md5":"d41d8cd98f00b204e9800998ecf8427e","raw_size":512,"virtual_size":null,"entropy":7.5}}"#,
            "raw_size",
            "512",
        ),
        (
            EntityKinds::PeImport,
            r#"{"PeImport":{"functions":["CreateFileW"]}}"#,
            "functions",
            r#"["CreateFileW"]"#,
        ),
        (EntityKinds::Other, r#""Other""#, "", ""),
    ];

    /// Kind data stored for every entity kind that existed before tool instances still reads as
    /// that kind and keeps its values
    #[test]
    fn old_entity_rows_deserialize() {
        // read every stored row
        for (kind, raw, field, expected) in OLD_ROWS {
            // read this row's kind data
            let metadata: EntityMetadata = serde_json::from_str(raw)
                .unwrap_or_else(|err| panic!("{kind} row failed to read: {err}"));
            // it keeps its kind
            assert_eq!(EntityKinds::from(&metadata), *kind);
            // kinds without data have no field to check
            if field.is_empty() {
                continue;
            }
            // the checked field reads with the stored value
            let read = serde_json::to_value(&metadata).unwrap();
            let expected: serde_json::Value = serde_json::from_str(expected).unwrap();
            assert_eq!(read[kind.as_str()][*field], expected, "{kind}.{field}");
        }
    }

    /// Stored provenance kinds read their tool names into `legacy_tools` through the entity
    /// metadata they're stored in, and tool instance ids into `tools`
    #[test]
    fn old_provenance_rows_split_tools() {
        // build stored rows that mix a tool name with a tool instance id
        let id = uuid::Uuid::new_v4();
        let rows = [
            format!(r#"{{"FileSystem":{{"sha256":"abc","tools":["binwalk","{id}"]}}}}"#),
            format!(r#"{{"WindowsProcessTree":{{"tools":["binwalk","{id}"]}}}}"#),
            format!(
                r#"{{"DecompiledFunction":{{"address":1,"tools":["binwalk","{id}"],"content":"x"}}}}"#
            ),
        ];
        // read each row
        for raw in &rows {
            // read this row's kind data and get its tool references
            let (tools, legacy_tools) = match serde_json::from_str(raw).unwrap() {
                EntityMetadata::FileSystem(fs) => (fs.tools, fs.legacy_tools),
                EntityMetadata::WindowsProcessTree(tree) => (tree.tools, tree.legacy_tools),
                EntityMetadata::DecompiledFunction(func) => (func.tools, func.legacy_tools),
                other => panic!("unexpected kind read from {raw}: {other:?}"),
            };
            // the id is a tool instance and the name is a legacy tool
            assert_eq!(tools, vec![id], "{raw}");
            assert_eq!(legacy_tools, vec!["binwalk".to_owned()], "{raw}");
        }
    }

    /// Entity requests that name a process tree without data, as tools and stored results wrote
    /// them before process trees listed tool instances, read as an empty process tree, while
    /// every other request shape reads and writes unchanged
    #[test]
    fn bare_process_tree_requests_deserialize() {
        // read a request whose kind is the bare process tree variant name
        let raw = r#"{"name":"t","metadata":"WindowsProcessTree","groups":[],"tags":{},"description":null}"#;
        let req: EntityRequest = serde_json::from_str(raw).unwrap();
        // it reads as a process tree with no tools
        match &req.metadata {
            EntityMetadataRequest::WindowsProcessTree(tree) => {
                assert_eq!(tree.tools, Vec::<uuid::Uuid>::new());
                assert_eq!(tree.legacy_tools, Vec::<String>::new());
            }
            other => panic!("unexpected kind: {other:?}"),
        }
        // it writes in the process tree's data shape
        let written = serde_json::to_value(&req.metadata).unwrap();
        assert_eq!(
            written,
            serde_json::json!({"WindowsProcessTree": {"tools": []}})
        );
        // the other kind without data still reads from and writes to its bare name
        let other: EntityMetadataRequest = serde_json::from_str(r#""Other""#).unwrap();
        assert!(matches!(other, EntityMetadataRequest::Other));
        assert_eq!(serde_json::to_string(&other).unwrap(), r#""Other""#);
        // process trees with data still read their tool instances
        let id = uuid::Uuid::new_v4();
        let tree = format!(r#"{{"WindowsProcessTree":{{"tools":["{id}"]}}}}"#);
        match serde_json::from_str(&tree).unwrap() {
            EntityMetadataRequest::WindowsProcessTree(tree) => assert_eq!(tree.tools, vec![id]),
            other => panic!("unexpected kind: {other:?}"),
        }
        // other kinds with data still read
        let fs: EntityMetadataRequest =
            serde_json::from_str(r#"{"FileSystem":{"sha256":"abc","tools":[]}}"#).unwrap();
        assert!(matches!(fs, EntityMetadataRequest::FileSystem(_)));
        // unknown bare names and unknown kinds are still refused
        assert!(serde_json::from_str::<EntityMetadataRequest>(r#""Corn""#).is_err());
        assert!(serde_json::from_str::<EntityMetadataRequest>(r#"{"Corn":{}}"#).is_err());
        // a malformed process tree is refused with the field's own error
        let err =
            serde_json::from_str::<EntityMetadataRequest>(r#"{"WindowsProcessTree":{"tools":5}}"#)
                .unwrap_err();
        assert!(err.to_string().contains("invalid type"), "{err}");
    }

    /// Provenance updates send tool instance ids and legacy name removals, and never send a
    /// field that adds a tool name
    #[test]
    fn provenance_update_fields() {
        // build the fields of an update that touches every provenance list
        let add = uuid::Uuid::new_v4();
        let remove = uuid::Uuid::new_v4();
        let fields = super::provenance_update_fields(&[add], &[remove], &["binwalk".to_owned()]);
        // each list is sent under its tool instance or legacy name field
        assert_eq!(
            fields,
            vec![
                ("metadata[add_tool_ids][]", add.to_string()),
                ("metadata[remove_tool_ids][]", remove.to_string()),
                ("metadata[remove_legacy_tools][]", "binwalk".to_owned()),
            ]
        );
    }
}
