//! The documents `models endpoints edit` and `models profiles edit` show and read back
//!
//! A document names things (a credential, an endpoint) instead of carrying ids and holds no
//! secret: credentials appear by name only and their secrets never leave the API, so a changed
//! secret can only go through `models credentials create`, `rotate` or `update`.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use thorium::models::{
    Entity, EntityKinds, EntityMetadata, EntityMetadataUpdate, EntityUpdate, ModelApi,
    ModelCapabilities, ModelCatalogEntry, ModelCatalogEntryRequest, ModelEndpointEntity,
    ModelLimits, ModelParameters, ModelProfileEntity, ModelProvider,
};
use thorium::{CtlConf, Error, Thorium};

use super::find;
use crate::args::models::{EditModel, SpecModel};
use crate::handlers::edit::{Editable, default_header, edit_with, update_with};
use crate::utils;
use crate::utils::output::OutputFormat;

/// What a model can do, as a document names it (every key is optional and defaults to false)
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
// each capability is an independent flag of the model
#[allow(clippy::struct_excessive_bools)]
#[serde(default, deny_unknown_fields)]
pub struct CapabilitiesView {
    /// The model can call tools
    pub tool_calling: bool,
    /// The model accepts images
    pub vision: bool,
    /// The model accepts PDF documents
    pub pdf_input: bool,
    /// The model can produce output that follows a JSON schema
    pub structured_output: bool,
    /// The model supports extended thinking or reasoning
    pub thinking: bool,
    /// The model accepts strict tool schemas
    pub strict_tools: bool,
    /// The model's API can turn parallel tool calls off for it
    pub parallel_tool_calls: bool,
    /// The model accepts a forced tool choice
    pub forced_tool_choice: bool,
}

impl From<&ModelCapabilities> for CapabilitiesView {
    /// Show the capabilities of a stored model
    ///
    /// # Arguments
    ///
    /// * `caps` - The stored capabilities
    fn from(caps: &ModelCapabilities) -> Self {
        CapabilitiesView {
            tool_calling: caps.tool_calling,
            vision: caps.vision,
            pdf_input: caps.pdf_input,
            structured_output: caps.structured_output,
            thinking: caps.thinking,
            strict_tools: caps.strict_tools,
            parallel_tool_calls: caps.parallel_tool_calls,
            forced_tool_choice: caps.forced_tool_choice,
        }
    }
}

impl From<CapabilitiesView> for ModelCapabilities {
    /// Turn a document's capabilities into the API's
    ///
    /// # Arguments
    ///
    /// * `view` - The capabilities as the document names them
    fn from(view: CapabilitiesView) -> Self {
        ModelCapabilities {
            tool_calling: view.tool_calling,
            vision: view.vision,
            pdf_input: view.pdf_input,
            structured_output: view.structured_output,
            thinking: view.thinking,
            strict_tools: view.strict_tools,
            parallel_tool_calls: view.parallel_tool_calls,
            forced_tool_choice: view.forced_tool_choice,
        }
    }
}

/// The token limits a document sets by hand
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LimitsView {
    /// The max input tokens
    #[serde(default)]
    pub context_window: Option<u64>,
    /// The max output tokens
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
}

impl From<&ModelLimits> for LimitsView {
    /// Show the limits of a stored profile
    ///
    /// # Arguments
    ///
    /// * `limits` - The stored limits
    fn from(limits: &ModelLimits) -> Self {
        LimitsView {
            context_window: limits.context_window,
            max_output_tokens: limits.max_output_tokens,
        }
    }
}

impl From<LimitsView> for ModelLimits {
    /// Turn limits written in a document into limits set by hand
    ///
    /// # Arguments
    ///
    /// * `view` - The limits as the document names them
    fn from(view: LimitsView) -> Self {
        ModelLimits {
            context_window: view.context_window,
            max_output_tokens: view.max_output_tokens,
            ..ModelLimits::default()
        }
    }
}

