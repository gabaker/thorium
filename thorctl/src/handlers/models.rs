//! Handles model endpoint, profile, credential, catalog, and ban commands
//!
//! Secrets are read from files or stdin and only ever sent to the API; every view prints
//! credentials by their fingerprints, since the API never returns a secret.

mod documents;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::io::Read;
use std::path::Path;
use thorium::models::{
    AiSettingsUpdate, CredentialKind, Entity, EntityKinds, EntityListOpts, EntityMetadata,
    EntityMetadataRequest, EntityMetadataUpdate, EntityRequest, EntityUpdate, ModelApi, ModelBan,
    ModelBanUpdate, ModelCapabilities, ModelCatalogEntry, ModelCatalogEntryRequest,
    ModelCredentialEntityRequest, ModelEndpointEntityRequest, ModelLimits, ModelParameters,
    ModelProfileEntityRequest, ModelProvider, ModelRefreshState, ModelSecret, UserCredentialRef,
    UserModelEndpoint, UserModelSettings, UserModelSettingsUpdate, UserSettingsUpdate, UserUpdate,
};
use thorium::{Error, Thorium};
use uuid::Uuid;

use crate::args::Args;
use crate::args::helpers::EditArgs;
use crate::args::models::{
    AddBan, AddOwnEndpoint, Bans, Catalogs, ClearBan, CreateArgs, CreateCredential, Credentials,
    DeleteModel, DescribeArgs, DescribeBan, DescribeCatalogModel, Endpoints, GetBans, GetCatalog,
    GetSettings, ListArgs, Models, Profiles, RefreshEndpoint, RemoveOwnCredential,
    RemoveOwnEndpoint, RotateCredential, SetDefaultProfiles, SetOwnCredential, Settings,
    UpdateCredential, UpdateEndpoint, UpdateOwnEndpoint, UpdateProfile,
};
use crate::handlers::agents::confirm_delete;
use crate::utils;
use crate::utils::output::{self, OutputFormat, View};
use crate::utils::render;
use crate::utils::watch;

/// The three model configuration kinds
const MODEL_KINDS: [EntityKinds; 3] = [
    EntityKinds::ModelEndpoint,
    EntityKinds::ModelProfile,
    EntityKinds::ModelCredential,
];

/// Describe a model kind in plain words
///
/// # Arguments
///
/// * `kind` - The kind to describe
fn kind_name(kind: EntityKinds) -> &'static str {
    // name each model kind in plain words
    match kind {
        EntityKinds::ModelEndpoint => "model endpoint",
        EntityKinds::ModelProfile => "model profile",
        EntityKinds::ModelCredential => "model credential",
        _ => "entity",
    }
}

/// Read a YAML or JSON file into a typed value
///
/// # Arguments
///
/// * `path` - The path to read
fn read_doc<T: DeserializeOwned>(path: &Path) -> Result<T, Error> {
    // read the file
    let raw = std::fs::read_to_string(path)
        .map_err(|err| Error::new(format!("Failed to read {}: {err}", path.display())))?;
    // a YAML document may name enum variants with tags
    if let Ok(doc) = serde_norway::from_str::<T>(&raw) {
        return Ok(doc);
    }
    // otherwise read the document, YAML or JSON, with variants as keys
    let value: serde_json::Value = serde_norway::from_str(&raw)
        .map_err(|err| Error::new(format!("Failed to parse {}: {err}", path.display())))?;
    serde_json::from_value(value)
        .map_err(|err| Error::new(format!("{} is not valid: {err}", path.display())))
}

/// Read model parameters from a YAML or JSON file, refusing any key that isn't a known parameter
///
/// # Arguments
///
/// * `path` - The file to read
fn read_parameters(path: &Path) -> Result<ModelParameters, Error> {
    // read the document as a plain value so no key is dropped
    let value: serde_json::Value = read_doc(path)?;
    // refuse any key that isn't a known parameter
    strict_parameters(value, path)
}

/// Convert model parameters read from a file, refusing any key that isn't a known parameter
///
/// # Arguments
///
/// * `value` - The parameters as they were read
/// * `path` - The file they were read from, for the error
fn strict_parameters(value: serde_json::Value, path: &Path) -> Result<ModelParameters, Error> {
    // name the file and the key in the error
    ModelParameters::from_value_strict(value).map_err(|err| {
        Error::new(format!(
            "{} is not valid: {}",
            path.display(),
            err.msg().unwrap_or_default()
        ))
    })
}

/// Read a secret from a file, or from stdin when the path is `-`
///
/// One trailing newline is dropped, so files written by editors and `echo` work.
///
/// # Arguments
///
/// * `path` - The file to read, or `-` for stdin
fn read_secret(path: &Path) -> Result<ModelSecret, Error> {
    // read the whole secret
    let mut raw = String::new();
    if path.as_os_str() == "-" {
        std::io::stdin()
            .read_to_string(&mut raw)
            .map_err(|err| Error::new(format!("Failed to read a secret from stdin: {err}")))?;
    } else {
        raw = std::fs::read_to_string(path)
            .map_err(|err| Error::new(format!("Failed to read {}: {err}", path.display())))?;
    }
    // drop one trailing newline
    if raw.ends_with('\n') {
        raw.pop();
        if raw.ends_with('\r') {
            raw.pop();
        }
    }
    Ok(ModelSecret::from(raw))
}

/// Find a model entity of some kinds by its ID or name
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `kinds` - The kinds the entity may be
/// * `entity` - The entity's ID or name
async fn find(thorium: &Thorium, kinds: &[EntityKinds], entity: &str) -> Result<Entity, Error> {
    // an ID is read directly
    if let Ok(id) = Uuid::parse_str(entity) {
        let found = thorium.entities.get(id).await?;
        if !kinds.contains(&found.kind) {
            return Err(Error::new(format!(
                "{entity} is a {}, not a {}",
                found.kind,
                kinds
                    .iter()
                    .map(|kind| kind_name(*kind))
                    .collect::<Vec<_>>()
                    .join(" or ")
            )));
        }
        return Ok(found);
    }
    // otherwise find the entities of these kinds with this name
    let opts = EntityListOpts::default()
        .kinds(kinds.iter().copied())
        .limit(10_000);
    let mut cursor = thorium.entities.list(&opts).await?;
    let mut matches = Vec::new();
    loop {
        // keep the entities with this exact name
        matches.extend(
            cursor
                .data
                .drain(..)
                .filter(|line| line.name == entity)
                .map(|line| line.id),
        );
        // stop once every page was read
        if cursor.exhausted() {
            break;
        }
        cursor.refill().await?;
    }
    // the name must name exactly one entity
    matches.sort();
    matches.dedup();
    match matches.as_slice() {
        [id] => thorium.entities.get(*id).await,
        [] => Err(Error::new(format!("{entity} not found"))),
        ids => Err(Error::new(format!(
            "Several entities are named {entity}; use one of their IDs: {}",
            ids.iter()
                .map(ToString::to_string)
                .collect::<Vec<String>>()
                .join(", ")
        ))),
    }
}

/// List the details of the model entities of one kind in some groups
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `kind` - The kind to list
/// * `groups` - The groups to list from (all of the user's groups when empty)
/// * `limit` - The most entities to list
async fn list(
    thorium: &Thorium,
    kind: EntityKinds,
    groups: &[String],
    limit: usize,
) -> Result<Vec<Entity>, Error> {
    // list the details of this kind in the requested groups
    let opts = EntityListOpts::default()
        .groups(groups.to_vec())
        .kind(kind)
        .limit(limit);
    let mut cursor = thorium.entities.list_details(&opts).await?;
    // read pages until the limit or the last entity
    let mut entities = std::mem::take(&mut cursor.data);
    while !cursor.exhausted() {
        cursor.refill().await?;
        entities.append(&mut cursor.data);
    }
    Ok(entities)
}