/// The sampling and generation parameters a profile applies to every call
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParametersView {
    /// The sampling temperature
    #[serde(default)]
    pub temperature: Option<f64>,
    /// The nucleus sampling probability mass
    #[serde(default)]
    pub top_p: Option<f64>,
    /// The max tokens to produce per response
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
    /// The token budget for extended thinking
    #[serde(default)]
    pub thinking_budget_tokens: Option<u64>,
    /// Sequences that stop generation
    #[serde(default)]
    pub stop_sequences: Vec<String>,
}

impl From<&ModelParameters> for ParametersView {
    /// Show the parameters of a stored profile
    ///
    /// # Arguments
    ///
    /// * `params` - The stored parameters
    fn from(params: &ModelParameters) -> Self {
        ParametersView {
            temperature: params.temperature,
            top_p: params.top_p,
            max_output_tokens: params.max_output_tokens,
            thinking_budget_tokens: params.thinking_budget_tokens,
            stop_sequences: params.stop_sequences.clone(),
        }
    }
}

impl From<ParametersView> for ModelParameters {
    /// Turn a document's parameters into the API's
    ///
    /// # Arguments
    ///
    /// * `view` - The parameters as the document names them
    fn from(view: ParametersView) -> Self {
        ModelParameters {
            temperature: view.temperature,
            top_p: view.top_p,
            max_output_tokens: view.max_output_tokens,
            thinking_budget_tokens: view.thinking_budget_tokens,
            stop_sequences: view.stop_sequences,
        }
    }
}

/// One model of an endpoint's catalog, as a document lists it
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogRow {
    /// The provider's exact model id
    pub model_id: String,
    /// The max input tokens
    #[serde(default)]
    pub context_window: Option<u64>,
    /// The max output tokens
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
    /// What the model can do
    #[serde(default)]
    pub capabilities: CapabilitiesView,
}

impl From<&ModelCatalogEntry> for CatalogRow {
    /// Show a stored catalog entry
    ///
    /// # Arguments
    ///
    /// * `entry` - The stored entry
    fn from(entry: &ModelCatalogEntry) -> Self {
        CatalogRow {
            model_id: entry.model_id.clone(),
            context_window: entry.limits.context_window,
            max_output_tokens: entry.limits.max_output_tokens,
            capabilities: CapabilitiesView::from(&entry.capabilities),
        }
    }
}

impl From<CatalogRow> for ModelCatalogEntryRequest {
    /// Turn a document's catalog row into the request that adds or replaces an entry
    ///
    /// # Arguments
    ///
    /// * `row` - The row as the document lists it
    fn from(row: CatalogRow) -> Self {
        ModelCatalogEntryRequest {
            model_id: row.model_id,
            context_window: row.context_window,
            max_output_tokens: row.max_output_tokens,
            capabilities: row.capabilities.into(),
        }
    }
}

/// A provider as a document holds it
///
/// A provider is an enum whose variants hold data, which YAML text names with a map key and which
/// the YAML reader only accepts as a tag. Reading the document as a plain value first accepts the
/// map form that `spec` and the editor show.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ProviderDoc(pub ModelProvider);

impl<'de> Deserialize<'de> for ProviderDoc {
    /// Read a provider from its map form
    ///
    /// # Arguments
    ///
    /// * `deserializer` - The deserializer holding the document
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // read the provider as a plain value, then as the provider it describes
        let value = serde_json::Value::deserialize(deserializer)?;
        serde_json::from_value(value)
            .map(ProviderDoc)
            .map_err(serde::de::Error::custom)
    }
}

/// A model endpoint as the edit flow reads it
#[derive(Debug, Clone)]
pub struct EndpointData {
    /// The entity as the API returned it
    pub entity: Entity,
    /// The entity's endpoint settings
    pub meta: ModelEndpointEntity,
    /// The name of the credential the endpoint authenticates with, if it has one
    pub credential: Option<String>,
}