/// Summarize a model entity's kind specific columns for a table
///
/// # Arguments
///
/// * `entity` - The entity to summarize
fn table_row(entity: &Entity) -> Vec<String> {
    // start with the name and id
    let mut row = vec![entity.name.clone(), entity.id.to_string()];
    match &entity.metadata {
        EntityMetadata::ModelEndpoint(meta) => {
            row.push(meta.provider.kind().to_string());
            row.push(
                meta.credential
                    .map_or_else(|| "-".to_owned(), |id| id.to_string()),
            );
            row.push(meta.catalog.len().to_string());
            row.push(meta.bans.len().to_string());
        }
        EntityMetadata::ModelProfile(meta) => {
            row.push(meta.endpoint.to_string());
            row.push(meta.model_id.clone());
            row.push(meta.api.name());
            row.push(meta.bans.len().to_string());
        }
        EntityMetadata::ModelCredential(meta) => {
            row.push(meta.kind.cred_type().to_string());
            row.push(
                meta.secrets
                    .fingerprint
                    .clone()
                    .unwrap_or_else(|| "-".to_owned()),
            );
            row.push(
                meta.expires
                    .map_or_else(|| "-".to_owned(), |time| time.to_rfc3339()),
            );
            row.push(meta.bans.len().to_string());
        }
        _ => row.extend([
            "-".to_owned(),
            "-".to_owned(),
            "-".to_owned(),
            "-".to_owned(),
        ]),
    }
    // end with the groups
    row.push(entity.groups.join(", "));
    row
}

/// Get the table columns for a model kind
///
/// # Arguments
///
/// * `kind` - The kind being listed
fn columns(kind: EntityKinds) -> &'static [&'static str] {
    // each kind shows its own columns
    match kind {
        EntityKinds::ModelEndpoint => &[
            "NAME",
            "ID",
            "PROVIDER",
            "CREDENTIAL",
            "MODELS",
            "BANS",
            "GROUPS",
        ],
        EntityKinds::ModelProfile => &["NAME", "ID", "ENDPOINT", "MODEL", "API", "BANS", "GROUPS"],
        _ => &[
            "NAME",
            "ID",
            "KIND",
            "FINGERPRINT",
            "EXPIRES",
            "BANS",
            "GROUPS",
        ],
    }
}

/// Describe a set of bans as text lines
///
/// # Arguments
///
/// * `bans` - The bans to describe
fn ban_lines<'a, I: Iterator<Item = &'a ModelBan>>(bans: I) -> Vec<String> {
    // one line per ban, oldest first
    let mut bans = bans.collect::<Vec<_>>();
    bans.sort_by_key(|ban| ban.time_banned);
    bans.into_iter()
        .map(|ban| {
            format!(
                "  {} {} ({})",
                ban.id,
                ban.ban_kind,
                ban.time_banned.to_rfc3339()
            )
        })
        .collect()
}

/// Render a model entity's details as text
///
/// # Arguments
///
/// * `entity` - The entity to describe
/// * `ansi` - Whether to style the text
fn describe_text(entity: &Entity, ansi: bool) -> String {
    // start with the entity's name, id, and groups
    let mut lines = vec![render::header(
        &entity.name,
        &entity.groups.join(", "),
        ansi,
    )];
    lines.push(render::field("ID", &entity.id.to_string(), ansi));
    lines.push(render::field("Kind", entity.kind.as_str(), ansi));
    lines.push(render::field("Groups", &entity.groups.join(", "), ansi));
    // add the kind's own details
    match &entity.metadata {
        EntityMetadata::ModelEndpoint(meta) => {
            lines.push(render::field(
                "Provider",
                meta.provider.kind().as_ref(),
                ansi,
            ));
            lines.push(render::field(
                "Credential",
                &meta
                    .credential
                    .map_or_else(|| "none".to_owned(), |id| id.to_string()),
                ansi,
            ));
            let models = meta
                .catalog
                .iter()
                .map(|entry| entry.model_id.clone())
                .collect::<Vec<_>>();
            lines.push(render::field("Catalog", &models.join(", "), ansi));
            let disabled = meta.disabled_models.iter().cloned().collect::<Vec<_>>();
            lines.push(render::field("Disabled models", &disabled.join(", "), ansi));
            lines.push(render::field("Bans", &meta.bans.len().to_string(), ansi));
            lines.extend(ban_lines(meta.bans.values()));
            lines.push(render::field("Modified", &meta.modified.to_rfc3339(), ansi));
        }
        EntityMetadata::ModelProfile(meta) => {
            lines.push(render::field("Endpoint", &meta.endpoint.to_string(), ansi));
            lines.push(render::field("Model", &meta.model_id, ansi));
            lines.push(render::field("API", &meta.api.name(), ansi));
            lines.push(render::field(
                "Parameters",
                &serde_json::to_string(&meta.parameters).unwrap_or_default(),
                ansi,
            ));
            lines.push(render::field("Bans", &meta.bans.len().to_string(), ansi));
            lines.extend(ban_lines(meta.bans.values()));
            lines.push(render::field("Modified", &meta.modified.to_rfc3339(), ansi));
        }
        EntityMetadata::ModelCredential(meta) => {
            lines.push(render::field(
                "Credential kind",
                meta.kind.cred_type().as_ref(),
                ansi,
            ));
            lines.push(render::field(
                "Fingerprint",
                meta.secrets.fingerprint.as_deref().unwrap_or("-"),
                ansi,
            ));
            lines.push(render::field(
                "Expires",
                &meta
                    .expires
                    .map_or_else(|| "-".to_owned(), |time| time.to_rfc3339()),
                ansi,
            ));
            lines.push(render::field(
                "Rotated",
                &meta
                    .rotated
                    .map_or_else(|| "-".to_owned(), |time| time.to_rfc3339()),
                ansi,
            ));
            lines.push(render::field("Bans", &meta.bans.len().to_string(), ansi));
            lines.extend(ban_lines(meta.bans.values()));
        }
        _ => (),
    }
    // add the description if there is one
    lines.push(render::field(
        "Description",
        entity.description.as_deref().unwrap_or("-"),
        ansi,
    ));
    lines.join("\n")
}

/// The model entities one command lists
#[derive(Serialize)]
#[serde(transparent)]
struct EntitiesView {
    /// The entities as the API returned them
    entities: Vec<Entity>,
    /// The kind being listed
    #[serde(skip)]
    kind: EntityKinds,
}

impl View for EntitiesView {
    /// Render the entities as a table
    fn table(&self) -> String {
        // one row per entity
        let rows = self.entities.iter().map(table_row).collect();
        output::table_from(columns(self.kind), rows)
    }

    /// Render the entities as text
    ///
    /// # Arguments
    ///
    /// * `ansi` - Whether to style the text
    fn human(&self, ansi: bool) -> String {
        // describe each entity, separated by rules
        self.entities
            .iter()
            .map(|entity| describe_text(entity, ansi))
            .collect::<Vec<String>>()
            .join(&format!("\n{}\n", render::separator(ansi)))
    }
}

/// One model entity described in full
#[derive(Serialize)]
#[serde(transparent)]
struct EntityView(
    /// The entity as the API returned it
    Entity,
);

impl View for EntityView {
    /// Render the entity as a one row table
    fn table(&self) -> String {
        // reuse the list table for one entity
        output::table_from(columns(self.0.kind), vec![table_row(&self.0)])
    }

    /// Render the entity as text
    ///
    /// # Arguments
    ///
    /// * `ansi` - Whether to style the text
    fn human(&self, ansi: bool) -> String {
        describe_text(&self.0, ansi)
    }
}

/// Summarize a catalog entry's capabilities
///
/// # Arguments
///
/// * `caps` - The capabilities to summarize
fn capabilities_text(caps: &ModelCapabilities) -> String {
    // list the capabilities the model has
    let names = [
        (caps.tool_calling, "tools"),
        (caps.vision, "vision"),
        (caps.pdf_input, "pdf"),
        (caps.structured_output, "structured"),
        (caps.thinking, "thinking"),
        (caps.strict_tools, "strict tools"),
        (caps.parallel_tool_calls, "parallel tool calls"),
        (caps.forced_tool_choice, "forced tool choice"),
    ]
    .into_iter()
    .filter(|(set, _)| *set)
    .map(|(_, name)| name)
    .collect::<Vec<_>>();
    if names.is_empty() {
        "-".to_owned()
    } else {
        names.join(", ")
    }
}

/// Build the table row of a catalog entry
///
/// # Arguments
///
/// * `entry` - The catalog entry
/// * `disabled` - Whether the endpoint disables this model id
fn catalog_row(entry: &ModelCatalogEntry, disabled: bool) -> Vec<String> {
    // show the limits, capabilities, bans, and whether it's disabled
    let opt = |value: Option<u64>| value.map_or_else(|| "-".to_owned(), |value| value.to_string());
    vec![
        entry.model_id.clone(),
        opt(entry.limits.context_window),
        opt(entry.limits.max_output_tokens),
        capabilities_text(&entry.capabilities),
        format!("{:?}", entry.source)
            .split_whitespace()
            .next()
            .unwrap_or("-")
            .to_owned(),
        entry.bans.len().to_string(),
        disabled.to_string(),
    ]
}

/// The columns of a catalog table
const CATALOG_COLUMNS: &[&str] = &[
    "MODEL",
    "CONTEXT",
    "MAX OUTPUT",
    "CAPABILITIES",
    "SOURCE",
    "BANS",
    "DISABLED",
];

/// An endpoint's catalog
#[derive(Serialize)]
#[serde(transparent)]
struct CatalogView {
    /// The catalog entries as the API returned them
    entries: Vec<ModelCatalogEntry>,
    /// The model ids the endpoint disables
    #[serde(skip)]
    disabled: BTreeSet<String>,
}

impl View for CatalogView {
    /// Render the catalog as a table
    fn table(&self) -> String {
        // one row per catalog entry
        let rows = self
            .entries
            .iter()
            .map(|entry| catalog_row(entry, self.disabled.contains(&entry.model_id)))
            .collect();
        output::table_from(CATALOG_COLUMNS, rows)
    }