/// The document users edit for a model endpoint
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointView {
    /// The endpoint's id (read-only)
    #[serde(rename = "*id*", default)]
    pub id: String,
    /// The endpoint's name
    pub name: String,
    /// What the endpoint is for
    #[serde(default)]
    pub description: Option<String>,
    /// The groups the endpoint is in (read-only)
    #[serde(rename = "*groups*", default)]
    pub groups: Vec<String>,
    /// The provider and how to connect to it
    pub provider: ProviderDoc,
    /// The name of the group credential to authenticate with, or none
    #[serde(default)]
    pub credential: Option<String>,
    /// Exact model ids the endpoint never resolves to
    #[serde(default)]
    pub disabled_models: Vec<String>,
    /// The models the endpoint serves; leave one out to remove it
    #[serde(default)]
    pub catalog: Vec<CatalogRow>,
}

/// A model profile as the edit flow reads it
#[derive(Debug, Clone)]
pub struct ProfileData {
    /// The entity as the API returned it
    pub entity: Entity,
    /// The entity's profile settings
    pub meta: ModelProfileEntity,
    /// The name of the endpoint the profile calls
    pub endpoint: String,
}

/// The document users edit for a model profile
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileView {
    /// The profile's id (read-only)
    #[serde(rename = "*id*", default)]
    pub id: String,
    /// The profile's name
    pub name: String,
    /// What the profile is for
    #[serde(default)]
    pub description: Option<String>,
    /// The groups the profile is in (read-only)
    #[serde(rename = "*groups*", default)]
    pub groups: Vec<String>,
    /// The name of the endpoint the profile calls
    pub endpoint: String,
    /// The exact model id to call
    pub model_id: String,
    /// The protocol to call the model with
    pub api: ModelApi,
    /// The parameters applied to every call
    #[serde(default)]
    pub parameters: ParametersView,
    /// Limits that replace the catalog entry's, or none to use the entry's
    #[serde(default)]
    pub limits: Option<LimitsView>,
    /// Capabilities that replace the catalog entry's, or none to use the entry's
    #[serde(default)]
    pub capabilities: Option<CapabilitiesView>,
}

/// Check the parts of a document every model kind shares
///
/// # Arguments
///
/// * `name` - The name the document gives the entity
fn check_name(name: &str) -> Result<(), Error> {
    // an entity needs a name
    if name.trim().is_empty() {
        return Err(Error::new("name can't be empty"));
    }
    Ok(())
}

/// Start the update of the root fields every model kind shares
///
/// # Arguments
///
/// * `entity` - The entity as fetched
/// * `name` - The name the document gives the entity
/// * `description` - The description the document gives the entity
fn root_update(entity: &Entity, name: &str, description: Option<&String>) -> EntityUpdate {
    // start from an update that changes nothing
    let mut update = EntityUpdate::default();
    // a new name renames the entity
    if name != entity.name {
        update.name = Some(name.to_owned());
    }
    // a new description replaces the old one, and none clears it
    if description != entity.description.as_ref() {
        match description {
            Some(text) => update.description = Some(text.clone()),
            None => update.clear_description = true,
        }
    }
    update
}

/// Whether an update changes nothing
///
/// # Arguments
///
/// * `update` - The update to check
fn is_empty(update: &EntityUpdate) -> bool {
    // an update is empty when it renames nothing, changes no description and has no metadata
    update.name.is_none()
        && update.description.is_none()
        && !update.clear_description
        && update.metadata.is_none()
}

/// Name an entity a document refers to, falling back to its id when it can't be read
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `id` - The entity's id
async fn name_of(thorium: &Thorium, id: uuid::Uuid) -> String {
    // an unreadable reference stays an id so the document still says what it points at
    match thorium.entities.get(id).await {
        Ok(entity) => entity.name,
        Err(_) => id.to_string(),
    }
}

/// [`Editable`] for model endpoints
pub struct Endpoint;

impl Editable for Endpoint {
    /// Endpoints are edited through the Thorium client
    type Client = Thorium;
    /// The endpoint's id or name
    type Target = String;
    /// The endpoint entity, its metadata, and its credential's name
    type Data = EndpointData;
    /// The endpoint document users edit, which never holds a secret
    type View = EndpointView;
    /// An entity update carrying the endpoint changes
    type Update = EntityUpdate;
    /// Endpoints are called `Model endpoint` in messages
    const KIND: &'static str = "Model endpoint";
    /// The endpoint document's keys
    const FIELD_ORDER: &'static [&'static str] = &[
        "id",
        "name",
        "description",
        "groups",
        "provider",
        "credential",
        "disabled_models",
        "catalog",
    ];

    /// Read the endpoint and the name of its credential
    ///
    /// # Arguments
    ///
    /// * `client` - The Thorium client
    /// * `target` - The endpoint's id or name
    async fn fetch(client: &Thorium, target: &String) -> Result<EndpointData, Error> {
        // find the endpoint by id or name
        let entity = find(client, &[EntityKinds::ModelEndpoint], target).await?;
        let EntityMetadata::ModelEndpoint(meta) = entity.metadata.clone() else {
            return Err(Error::new(format!("{} is not a model endpoint", entity.id)));
        };
        // name the credential the endpoint uses
        let credential = match meta.credential {
            Some(id) => Some(name_of(client, id).await),
            None => None,
        };
        Ok(EndpointData {
            entity,
            meta,
            credential,
        })
    }

    /// Show the endpoint's settings, never a secret
    ///
    /// # Arguments
    ///
    /// * `data` - The endpoint as fetched
    fn to_view(data: &EndpointData) -> EndpointView {
        EndpointView {
            id: data.entity.id.to_string(),
            name: data.entity.name.clone(),
            description: data.entity.description.clone(),
            groups: data.entity.groups.clone(),
            provider: ProviderDoc(data.meta.provider.clone()),
            credential: data.credential.clone(),
            disabled_models: data.meta.disabled_models.iter().cloned().collect(),
            catalog: data.meta.catalog.iter().map(CatalogRow::from).collect(),
        }
    }

    /// The endpoint's id
    ///
    /// # Arguments
    ///
    /// * `data` - The endpoint as fetched
    fn id(data: &EndpointData) -> String {
        data.entity.id.to_string()
    }

    /// The endpoint's name
    ///
    /// # Arguments
    ///
    /// * `data` - The endpoint as fetched
    fn name(data: &EndpointData) -> String {
        data.entity.name.clone()
    }

    /// Refuse an empty name, a provider the API would refuse, and catalog ids that are empty or
    /// repeated
    ///
    /// # Arguments
    ///
    /// * `view` - The edited endpoint view
    /// * `_data` - The endpoint as fetched, unused here
    fn validate(view: &EndpointView, _data: &EndpointData) -> Result<(), Error> {
        // the endpoint needs a name and a provider with its own required settings
        check_name(&view.name)?;
        view.provider.0.validate().map_err(Error::new)?;
        // every catalog model needs one distinct id
        let mut seen = BTreeSet::new();
        for row in &view.catalog {
            if row.model_id.trim().is_empty() {
                return Err(Error::new("a catalog entry's model_id can't be empty"));
            }
            if !seen.insert(row.model_id.as_str()) {
                return Err(Error::new(format!(
                    "catalog model_id `{}` is listed more than once",
                    row.model_id
                )));
            }
        }
        // a disabled model needs an id
        if view.disabled_models.iter().any(|id| id.trim().is_empty()) {
            return Err(Error::new("a disabled model id can't be empty"));
        }
        Ok(())
    }

    /// Diff the document against the endpoint into one update
    ///
    /// # Arguments
    ///
    /// * `client` - The Thorium client
    /// * `data` - The endpoint as fetched
    /// * `view` - The edited endpoint view
    async fn calculate_update(
        client: &Thorium,
        data: &EndpointData,
        view: EndpointView,
    ) -> Result<Option<EntityUpdate>, Error> {
        // diff the document against the endpoint as it is now
        let before = Self::to_view(data);
        let mut update = root_update(&data.entity, &view.name, view.description.as_ref());
        let mut meta = EntityMetadataUpdate::model_endpoint();
        let mut meta_changed = false;
        // a different provider replaces the connection settings
        if view.provider != before.provider {
            meta = meta.endpoint_provider(view.provider.0.clone());
            meta_changed = true;
        }
        // a different credential name is looked up, and none clears the credential
        if view.credential != before.credential {
            meta = match view.credential.as_deref() {
                Some(name) => {
                    let found = find(client, &[EntityKinds::ModelCredential], name).await?;
                    meta.endpoint_credential(found.id)
                }
                None => meta.clear_endpoint_credential(),
            };
            meta_changed = true;
        }
        // models newly listed as disabled are disabled and models no longer listed are enabled
        let old_disabled: BTreeSet<&String> = before.disabled_models.iter().collect();
        let new_disabled: BTreeSet<&String> = view.disabled_models.iter().collect();
        for id in new_disabled.difference(&old_disabled) {
            meta = meta.disable_model(*id);
            meta_changed = true;
        }
        for id in old_disabled.difference(&new_disabled) {
            meta = meta.enable_model(*id);
            meta_changed = true;
        }
        // new and changed rows are added or replaced, and rows left out are removed
        let old_rows: BTreeMap<&str, &CatalogRow> = before
            .catalog
            .iter()
            .map(|row| (row.model_id.as_str(), row))
            .collect();
        let new_ids: BTreeSet<&str> = view
            .catalog
            .iter()
            .map(|row| row.model_id.as_str())
            .collect();
        for row in &view.catalog {
            if old_rows.get(row.model_id.as_str()) != Some(&row) {
                meta = meta.add_catalog_model(row.clone().into());
                meta_changed = true;
            }
        }
        for id in old_rows.keys().filter(|id| !new_ids.contains(*id)) {
            meta = meta.remove_catalog_model(*id);
            meta_changed = true;
        }
        // only send the metadata update when it holds a change
        if meta_changed {
            update = update.metadata(meta);
        }
        // nothing to send means nothing changed
        Ok((!is_empty(&update)).then_some(update))
    }

    /// Send the update to the endpoint by its id
    ///
    /// # Arguments
    ///
    /// * `client` - The Thorium client
    /// * `_target` - The endpoint's id or name, unused since `data` names the endpoint
    /// * `data` - The endpoint as fetched
    /// * `update` - The endpoint update to send
    async fn send_update(
        client: &Thorium,
        _target: &String,
        data: &EndpointData,
        update: &EntityUpdate,
    ) -> Result<(), Error> {
        // the update names the endpoint by its id
        client
            .entities
            .update(data.entity.id, update.clone())
            .await?;
        Ok(())
    }

    /// The standard header plus how the catalog and credential are edited
    ///
    /// # Arguments
    ///
    /// * `data` - The endpoint as fetched
    fn header(data: &EndpointData) -> String {
        // start from the standard header
        let mut header = default_header::<Self>(data);
        // explain how the catalog is edited and why no secret is shown
        header.push_str(
            "# catalog: add or change entries, and leave one out to remove it from the catalog.\n",
        );
        header.push_str(
            "# credential: the name of a group credential. Secrets are never shown or edited here;\n",
        );
        header.push_str("# use `thorctl models credentials rotate` to change one.\n");
        header
    }
}