    /// Render the catalog as text
    ///
    /// # Arguments
    ///
    /// * `_ansi` - Whether to style the text (catalog lists are never styled)
    fn human(&self, _ansi: bool) -> String {
        // one line per model
        self.entries
            .iter()
            .map(|entry| {
                let disabled = if self.disabled.contains(&entry.model_id) {
                    " (disabled)"
                } else {
                    ""
                };
                format!("{}{disabled}", entry.model_id)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// One catalog model described in full
#[derive(Serialize)]
struct CatalogModelView {
    /// The catalog entry as the API returned it
    #[serde(flatten)]
    entry: ModelCatalogEntry,
    /// Whether the endpoint disables this model id
    disabled: bool,
}

impl View for CatalogModelView {
    /// Render the model as a one row table
    fn table(&self) -> String {
        output::table_from(
            CATALOG_COLUMNS,
            vec![catalog_row(&self.entry, self.disabled)],
        )
    }

    /// Render the model as text
    ///
    /// # Arguments
    ///
    /// * `ansi` - Whether to style the text
    fn human(&self, ansi: bool) -> String {
        // show every detail of the model
        let opt =
            |value: Option<u64>| value.map_or_else(|| "-".to_owned(), |value| value.to_string());
        let mut lines = vec![
            render::field("Model", &self.entry.model_id, ansi),
            render::field(
                "Context window",
                &opt(self.entry.limits.context_window),
                ansi,
            ),
            render::field(
                "Max output tokens",
                &opt(self.entry.limits.max_output_tokens),
                ansi,
            ),
            render::field(
                "Capabilities",
                &capabilities_text(&self.entry.capabilities),
                ansi,
            ),
            render::field("Disabled", &self.disabled.to_string(), ansi),
            render::field("Bans", &self.entry.bans.len().to_string(), ansi),
        ];
        lines.extend(ban_lines(self.entry.bans.values()));
        lines.join("\n")
    }
}

/// One ban and what it's on
#[derive(Serialize, Clone)]
struct BanRow {
    /// The id of the entity holding the ban
    entity: Uuid,
    /// The name of the entity holding the ban
    entity_name: String,
    /// The kind of the entity holding the ban
    kind: EntityKinds,
    /// The catalog model holding the ban, if it's on a catalog model
    model: Option<String>,
    /// The ban
    ban: ModelBan,
}

/// The bans one command lists
#[derive(Serialize)]
#[serde(transparent)]
struct BansView(
    /// The bans and what they're on
    Vec<BanRow>,
);

impl View for BansView {
    /// Render the bans as a table
    fn table(&self) -> String {
        // one row per ban
        let rows = self
            .0
            .iter()
            .map(|row| {
                vec![
                    row.ban.id.to_string(),
                    row.ban.ban_kind.name().to_owned(),
                    format!("{} ({})", row.entity_name, row.entity),
                    row.model.clone().unwrap_or_else(|| "-".to_owned()),
                    row.ban.time_banned.to_rfc3339(),
                    row.ban.ban_kind.to_string(),
                ]
            })
            .collect();
        output::table_from(&["BAN", "KIND", "ENTITY", "MODEL", "TIME", "DETAIL"], rows)
    }

    /// Render the bans as text
    ///
    /// # Arguments
    ///
    /// * `ansi` - Whether to style the text
    fn human(&self, ansi: bool) -> String {
        // describe each ban, separated by rules
        self.0
            .iter()
            .map(|row| {
                [
                    render::field("Ban", &row.ban.id.to_string(), ansi),
                    render::field("Kind", &row.ban.ban_kind.to_string(), ansi),
                    render::field(
                        "On",
                        &format!(
                            "{} `{}` ({})",
                            kind_name(row.kind),
                            row.entity_name,
                            row.entity
                        ),
                        ansi,
                    ),
                    render::field("Model", row.model.as_deref().unwrap_or("-"), ansi),
                    render::field("Banned", &row.ban.time_banned.to_rfc3339(), ansi),
                ]
                .join("\n")
            })
            .collect::<Vec<_>>()
            .join(&format!("\n{}\n", render::separator(ansi)))
    }
}

/// Collect every ban on a model entity and its catalog models
///
/// # Arguments
///
/// * `entity` - The entity to collect bans from
fn collect_bans(entity: &Entity) -> Vec<BanRow> {
    // build a row for a ban on this entity
    let row = |model: Option<String>, ban: &ModelBan| BanRow {
        entity: entity.id,
        entity_name: entity.name.clone(),
        kind: entity.kind,
        model,
        ban: ban.clone(),
    };
    let mut rows = Vec::new();
    match &entity.metadata {
        EntityMetadata::ModelEndpoint(meta) => {
            // the endpoint's own bans, then each catalog model's
            rows.extend(meta.bans.values().map(|ban| row(None, ban)));
            for entry in &meta.catalog {
                rows.extend(
                    entry
                        .bans
                        .values()
                        .map(|ban| row(Some(entry.model_id.clone()), ban)),
                );
            }
        }
        EntityMetadata::ModelProfile(meta) => {
            rows.extend(meta.bans.values().map(|ban| row(None, ban)));
        }
        EntityMetadata::ModelCredential(meta) => {
            rows.extend(meta.bans.values().map(|ban| row(None, ban)));
        }
        _ => (),
    }
    rows
}

/// List the model entities of one kind
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `kind` - The kind to list
/// * `cmd` - The list arguments
async fn get(thorium: &Thorium, kind: EntityKinds, cmd: &ListArgs) -> Result<(), Error> {
    // list in a table by default
    let format = cmd.format.format.unwrap_or(OutputFormat::Table);
    watch::show(format, &cmd.watch, Some(&cmd.wait), || async {
        Ok(EntitiesView {
            entities: list(thorium, kind, &cmd.group, cmd.limit).await?,
            kind,
        })
    })
    .await
}

/// Describe a model entity of one kind
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `kind` - The kind to describe
/// * `cmd` - The describe arguments
async fn describe(thorium: &Thorium, kind: EntityKinds, cmd: &DescribeArgs) -> Result<(), Error> {
    // describe in text by default
    let format = cmd.format.format.unwrap_or(OutputFormat::Human);
    watch::show(format, &cmd.watch, Some(&cmd.wait), || async {
        Ok(EntityView(find(thorium, &[kind], &cmd.entity).await?))
    })
    .await
}

/// Create a model entity
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `args` - The create arguments
/// * `metadata` - The entity's metadata
async fn create(
    thorium: &Thorium,
    args: &CreateArgs,
    metadata: EntityMetadataRequest,
) -> Result<Uuid, Error> {
    // build the request
    let kind = metadata.kind();
    let mut req = EntityRequest::new(&args.name, metadata, args.group.clone());
    req.description.clone_from(&args.description);
    // create the entity
    let resp = thorium.entities.create(req).await?;
    println!("Created {} {} ({})", kind_name(kind), resp.name, resp.id);
    Ok(resp.id)
}

/// Delete a model entity of one kind
///
/// A dry run only says what would be deleted. Otherwise the person is asked to confirm unless
/// `--skip-confirm` was given, and without a terminal the delete is refused. A delete that
/// Thorium refuses because something still references the entity prints the reason on one line.
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `kind` - The kind to delete
/// * `cmd` - The delete command
async fn delete(thorium: &Thorium, kind: EntityKinds, cmd: &DeleteModel) -> Result<(), Error> {
    // find the entity
    let entity = find(thorium, &[kind], &cmd.entity).await?;
    // a dry run only says what would be deleted
    if cmd.dry_run {
        println!(
            "Dry run: would delete {} {} ({})",
            kind_name(kind),
            entity.name,
            entity.id
        );
        return Ok(());
    }
    // confirm the deletion
    if !confirm_delete(kind_name(kind), &entity.name, cmd.skip_confirm)? {
        println!("Cancelled.");
        return Ok(());
    }
    // delete the entity, which the API refuses while something still references it
    thorium
        .entities
        .delete(entity.id)
        .await
        .map_err(utils::delete_refusal)?;
    println!(
        "Deleted {} {} ({})",
        kind_name(kind),
        entity.name,
        entity.id
    );
    Ok(())
}

/// Refuse the edit flags that only make sense with a document change, and refuse mixing a
/// document change with flags that change one setting each
///
/// # Arguments
///
/// * `edit` - The edit flags
/// * `flags_given` - Whether any flag that changes one setting was given
fn check_update_flags(edit: &EditArgs, flags_given: bool) -> Result<(), Error> {
    // a document change and per-setting flags are two ways of saying the same thing
    if edit.has_changes() && flags_given {
        return Err(Error::new(
            "Use either --file, --patch, --set and --unset, or the flags that change one setting each, not both",
        ));
    }
    // these only act on a document change
    let lone = edit.dry_run || edit.force || edit.skip_confirm || edit.format.is_some();
    if lone && !edit.has_changes() {
        return Err(Error::new(
            "--dry-run, --force, --skip-confirm and --format need --file, --patch, --set or --unset",
        ));
    }
    Ok(())
}

/// Update a model endpoint
///
/// A file, `--set`, or patch changes the endpoint's whole document; otherwise the flags change
/// its fields. The two can't be combined.
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The update command
async fn update_endpoint(thorium: &Thorium, cmd: &UpdateEndpoint) -> Result<(), Error> {
    // change the endpoint's document when asked to
    check_update_flags(&cmd.edit, cmd.has_flag_changes())?;
    if cmd.edit.has_changes() {
        return documents::update::<documents::Endpoint>(thorium, &cmd.endpoint, &cmd.edit).await;
    }
    // find the endpoint
    let endpoint = find(thorium, &[EntityKinds::ModelEndpoint], &cmd.endpoint).await?;
    // build the metadata update
    let mut meta = EntityMetadataUpdate::model_endpoint();
    if let Some(path) = &cmd.provider {
        meta = meta.endpoint_provider(read_doc::<ModelProvider>(path)?);
    }
    if let Some(credential) = cmd.credential {
        meta = meta.endpoint_credential(credential);
    }
    if cmd.clear_credential {
        meta = meta.clear_endpoint_credential();
    }
    if let Some(path) = &cmd.add_models {
        for model in read_doc::<Vec<ModelCatalogEntryRequest>>(path)? {
            meta = meta.add_catalog_model(model);
        }
    }
    for model_id in &cmd.remove_model {
        meta = meta.remove_catalog_model(model_id);
    }
    for model_id in &cmd.disable_model {
        meta = meta.disable_model(model_id);
    }
    for model_id in &cmd.enable_model {
        meta = meta.enable_model(model_id);
    }
    // send the update
    let mut update = EntityUpdate::default().metadata(meta);
    update.description.clone_from(&cmd.description);
    thorium.entities.update(endpoint.id, update).await?;
    println!("Updated model endpoint {} ({})", endpoint.name, endpoint.id);
    Ok(())
}

/// Get an endpoint's catalog and disabled models
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `endpoint` - The endpoint's ID or name
async fn catalog(
    thorium: &Thorium,
    endpoint: &str,
) -> Result<(Vec<ModelCatalogEntry>, BTreeSet<String>), Error> {
    // read the endpoint
    let endpoint = find(thorium, &[EntityKinds::ModelEndpoint], endpoint).await?;
    match endpoint.metadata {
        EntityMetadata::ModelEndpoint(meta) => Ok((meta.catalog, meta.disabled_models)),
        _ => Err(Error::new(format!(
            "{} is not a model endpoint",
            endpoint.id
        ))),
    }
}

/// List an endpoint's catalog
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The catalog command
async fn get_catalog(thorium: &Thorium, cmd: &GetCatalog) -> Result<(), Error> {
    // read the catalog and print it in a table by default
    let (entries, disabled) = catalog(thorium, &cmd.endpoint).await?;
    let format = cmd.format.format.unwrap_or(OutputFormat::Table);
    output::print(&CatalogView { entries, disabled }, format)
}

/// Describe one catalog model
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The describe command
async fn describe_catalog_model(
    thorium: &Thorium,
    cmd: &DescribeCatalogModel,
) -> Result<(), Error> {
    // find the model in the endpoint's catalog
    let (entries, disabled) = catalog(thorium, &cmd.endpoint).await?;
    let Some(entry) = entries
        .into_iter()
        .find(|entry| entry.model_id == cmd.model_id)
    else {
        return Err(Error::new(format!(
            "{} isn't in {}'s catalog",
            cmd.model_id, cmd.endpoint
        )));
    };
    // describe it in text by default
    let disabled = disabled.contains(&entry.model_id);
    let format = cmd.format.format.unwrap_or(OutputFormat::Human);
    output::print(&CatalogModelView { entry, disabled }, format)
}

/// The limits a profile spec sets
#[derive(Deserialize, Debug, Default)]
struct LimitsSpec {
    /// The max input tokens
    context_window: Option<u64>,
    /// The max output tokens
    max_output_tokens: Option<u64>,
}

impl LimitsSpec {
    /// Convert these limits into limits set by hand
    fn into_limits(self) -> ModelLimits {
        ModelLimits {
            context_window: self.context_window,
            max_output_tokens: self.max_output_tokens,
            ..ModelLimits::default()
        }
    }
}

/// A profile spec file
#[derive(Deserialize, Debug)]
struct ProfileSpec {
    /// The endpoint the profile calls
    endpoint: Uuid,
    /// The exact model id
    model_id: String,
    /// The protocol to call the model with
    api: ModelApi,
    /// The parameters applied to every call, read strictly by [`strict_parameters`]
    #[serde(default)]
    parameters: Option<serde_json::Value>,
    /// Limits that replace the catalog entry's
    #[serde(default)]
    limits: Option<LimitsSpec>,
    /// Capabilities that replace the catalog entry's
    #[serde(default)]
    capabilities: Option<ModelCapabilities>,
}

/// Create a model profile from a spec
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The create arguments
async fn create_profile(thorium: &Thorium, cmd: &CreateArgs) -> Result<(), Error> {
    // read the spec and build the request
    let spec: ProfileSpec = read_doc(&cmd.spec)?;
    let mut req = ModelProfileEntityRequest::new(spec.endpoint, spec.model_id, spec.api);
    if let Some(parameters) = spec.parameters {
        // refuse parameters with an unknown key before sending anything
        req = req.parameters(&strict_parameters(parameters, &cmd.spec)?)?;
    }
    if let Some(limits) = spec.limits {
        req = req.limits(limits.context_window, limits.max_output_tokens)?;
    }
    req.capabilities = spec.capabilities;
    create(thorium, cmd, EntityMetadataRequest::ModelProfile(req)).await?;
    Ok(())
}

/// Update a model profile
///
/// A file, `--set`, or patch changes the profile's whole document; otherwise the flags change
/// its fields. The two can't be combined.
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The update command
async fn update_profile(thorium: &Thorium, cmd: &UpdateProfile) -> Result<(), Error> {
    // change the profile's document when asked to
    check_update_flags(&cmd.edit, cmd.has_flag_changes())?;
    if cmd.edit.has_changes() {
        return documents::update::<documents::Profile>(thorium, &cmd.profile, &cmd.edit).await;
    }
    // find the profile
    let profile = find(thorium, &[EntityKinds::ModelProfile], &cmd.profile).await?;
    // build the metadata update
    let mut meta = EntityMetadataUpdate::model_profile();
    if let Some(endpoint) = cmd.endpoint {
        meta = meta.profile_endpoint(endpoint);
    }
    if let Some(model_id) = &cmd.model_id {
        meta = meta.profile_model(model_id);
    }
    if let Some(api) = &cmd.api {
        meta = meta.profile_api(ModelApi::parse_name(api).map_err(Error::new)?);
    }
    if let Some(path) = &cmd.parameters {
        meta = meta.profile_parameters(read_parameters(path)?);
    }
    if let Some(path) = &cmd.limits {
        meta = meta.profile_limits(read_doc::<LimitsSpec>(path)?.into_limits());
    }
    if cmd.clear_limits {
        meta = meta.clear_profile_limits();
    }
    if let Some(path) = &cmd.capabilities {
        meta = meta.profile_capabilities(read_doc(path)?);
    }
    if cmd.clear_capabilities {
        meta = meta.clear_profile_capabilities();
    }
    // send the update
    thorium
        .entities
        .update(profile.id, EntityUpdate::default().metadata(meta))
        .await?;
    println!("Updated model profile {} ({})", profile.name, profile.id);
    Ok(())
}

/// A credential spec file
#[derive(Deserialize, Debug)]
struct CredentialSpec {
    /// What kind of credential this is and its non-secret metadata
    kind: CredentialKind,
    /// When the credential stops working, if known
    #[serde(default)]
    expires: Option<chrono::DateTime<chrono::Utc>>,
}

/// Get the fingerprint of a credential as the API returns it
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `id` - The credential's id
async fn fingerprint(thorium: &Thorium, id: Uuid) -> Result<String, Error> {
    // read the credential back
    let credential = thorium.entities.get(id).await?;
    match credential.metadata {
        EntityMetadata::ModelCredential(meta) => Ok(meta
            .secrets
            .fingerprint
            .unwrap_or_else(|| "none".to_owned())),
        _ => Err(Error::new(format!("{id} is not a model credential"))),
    }
}

/// Create a model credential from a spec and a secret file
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The create command
async fn create_credential(thorium: &Thorium, cmd: &CreateCredential) -> Result<(), Error> {
    // read the spec and the secrets
    let spec: CredentialSpec = read_doc(&cmd.create.spec)?;
    let secret = cmd.secret_file.as_deref().map(read_secret).transpose()?;
    let mut req = ModelCredentialEntityRequest::new(spec.kind, secret);
    req.session_token = cmd
        .session_token_file
        .as_deref()
        .map(read_secret)
        .transpose()?;
    req.expires = spec.expires;
    // create the credential and print only its fingerprint
    let id = create(
        thorium,
        &cmd.create,
        EntityMetadataRequest::ModelCredential(req),
    )
    .await?;
    println!("Fingerprint: {}", fingerprint(thorium, id).await?);
    Ok(())
}

/// Replace a model credential's secret
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The rotate command
async fn rotate_credential(thorium: &Thorium, cmd: &RotateCredential) -> Result<(), Error> {
    // find the credential and read the new secrets
    let credential = find(thorium, &[EntityKinds::ModelCredential], &cmd.credential).await?;
    let mut meta =
        EntityMetadataUpdate::model_credential().credential_secret(read_secret(&cmd.secret_file)?);
    if let Some(path) = &cmd.session_token_file {
        meta = meta.credential_session_token(read_secret(path)?);
    }
    // send the rotation and print only the new fingerprint
    thorium
        .entities
        .update(credential.id, EntityUpdate::default().metadata(meta))
        .await?;
    println!(
        "Rotated model credential {} ({})\nFingerprint: {}",
        credential.name,
        credential.id,
        fingerprint(thorium, credential.id).await?
    );
    Ok(())
}

/// Update a model credential's expiration or session token
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The update command
async fn update_credential(thorium: &Thorium, cmd: &UpdateCredential) -> Result<(), Error> {
    // find the credential
    let credential = find(thorium, &[EntityKinds::ModelCredential], &cmd.credential).await?;
    // build the metadata update
    let mut meta = EntityMetadataUpdate::model_credential();
    if let Some(expires) = cmd.expires {
        meta = meta.credential_expires(expires);
    }
    if cmd.clear_expires {
        meta = meta.clear_credential_expires();
    }
    if cmd.clear_session_token {
        meta = meta.clear_credential_session_token();
    }
    // send the update
    thorium
        .entities
        .update(credential.id, EntityUpdate::default().metadata(meta))
        .await?;
    println!(
        "Updated model credential {} ({})",
        credential.name, credential.id
    );
    Ok(())
}

/// Collect the bans on the model entities in some groups
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `groups` - The groups to look in (all of the user's groups when empty)
async fn all_bans(thorium: &Thorium, groups: &[String]) -> Result<Vec<BanRow>, Error> {
    // collect the bans of every model kind
    let mut rows = Vec::new();
    for kind in MODEL_KINDS {
        for entity in list(thorium, kind, groups, 10_000).await? {
            rows.extend(collect_bans(&entity));
        }
    }
    Ok(rows)
}

/// List bans
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The get command
async fn get_bans(thorium: &Thorium, cmd: &GetBans) -> Result<(), Error> {
    // collect one endpoint's bans or every model entity's
    let rows = match &cmd.endpoint {
        Some(endpoint) => {
            collect_bans(&find(thorium, &[EntityKinds::ModelEndpoint], endpoint).await?)
        }
        None => all_bans(thorium, &cmd.group).await?,
    };
    // print them in a table by default
    let format = cmd.format.format.unwrap_or(OutputFormat::Table);
    output::print(&BansView(rows), format)
}

/// Describe a ban
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The describe command
async fn describe_ban(thorium: &Thorium, cmd: &DescribeBan) -> Result<(), Error> {
    // find the ban among the model entities
    let rows = all_bans(thorium, &cmd.group)
        .await?
        .into_iter()
        .filter(|row| row.ban.id == cmd.ban)
        .collect::<Vec<_>>();
    if rows.is_empty() {
        return Err(Error::new(format!("Ban {} not found", cmd.ban)));
    }
    // describe it in text by default
    let format = cmd.format.format.unwrap_or(OutputFormat::Human);
    output::print(&BansView(rows), format)
}

/// Build an update that changes one model entity's bans
///
/// # Arguments
///
/// * `entity` - The entity whose bans change
/// * `model` - The catalog model whose bans change instead, if any
/// * `update` - The ban changes
fn ban_update(
    entity: &Entity,
    model: Option<&str>,
    update: ModelBanUpdate,
) -> Result<EntityUpdate, Error> {
    // pick the kind's update
    let meta = match (entity.kind, model) {
        (EntityKinds::ModelEndpoint, Some(model)) => {
            EntityMetadataUpdate::model_endpoint().catalog_bans(model, update)
        }
        (_, Some(_)) => {
            return Err(Error::new(
                "--model only applies to a model endpoint's catalog".to_owned(),
            ));
        }
        (EntityKinds::ModelEndpoint, None) => {
            EntityMetadataUpdate::model_endpoint().model_bans_update(update)
        }
        (EntityKinds::ModelProfile, None) => {
            EntityMetadataUpdate::model_profile().model_bans_update(update)
        }
        (_, None) => EntityMetadataUpdate::model_credential().model_bans_update(update),
    };
    Ok(EntityUpdate::default().metadata(meta))
}

/// Ban a model entity or catalog model by hand
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The add command
async fn add_ban(thorium: &Thorium, cmd: &AddBan) -> Result<(), Error> {
    // find the entity and build a generic ban
    let entity = find(thorium, &MODEL_KINDS, &cmd.entity).await?;
    let ban = ModelBan::generic(&cmd.message);
    let id = ban.id;
    // send the ban
    let update = ban_update(
        &entity,
        cmd.model.as_deref(),
        ModelBanUpdate::default().add_ban(ban),
    )?;
    thorium.entities.update(entity.id, update).await?;
    println!("Added ban {id} to {} ({})", entity.name, entity.id);
    Ok(())
}

/// Clear a ban from a model entity or one of its catalog models
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The clear command
async fn clear_ban(thorium: &Thorium, cmd: &ClearBan) -> Result<(), Error> {
    // find the entity and where the ban is
    let entity = find(thorium, &MODEL_KINDS, &cmd.entity).await?;
    let Some(row) = collect_bans(&entity)
        .into_iter()
        .find(|row| row.ban.id == cmd.ban)
    else {
        return Err(Error::new(format!(
            "{} has no ban {}",
            entity.name, cmd.ban
        )));
    };
    // remove the ban from wherever it is
    let update = ban_update(
        &entity,
        row.model.as_deref(),
        ModelBanUpdate::default().remove_ban(cmd.ban),
    )?;
    thorium.entities.update(entity.id, update).await?;
    println!(
        "Cleared ban {} from {} ({})",
        cmd.ban, entity.name, entity.id
    );
    Ok(())
}

/// Read the caller's own model settings
///
/// # Arguments
///
/// * `thorium` - The Thorium client
async fn own_settings(thorium: &Thorium) -> Result<UserModelSettings, Error> {
    // the settings come back on whoami with their secrets cleared
    let info = thorium.users.info().await?;
    Ok(info.settings.ai.map_or_default(|ai| ai.models))
}

/// Send an update of the caller's own model settings
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `update` - The model settings update
async fn send_settings(thorium: &Thorium, update: UserModelSettingsUpdate) -> Result<(), Error> {
    // wrap the model update in a user settings update
    let ai = AiSettingsUpdate::default().models(update);
    let settings = UserSettingsUpdate::default().ai(ai);
    thorium
        .users
        .update_self(UserUpdate::default().settings(settings))
        .await?;
    Ok(())
}

/// Find an endpoint by ID, by the name of one of the caller's own endpoints, or optionally by
/// the name of a group endpoint
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `settings` - The caller's own model settings
/// * `endpoint` - The endpoint's ID or name
/// * `groups` - Whether group endpoints may be named too
async fn resolve_endpoint(
    thorium: &Thorium,
    settings: &UserModelSettings,
    endpoint: &str,
    groups: bool,
) -> Result<Uuid, Error> {
    // an ID is used as is, and the API checks it
    if let Ok(id) = Uuid::parse_str(endpoint) {
        return Ok(id);
    }
    // the caller's own endpoints are found by name
    let own = settings
        .endpoints
        .iter()
        .filter(|(_, own)| own.name == endpoint)
        .map(|(id, _)| *id)
        .collect::<Vec<Uuid>>();
    match own.as_slice() {
        [id] => return Ok(*id),
        [] => (),
        _ => {
            return Err(Error::new(format!(
                "Several of your endpoints are named {endpoint}; use its ID"
            )));
        }
    }
    // otherwise find a group endpoint with that name
    if groups {
        return Ok(find(thorium, &[EntityKinds::ModelEndpoint], endpoint)
            .await?
            .id);
    }
    Err(Error::new(format!("You have no endpoint named {endpoint}")))
}

/// A spec for one of a user's own endpoints
#[derive(Deserialize, Debug)]
struct OwnEndpointSpec {
    /// The name of the endpoint
    name: String,
    /// The provider and how to connect to it
    provider: ModelProvider,
    /// The models the endpoint serves
    #[serde(default)]
    models: Vec<ModelCatalogEntryRequest>,
    /// Model ids the endpoint never resolves to
    #[serde(default)]
    disabled_models: BTreeSet<String>,
}

/// A user's own model settings
#[derive(Serialize)]
#[serde(transparent)]
struct SettingsView(
    /// The settings as the API returned them
    UserModelSettings,
);

impl View for SettingsView {
    /// Render the endpoints and credentials as a table
    fn table(&self) -> String {
        // one row per endpoint that is the user's own or has a credential
        let ids = self
            .0
            .endpoints
            .keys()
            .chain(self.0.credentials.keys())
            .copied()
            .collect::<BTreeSet<Uuid>>();
        let rows = ids
            .into_iter()
            .map(|id| {
                let own = self.0.endpoints.get(&id);
                let credential = self.0.credentials.get(&id);
                vec![
                    own.map_or_else(|| "(group)".to_owned(), |own| own.name.clone()),
                    id.to_string(),
                    own.map_or_else(|| "-".to_owned(), |own| own.provider.kind().to_string()),
                    own.map_or_else(|| "-".to_owned(), |own| own.catalog.len().to_string()),
                    credential
                        .and_then(|cred| cred.secrets.fingerprint.clone())
                        .unwrap_or_else(|| "-".to_owned()),
                    (own.map_or(0, |own| own.bans.len())
                        + credential.map_or(0, |cred| cred.bans.len()))
                    .to_string(),
                ]
            })
            .collect();
        output::table_from(
            &[
                "ENDPOINT",
                "ID",
                "PROVIDER",
                "MODELS",
                "FINGERPRINT",
                "BANS",
            ],
            rows,
        )
    }

    /// Render the settings as text
    ///
    /// # Arguments
    ///
    /// * `ansi` - Whether to style the text
    fn human(&self, ansi: bool) -> String {
        let mut lines = Vec::new();
        // each of the user's own endpoints
        for (id, own) in &self.0.endpoints {
            lines.push(render::header(&own.name, &id.to_string(), ansi));
            lines.push(render::field(
                "Provider",
                own.provider.kind().as_ref(),
                ansi,
            ));
            let models = own
                .catalog
                .iter()
                .map(|entry| entry.model_id.clone())
                .collect::<Vec<String>>()
                .join(", ");
            lines.push(render::field("Models", &models, ansi));
            let disabled = own
                .disabled_models
                .iter()
                .cloned()
                .collect::<Vec<String>>()
                .join(", ");
            lines.push(render::field("Disabled models", &disabled, ansi));
            if !own.bans.is_empty() {
                lines.push(render::field("Bans", "", ansi));
                lines.extend(ban_lines(own.bans.values()));
            }
        }
        // each credential, by fingerprint only
        for (endpoint, cred) in &self.0.credentials {
            let fingerprint = cred
                .secrets
                .fingerprint
                .clone()
                .unwrap_or_else(|| "none".to_owned());
            lines.push(render::field(
                &format!("Credential for {endpoint}"),
                &format!("{} ({fingerprint})", cred.kind.cred_type()),
                ansi,
            ));
            if !cred.bans.is_empty() {
                lines.extend(ban_lines(cred.bans.values()));
            }
        }
        // the default profiles in order
        let defaults = self
            .0
            .default_profiles
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<String>>()
            .join(", ");
        lines.push(render::field("Default profiles", &defaults, ansi));
        lines.join("\n")
    }
}

/// Show the caller's own model settings
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The get command
async fn get_settings(thorium: &Thorium, cmd: &GetSettings) -> Result<(), Error> {
    // print in text by default
    let format = cmd.format.format.unwrap_or(OutputFormat::Human);
    output::print(&SettingsView(own_settings(thorium).await?), format)
}

/// Add one of the caller's own endpoints
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The add command
async fn add_own_endpoint(thorium: &Thorium, cmd: &AddOwnEndpoint) -> Result<(), Error> {
    // build the endpoint from its spec
    let spec: OwnEndpointSpec = read_doc(&cmd.spec)?;
    let mut endpoint = UserModelEndpoint::new(&spec.name, spec.provider);
    for model in spec.models {
        endpoint = endpoint.model(model);
    }
    endpoint.disabled_models = spec.disabled_models;
    // add it under a new id
    let id = Uuid::new_v4();
    send_settings(
        thorium,
        UserModelSettingsUpdate::default().endpoint(id, endpoint),
    )
    .await?;
    println!("Added model endpoint {} ({id})", spec.name);
    Ok(())
}

/// Disable or enable model ids on one of the caller's own endpoints
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The update command
async fn update_own_endpoint(thorium: &Thorium, cmd: &UpdateOwnEndpoint) -> Result<(), Error> {
    // find the endpoint
    let settings = own_settings(thorium).await?;
    let id = resolve_endpoint(thorium, &settings, &cmd.endpoint, false).await?;
    // disable and enable the requested ids
    let mut update = UserModelSettingsUpdate::default();
    for model in &cmd.disable_model {
        update = update.disable_model(id, model);
    }
    for model in &cmd.enable_model {
        update = update.enable_model(id, model);
    }
    send_settings(thorium, update).await?;
    println!("Updated model endpoint {id}");
    Ok(())
}

/// Remove one of the caller's own endpoints and their credential for it
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The remove command
async fn remove_own_endpoint(thorium: &Thorium, cmd: &RemoveOwnEndpoint) -> Result<(), Error> {
    // find and remove the endpoint
    let settings = own_settings(thorium).await?;
    let id = resolve_endpoint(thorium, &settings, &cmd.endpoint, false).await?;
    send_settings(
        thorium,
        UserModelSettingsUpdate::default().remove_endpoint(id),
    )
    .await?;
    println!("Removed model endpoint {id}");
    Ok(())
}

/// Set or rotate the caller's own credential for an endpoint
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The set command
async fn set_own_credential(thorium: &Thorium, cmd: &SetOwnCredential) -> Result<(), Error> {
    // find the endpoint and read the spec and secrets
    let settings = own_settings(thorium).await?;
    let id = resolve_endpoint(thorium, &settings, &cmd.endpoint, true).await?;
    let spec: CredentialSpec = read_doc(&cmd.spec)?;
    let secret = cmd.secret_file.as_deref().map(read_secret).transpose()?;
    let mut credential = UserCredentialRef::new(spec.kind, secret);
    credential.expires = spec.expires;
    if let Some(path) = &cmd.session_token_file {
        credential = credential.session_token(read_secret(path)?);
    }
    // send it and print only its fingerprint
    send_settings(
        thorium,
        UserModelSettingsUpdate::default().credential(id, credential),
    )
    .await?;
    let fingerprint = own_settings(thorium)
        .await?
        .credentials
        .get(&id)
        .and_then(|cred| cred.secrets.fingerprint.clone())
        .unwrap_or_else(|| "none".to_owned());
    println!("Set your credential for model endpoint {id}");
    println!("Fingerprint: {fingerprint}");
    Ok(())
}

/// Remove the caller's own credential for an endpoint
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The remove command
async fn remove_own_credential(thorium: &Thorium, cmd: &RemoveOwnCredential) -> Result<(), Error> {
    // find the endpoint and remove the credential for it
    let settings = own_settings(thorium).await?;
    let id = resolve_endpoint(thorium, &settings, &cmd.endpoint, true).await?;
    send_settings(
        thorium,
        UserModelSettingsUpdate::default().remove_credential(id),
    )
    .await?;
    println!("Removed your credential for model endpoint {id}");
    Ok(())
}

/// Set the caller's default profiles
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The set command
async fn set_default_profiles(thorium: &Thorium, cmd: &SetDefaultProfiles) -> Result<(), Error> {
    // find each profile in order
    let mut profiles = Vec::with_capacity(cmd.profiles.len());
    for profile in &cmd.profiles {
        profiles.push(
            find(thorium, &[EntityKinds::ModelProfile], profile)
                .await?
                .id,
        );
    }
    // replace the defaults
    let count = profiles.len();
    send_settings(
        thorium,
        UserModelSettingsUpdate::default().default_profiles(profiles),
    )
    .await?;
    println!("Set {count} default model profiles");
    Ok(())
}

/// The state of a model endpoint's most recent refresh
#[derive(Serialize)]
struct RefreshView {
    /// The endpoint's id
    endpoint: Uuid,
    /// The endpoint's name
    name: String,
    /// Where the most recent refresh is
    state: ModelRefreshState,
    /// When the most recent refresh was requested
    requested: Option<chrono::DateTime<chrono::Utc>>,
    /// When the most recent refresh finished
    finished: Option<chrono::DateTime<chrono::Utc>>,
    /// Why the most recent refresh failed
    error: Option<String>,
    /// When the requested refresh finished, once it has
    refreshed: Option<chrono::DateTime<chrono::Utc>>,
}

impl RefreshView {
    /// Build the view of an endpoint's refresh state
    ///
    /// # Arguments
    ///
    /// * `entity` - The endpoint
    fn new(entity: &Entity) -> Result<Self, Error> {
        // only endpoints are refreshed
        let EntityMetadata::ModelEndpoint(meta) = &entity.metadata else {
            return Err(Error::new(format!(
                "{} is not a model endpoint",
                entity.name
            )));
        };
        let refresh = &meta.refresh;
        // the requested refresh is done once a refresh finished after it was requested
        let refreshed = refreshed_at(refresh.requested, refresh.finished);
        Ok(RefreshView {
            endpoint: entity.id,
            name: entity.name.clone(),
            state: refresh.state,
            requested: refresh.requested,
            finished: refresh.finished,
            error: refresh.error.clone(),
            refreshed,
        })
    }
}

/// Get when the requested refresh finished, once a refresh finished after it was requested
///
/// # Arguments
///
/// * `requested` - When the most recent refresh was requested
/// * `finished` - When the most recent refresh finished
fn refreshed_at(
    requested: Option<chrono::DateTime<chrono::Utc>>,
    finished: Option<chrono::DateTime<chrono::Utc>>,
) -> Option<chrono::DateTime<chrono::Utc>> {
    // a refresh that finished before the request is an earlier one
    match (requested, finished) {
        (Some(requested), Some(finished)) if finished >= requested => Some(finished),
        (None, Some(finished)) => Some(finished),
        _ => None,
    }
}

impl View for RefreshView {
    /// Render the refresh state as a one row table
    fn table(&self) -> String {
        // one row with the state and its times
        let time = |time: Option<chrono::DateTime<chrono::Utc>>| {
            time.map_or_default(|time| time.to_rfc3339())
        };
        output::table_from(
            &["ENDPOINT", "STATE", "REQUESTED", "FINISHED", "ERROR"],
            vec![vec![
                self.name.clone(),
                format!("{:?}", self.state),
                time(self.requested),
                time(self.finished),
                self.error.clone().unwrap_or_default(),
            ]],
        )
    }

    /// Render the refresh state as text
    ///
    /// # Arguments
    ///
    /// * `ansi` - Whether to style the text
    fn human(&self, ansi: bool) -> String {
        // one field per line
        let time = |time: Option<chrono::DateTime<chrono::Utc>>| {
            time.map_or_else(|| "-".to_owned(), |time| time.to_rfc3339())
        };
        let mut lines = vec![
            render::field(
                "Endpoint",
                &format!("{} ({})", self.name, self.endpoint),
                ansi,
            ),
            render::field("State", &format!("{:?}", self.state), ansi),
            render::field("Requested", &time(self.requested), ansi),
            render::field("Finished", &time(self.finished), ansi),
        ];
        // a failed refresh shows why
        if let Some(error) = &self.error {
            lines.push(render::field("Error", error, ansi));
        }
        lines.join("\n")
    }
}

/// Ask for a refresh of a model endpoint and print its state
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The refresh arguments
async fn refresh(thorium: &Thorium, cmd: &RefreshEndpoint) -> Result<(), Error> {
    // find the endpoint and queue its refresh
    let entity = find(thorium, &[EntityKinds::ModelEndpoint], &cmd.endpoint).await?;
    thorium.entities.refresh_model_endpoint(entity.id).await?;
    // print its refresh state, waiting for a condition if asked
    let format = cmd.format.format.unwrap_or(OutputFormat::Human);
    watch::show(format, &cmd.watch, Some(&cmd.wait), || async {
        RefreshView::new(&thorium.entities.get(entity.id).await?)
    })
    .await
}

/// Handle all models commands
///
/// # Arguments
///
/// * `args` - The arguments passed to Thorctl
/// * `cmd` - The models command to execute
pub async fn handle(args: &Args, cmd: &Models) -> Result<(), Error> {
    // load our config and instance our client
    let (conf, thorium) = utils::get_client(args).await?;
    // call the right models handler
    let result = match cmd {
        Models::Endpoints(cmd) => match cmd {
            Endpoints::Get(cmd) => get(&thorium, EntityKinds::ModelEndpoint, cmd).await,
            Endpoints::Describe(cmd) => describe(&thorium, EntityKinds::ModelEndpoint, cmd).await,
            Endpoints::Create(cmd) => {
                let req: ModelEndpointEntityRequest = read_doc(&cmd.spec)?;
                create(&thorium, cmd, EntityMetadataRequest::ModelEndpoint(req))
                    .await
                    .map(|_| ())
            }
            Endpoints::Update(cmd) => update_endpoint(&thorium, cmd).await,
            Endpoints::Edit(cmd) => {
                documents::edit::<documents::Endpoint>(&thorium, &conf, cmd).await
            }
            Endpoints::Spec(cmd) => documents::spec::<documents::Endpoint>(&thorium, cmd).await,
            Endpoints::Delete(cmd) => delete(&thorium, EntityKinds::ModelEndpoint, cmd).await,
        },
        Models::Catalogs(cmd) => match cmd {
            Catalogs::Get(cmd) => get_catalog(&thorium, cmd).await,
            Catalogs::Describe(cmd) => describe_catalog_model(&thorium, cmd).await,
        },
        Models::Profiles(cmd) => match cmd {
            Profiles::Get(cmd) => get(&thorium, EntityKinds::ModelProfile, cmd).await,
            Profiles::Describe(cmd) => describe(&thorium, EntityKinds::ModelProfile, cmd).await,
            Profiles::Create(cmd) => create_profile(&thorium, cmd).await,
            Profiles::Update(cmd) => update_profile(&thorium, cmd).await,
            Profiles::Edit(cmd) => {
                documents::edit::<documents::Profile>(&thorium, &conf, cmd).await
            }
            Profiles::Spec(cmd) => documents::spec::<documents::Profile>(&thorium, cmd).await,
            Profiles::Delete(cmd) => delete(&thorium, EntityKinds::ModelProfile, cmd).await,
        },
        Models::Credentials(cmd) => match cmd {
            Credentials::Get(cmd) => get(&thorium, EntityKinds::ModelCredential, cmd).await,
            Credentials::Describe(cmd) => {
                describe(&thorium, EntityKinds::ModelCredential, cmd).await
            }
            Credentials::Create(cmd) => create_credential(&thorium, cmd).await,
            Credentials::Rotate(cmd) => rotate_credential(&thorium, cmd).await,
            Credentials::Update(cmd) => update_credential(&thorium, cmd).await,
            Credentials::Delete(cmd) => delete(&thorium, EntityKinds::ModelCredential, cmd).await,
        },
        Models::Bans(cmd) => match cmd {
            Bans::Get(cmd) => get_bans(&thorium, cmd).await,
            Bans::Describe(cmd) => describe_ban(&thorium, cmd).await,
            Bans::Add(cmd) => add_ban(&thorium, cmd).await,
            Bans::Clear(cmd) => clear_ban(&thorium, cmd).await,
        },
        Models::Settings(cmd) => match cmd {
            Settings::Get(cmd) => get_settings(&thorium, cmd).await,
            Settings::AddEndpoint(cmd) => add_own_endpoint(&thorium, cmd).await,
            Settings::UpdateEndpoint(cmd) => update_own_endpoint(&thorium, cmd).await,
            Settings::RemoveEndpoint(cmd) => remove_own_endpoint(&thorium, cmd).await,
            Settings::SetCredential(cmd) => set_own_credential(&thorium, cmd).await,
            Settings::RemoveCredential(cmd) => remove_own_credential(&thorium, cmd).await,
            Settings::SetDefault(cmd) => set_default_profiles(&thorium, cmd).await,
        },
        Models::Refresh(cmd) => refresh(&thorium, cmd).await,
    };
    // show the API's status code with any refusal
    result.map_err(|err| match err.status() {
        Some(_) => Error::new(err.to_string()),
        None => err,
    })
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{check_update_flags, refreshed_at};
    use crate::args::helpers::EditArgs;

    /// Parse edit flags the way a command line gives them
    ///
    /// # Arguments
    ///
    /// * `args` - The flags to parse
    fn edit_flags(args: &[&str]) -> EditArgs {
        // wrap the flags in a command that flattens them
        /// A command holding only the shared edit flags
        #[derive(Parser)]
        struct Wrapper {
            /// The shared edit flags
            #[clap(flatten)]
            edit: EditArgs,
        }
        let mut full = vec!["thorctl"];
        full.extend_from_slice(args);
        Wrapper::try_parse_from(full).unwrap().edit
    }

    /// A document change and the flags that change one setting each can't be mixed
    #[test]
    fn document_changes_and_flags_cannot_be_mixed() {
        // either one alone is fine
        check_update_flags(&edit_flags(&["--set", "name=a"]), false).unwrap();
        check_update_flags(&edit_flags(&[]), true).unwrap();
        check_update_flags(&edit_flags(&[]), false).unwrap();
        // both together are refused, naming the two ways
        let err = check_update_flags(&edit_flags(&["--patch", "{}"]), true).unwrap_err();
        assert!(err.to_string().contains("not both"), "{err}");
    }

    /// The flags that act on a document change are refused without one
    #[test]
    fn document_only_flags_need_a_document_change() {
        // each lone flag is refused
        for flag in ["--dry-run", "--force", "--skip-confirm", "--yes"] {
            let err = check_update_flags(&edit_flags(&[flag]), false).unwrap_err();
            assert!(err.to_string().contains("--dry-run"), "{flag}: {err}");
        }
        let err = check_update_flags(&edit_flags(&["-o", "json"]), true).unwrap_err();
        assert!(err.to_string().contains("--format"), "{err}");
        // and accepted beside a change
        check_update_flags(&edit_flags(&["--dry-run", "--set", "name=a"]), false).unwrap();
    }

    /// A requested refresh counts as done only once a refresh finished at or after the request
    #[test]
    fn refreshed_after_request() {
        // a refresh that finished before the request is an earlier one
        let requested = chrono::Utc::now();
        let before = requested - chrono::Duration::seconds(5);
        let after = requested + chrono::Duration::seconds(5);
        assert_eq!(refreshed_at(Some(requested), Some(before)), None);
        // one that finished at or after the request is the requested one
        assert_eq!(refreshed_at(Some(requested), Some(after)), Some(after));
        assert_eq!(
            refreshed_at(Some(requested), Some(requested)),
            Some(requested)
        );
        // a refresh never requested by hand counts once it finished, and nothing before
        assert_eq!(refreshed_at(None, Some(before)), Some(before));
        assert_eq!(refreshed_at(Some(requested), None), None);
        assert_eq!(refreshed_at(None, None), None);
    }
}