/// [`Editable`] for model profiles
pub struct Profile;

impl Editable for Profile {
    /// Profiles are edited through the Thorium client
    type Client = Thorium;
    /// The profile's id or name
    type Target = String;
    /// The profile entity, its metadata, and its endpoint's name
    type Data = ProfileData;
    /// The profile document users edit
    type View = ProfileView;
    /// An entity update carrying the profile changes
    type Update = EntityUpdate;
    /// Profiles are called `Model profile` in messages
    const KIND: &'static str = "Model profile";
    /// The profile document's keys
    const FIELD_ORDER: &'static [&'static str] = &[
        "id",
        "name",
        "description",
        "groups",
        "endpoint",
        "model_id",
        "api",
        "parameters",
        "limits",
        "capabilities",
    ];

    /// Read the profile and the name of its endpoint
    ///
    /// # Arguments
    ///
    /// * `client` - The Thorium client
    /// * `target` - The profile's id or name
    async fn fetch(client: &Thorium, target: &String) -> Result<ProfileData, Error> {
        // find the profile by id or name
        let entity = find(client, &[EntityKinds::ModelProfile], target).await?;
        let EntityMetadata::ModelProfile(meta) = entity.metadata.clone() else {
            return Err(Error::new(format!("{} is not a model profile", entity.id)));
        };
        // name the endpoint the profile calls
        let endpoint = name_of(client, meta.endpoint).await;
        Ok(ProfileData {
            entity,
            meta,
            endpoint,
        })
    }

    /// Show the profile's settings
    ///
    /// # Arguments
    ///
    /// * `data` - The profile as fetched
    fn to_view(data: &ProfileData) -> ProfileView {
        ProfileView {
            id: data.entity.id.to_string(),
            name: data.entity.name.clone(),
            description: data.entity.description.clone(),
            groups: data.entity.groups.clone(),
            endpoint: data.endpoint.clone(),
            model_id: data.meta.model_id.clone(),
            api: data.meta.api,
            parameters: ParametersView::from(&data.meta.parameters),
            limits: data.meta.limits.as_ref().map(LimitsView::from),
            capabilities: data.meta.capabilities.as_ref().map(CapabilitiesView::from),
        }
    }

    /// The profile's id
    ///
    /// # Arguments
    ///
    /// * `data` - The profile as fetched
    fn id(data: &ProfileData) -> String {
        data.entity.id.to_string()
    }

    /// The profile's name
    ///
    /// # Arguments
    ///
    /// * `data` - The profile as fetched
    fn name(data: &ProfileData) -> String {
        data.entity.name.clone()
    }

    /// Refuse an empty name, endpoint or model id
    ///
    /// # Arguments
    ///
    /// * `view` - The edited profile view
    /// * `_data` - The profile as fetched, unused here
    fn validate(view: &ProfileView, _data: &ProfileData) -> Result<(), Error> {
        // the profile needs a name, an endpoint and a model
        check_name(&view.name)?;
        if view.endpoint.trim().is_empty() {
            return Err(Error::new("endpoint can't be empty"));
        }
        if view.model_id.trim().is_empty() {
            return Err(Error::new("model_id can't be empty"));
        }
        Ok(())
    }

    /// Diff the document against the profile into one update
    ///
    /// # Arguments
    ///
    /// * `client` - The Thorium client
    /// * `data` - The profile as fetched
    /// * `view` - The edited profile view
    async fn calculate_update(
        client: &Thorium,
        data: &ProfileData,
        view: ProfileView,
    ) -> Result<Option<EntityUpdate>, Error> {
        // diff the document against the profile as it is now
        let before = Self::to_view(data);
        let mut update = root_update(&data.entity, &view.name, view.description.as_ref());
        let mut meta = EntityMetadataUpdate::model_profile();
        let mut meta_changed = false;
        // a different endpoint name is looked up
        if view.endpoint != before.endpoint {
            let found = find(client, &[EntityKinds::ModelEndpoint], &view.endpoint).await?;
            meta = meta.profile_endpoint(found.id);
            meta_changed = true;
        }
        // a different model or protocol replaces the old one
        if view.model_id != before.model_id {
            meta = meta.profile_model(&view.model_id);
            meta_changed = true;
        }
        if view.api != before.api {
            meta = meta.profile_api(view.api);
            meta_changed = true;
        }
        // different parameters replace all of the old ones
        if view.parameters != before.parameters {
            meta = meta.profile_parameters(view.parameters.clone().into());
            meta_changed = true;
        }
        // limits and capabilities are replaced, or cleared to use the catalog entry's again
        if view.limits != before.limits {
            meta = match view.limits.clone() {
                Some(limits) => meta.profile_limits(limits.into()),
                None => meta.clear_profile_limits(),
            };
            meta_changed = true;
        }
        if view.capabilities != before.capabilities {
            meta = match view.capabilities.clone() {
                Some(caps) => meta.profile_capabilities(caps.into()),
                None => meta.clear_profile_capabilities(),
            };
            meta_changed = true;
        }
        // only send the metadata update when it holds a change
        if meta_changed {
            update = update.metadata(meta);
        }
        // nothing to send means nothing changed
        Ok((!is_empty(&update)).then_some(update))
    }

    /// Send the update to the profile by its id
    ///
    /// # Arguments
    ///
    /// * `client` - The Thorium client
    /// * `_target` - The profile's id or name, unused since `data` names the profile
    /// * `data` - The profile as fetched
    /// * `update` - The profile update to send
    async fn send_update(
        client: &Thorium,
        _target: &String,
        data: &ProfileData,
        update: &EntityUpdate,
    ) -> Result<(), Error> {
        // the update names the profile by its id
        client
            .entities
            .update(data.entity.id, update.clone())
            .await?;
        Ok(())
    }
}

/// Print or write the document `edit` shows for a model entity
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The spec command
pub async fn spec<E: Editable<Client = Thorium, Target = String>>(
    thorium: &Thorium,
    cmd: &SpecModel,
) -> Result<(), Error> {
    // read the entity and render its document
    let data = E::fetch(thorium, &cmd.entity).await?;
    let view = E::to_view(&data);
    // render it as JSON when asked to and as YAML otherwise
    let text = match cmd.format.format {
        Some(OutputFormat::Json) => utils::curated_json(&view, E::FIELD_ORDER)?,
        _ => utils::curated_yaml(&view, E::FIELD_ORDER)?,
    };
    // write it where the user asked, or print it
    match &cmd.output {
        Some(path) => {
            std::fs::write(path, &text)
                .map_err(|err| Error::new(format!("Failed to write {}: {err}", path.display())))?;
            println!(
                "Wrote {} {} to {}",
                E::KIND.to_lowercase(),
                E::name(&data),
                path.display()
            );
        }
        None => print!("{text}"),
    }
    Ok(())
}

/// Edit a model entity in an editor, or from the edit flags
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `conf` - The Thorctl config (for the default editor)
/// * `cmd` - The edit command
pub async fn edit<E: Editable<Client = Thorium, Target = String>>(
    thorium: &Thorium,
    conf: &CtlConf,
    cmd: &EditModel,
) -> Result<(), Error> {
    // the flow prints its own result
    edit_with::<E>(thorium, conf, &cmd.entity, &cmd.edit).await?;
    Ok(())
}

/// Update a model entity from `--file`, `--patch`, `--set` and `--unset`
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `entity` - The entity to update, by id or name
/// * `args` - The edit flags
pub async fn update<E: Editable<Client = Thorium, Target = String>>(
    thorium: &Thorium,
    entity: &str,
    args: &crate::args::helpers::EditArgs,
) -> Result<(), Error> {
    // the flow prints its own result
    update_with::<E>(thorium, &entity.to_owned(), args).await?;
    Ok(())
}

#[cfg(test)]
mod tests;
