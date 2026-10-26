//! Arguments for model endpoint, profile, credential, catalog, and ban Thorctl commands

use clap::Parser;
use std::path::PathBuf;

use super::helpers::{EditArgs, FormatArgs, ShortFormatArgs, WaitForArgs, WatchArgs};

/// The commands to send to the models task handler
#[derive(Parser, Debug)]
pub enum Models {
    /// Manage model endpoints: the providers a group can call and the models they serve
    #[clap(version, author, subcommand, alias = "endpoint")]
    Endpoints(Endpoints),
    /// Read the models an endpoint's catalog lists
    #[clap(version, author, subcommand, alias = "catalog")]
    Catalogs(Catalogs),
    /// Manage model profiles: one model on one endpoint with a protocol and parameters
    #[clap(version, author, subcommand, alias = "profile")]
    Profiles(Profiles),
    /// Manage group model credentials (secrets are read from a file or stdin, never printed)
    #[clap(version, author, subcommand, alias = "credential")]
    Credentials(Credentials),
    /// Read, add, and clear bans on model endpoints, catalog models, profiles, and credentials
    #[clap(version, author, subcommand, alias = "ban")]
    Bans(Bans),
    /// Manage your own model endpoints, credentials, and default profiles
    #[clap(version, author, subcommand, alias = "setting")]
    Settings(Settings),
    /// Ask the event handler to list a model endpoint's models again (owners and managers)
    #[clap(version, author)]
    Refresh(RefreshEndpoint),
}

/// A request to refresh a model endpoint's catalog
#[derive(Parser, Debug)]
pub struct RefreshEndpoint {
    /// The endpoint to refresh, by ID or name
    pub endpoint: String,
    /// The format to print the refresh's state in (human readable by default)
    #[clap(flatten)]
    pub format: ShortFormatArgs,
    /// Re-read and re-print the refresh's state until interrupted
    #[clap(flatten)]
    pub watch: WatchArgs,
    /// Wait until the refresh's state matches a condition (`refreshed` waits for it to finish)
    #[clap(flatten)]
    pub wait: WaitForArgs,
}

/// The arguments shared by every model list command
#[derive(clap::Args, Debug)]
pub struct ListArgs {
    /// The groups to list from (all of your groups by default)
    #[clap(short, long)]
    pub group: Vec<String>,
    /// The most items to list
    #[clap(short, long, default_value_t = 50)]
    pub limit: usize,
    /// The format to print in (a table by default)
    #[clap(flatten)]
    pub format: ShortFormatArgs,
    /// Re-read and re-print until interrupted
    #[clap(flatten)]
    pub watch: WatchArgs,
    /// Wait until every listed item matches a condition
    #[clap(flatten)]
    pub wait: WaitForArgs,
}

/// The arguments shared by every model describe command
#[derive(Parser, Debug)]
pub struct DescribeArgs {
    /// The entity to describe, by ID or name
    pub entity: String,
    /// The format to describe it in (human readable by default)
    #[clap(flatten)]
    pub format: ShortFormatArgs,
    /// Re-read and re-describe until interrupted
    #[clap(flatten)]
    pub watch: WatchArgs,
    /// Wait until the entity matches a condition
    #[clap(flatten)]
    pub wait: WaitForArgs,
}

/// The arguments shared by every model create command
#[derive(clap::Args, Debug)]
pub struct CreateArgs {
    /// The path to the entity's spec, as YAML or JSON
    #[clap(long)]
    pub spec: PathBuf,
    /// The name of the entity
    #[clap(short, long)]
    pub name: String,
    /// The groups to create the entity in
    #[clap(short, long, required = true)]
    pub group: Vec<String>,
    /// A description of the entity
    #[clap(short, long)]
    pub description: Option<String>,
}

/// A command to edit a model entity as a document
///
/// Without `--file`, `--patch`, `--set` or `--unset` the document opens in your editor; with any
/// of them the change is applied without opening one.
#[derive(Parser, Debug)]
pub struct EditModel {
    /// The entity to edit, by ID or name
    pub entity: String,
    /// How to change the document, and what to do with the change
    #[clap(flatten)]
    pub edit: EditArgs,
}

/// A command to print the document `edit` shows for a model entity
#[derive(Parser, Debug)]
pub struct SpecModel {
    /// The entity to print, by ID or name
    pub entity: String,
    /// Write the document to this file instead of printing it
    #[clap(short, long)]
    pub output: Option<PathBuf>,
    /// The format of the document: yaml by default, or json
    #[clap(flatten)]
    pub format: FormatArgs,
}

/// A command to delete a model entity
#[derive(Parser, Debug)]
pub struct DeleteModel {
    /// The entity to delete, by ID or name
    pub entity: String,
}

/// The model endpoint commands
#[derive(Parser, Debug)]
pub enum Endpoints {
    /// List the model endpoints in your groups
    #[clap(version, author)]
    Get(ListArgs),
    /// Describe a model endpoint: its provider, credential, catalog, disabled models, and bans
    #[clap(version, author)]
    Describe(DescribeArgs),
    /// Create a model endpoint from a YAML or JSON spec (owners and managers only)
    #[clap(version, author)]
    Create(CreateArgs),
    /// Update a model endpoint with flags, or from --file, --patch, --set and --unset (owners and
    /// managers only)
    #[clap(version, author)]
    Update(UpdateEndpoint),
    /// Edit a model endpoint's document in your editor (owners and managers only)
    #[clap(version, author)]
    Edit(EditModel),
    /// Print the document `edit` shows for a model endpoint (credentials appear by name only)
    #[clap(version, author)]
    Spec(SpecModel),
    /// Delete a model endpoint (owners and managers only)
    #[clap(version, author)]
    Delete(DeleteModel),
}

/// A command to update a model endpoint
#[derive(Parser, Debug)]
pub struct UpdateEndpoint {
    /// The endpoint to update, by ID or name
    pub endpoint: String,
    /// The path to a new provider, as YAML or JSON
    #[clap(long)]
    pub provider: Option<PathBuf>,
    /// The group credential to authenticate with
    #[clap(long, conflicts_with = "clear_credential")]
    pub credential: Option<uuid::Uuid>,
    /// Stop using a group credential
    #[clap(long)]
    pub clear_credential: bool,
    /// The path to catalog entries to add or replace, as a YAML or JSON list
    #[clap(long)]
    pub add_models: Option<PathBuf>,
    /// A model id to remove from the catalog
    #[clap(long)]
    pub remove_model: Vec<String>,
    /// A model id this endpoint should never resolve to
    #[clap(long)]
    pub disable_model: Vec<String>,
    /// A disabled model id this endpoint may resolve to again
    #[clap(long)]
    pub enable_model: Vec<String>,
    /// A new description
    #[clap(short, long)]
    pub description: Option<String>,
    /// Change the endpoint's document instead (cannot be combined with the flags above)
    #[clap(flatten)]
    pub edit: EditArgs,
}

impl UpdateEndpoint {
    /// Whether any of the flags that change one setting each was given
    #[must_use]
    pub fn has_flag_changes(&self) -> bool {
        self.provider.is_some()
            || self.credential.is_some()
            || self.clear_credential
            || self.add_models.is_some()
            || !self.remove_model.is_empty()
            || !self.disable_model.is_empty()
            || !self.enable_model.is_empty()
            || self.description.is_some()
    }
}

/// The catalog commands
#[derive(Parser, Debug)]
pub enum Catalogs {
    /// List the models an endpoint's catalog lists
    #[clap(version, author)]
    Get(GetCatalog),
    /// Describe one model in an endpoint's catalog
    #[clap(version, author)]
    Describe(DescribeCatalogModel),
}

/// A command to list an endpoint's catalog
#[derive(Parser, Debug)]
pub struct GetCatalog {
    /// The endpoint whose catalog to list, by ID or name
    pub endpoint: String,
    /// The format to print in (a table by default)
    #[clap(flatten)]
    pub format: ShortFormatArgs,
}

/// A command to describe one catalog model
#[derive(Parser, Debug)]
pub struct DescribeCatalogModel {
    /// The endpoint whose catalog holds the model, by ID or name
    pub endpoint: String,
    /// The exact model id
    pub model_id: String,
    /// The format to describe it in (human readable by default)
    #[clap(flatten)]
    pub format: ShortFormatArgs,
}

/// The model profile commands
#[derive(Parser, Debug)]
pub enum Profiles {
    /// List the model profiles in your groups
    #[clap(version, author)]
    Get(ListArgs),
    /// Describe a model profile
    #[clap(version, author)]
    Describe(DescribeArgs),
    /// Create a model profile from a YAML or JSON spec
    #[clap(version, author)]
    Create(CreateArgs),
    /// Update a model profile with flags, or from --file, --patch, --set and --unset
    #[clap(version, author)]
    Update(UpdateProfile),
    /// Edit a model profile's document in your editor
    #[clap(version, author)]
    Edit(EditModel),
    /// Print the document `edit` shows for a model profile
    #[clap(version, author)]
    Spec(SpecModel),
    /// Delete a model profile
    #[clap(version, author)]
    Delete(DeleteModel),
}

/// A command to update a model profile
#[derive(Parser, Debug)]
pub struct UpdateProfile {
    /// The profile to update, by ID or name
    pub profile: String,
    /// The endpoint to call
    #[clap(long)]
    pub endpoint: Option<uuid::Uuid>,
    /// The exact model id to call
    #[clap(long)]
    pub model_id: Option<String>,
    /// The protocol to call the model with (e.g. `OpenAiChat`)
    #[clap(long)]
    pub api: Option<String>,
    /// The path to new parameters, as YAML or JSON
    #[clap(long)]
    pub parameters: Option<PathBuf>,
    /// The path to limits that replace the catalog entry's, as YAML or JSON
    #[clap(long, conflicts_with = "clear_limits")]
    pub limits: Option<PathBuf>,
    /// Use the catalog entry's limits again
    #[clap(long)]
    pub clear_limits: bool,
    /// The path to capabilities that replace the catalog entry's, as YAML or JSON
    #[clap(long, conflicts_with = "clear_capabilities")]
    pub capabilities: Option<PathBuf>,
    /// Use the catalog entry's capabilities again
    #[clap(long)]
    pub clear_capabilities: bool,
    /// Change the profile's document instead (cannot be combined with the flags above)
    #[clap(flatten)]
    pub edit: EditArgs,
}

impl UpdateProfile {
    /// Whether any of the flags that change one setting each was given
    #[must_use]
    pub fn has_flag_changes(&self) -> bool {
        self.endpoint.is_some()
            || self.model_id.is_some()
            || self.api.is_some()
            || self.parameters.is_some()
            || self.limits.is_some()
            || self.clear_limits
            || self.capabilities.is_some()
            || self.clear_capabilities
    }
}

/// The model credential commands
#[derive(Parser, Debug)]
pub enum Credentials {
    /// List the model credentials in your groups (only fingerprints, never secrets)
    #[clap(version, author)]
    Get(ListArgs),
    /// Describe a model credential (only its fingerprint, never its secret)
    #[clap(version, author)]
    Describe(DescribeArgs),
    /// Create a model credential from a YAML or JSON spec and a secret file (owners and managers
    /// only)
    #[clap(version, author)]
    Create(CreateCredential),
    /// Replace a model credential's secret (owners and managers only)
    #[clap(version, author)]
    Rotate(RotateCredential),
    /// Update a model credential's expiration or session token (owners and managers only)
    #[clap(version, author)]
    Update(UpdateCredential),
    /// Delete a model credential (owners and managers only)
    #[clap(version, author)]
    Delete(DeleteModel),
}

/// A command to create a model credential
#[derive(Parser, Debug)]
pub struct CreateCredential {
    /// The spec, name, groups, and description of the credential
    #[clap(flatten)]
    pub create: CreateArgs,
    /// The file to read the secret from (`-` reads stdin); not needed for `AwsAssumeRole`
    #[clap(long)]
    pub secret_file: Option<PathBuf>,
    /// The file to read an AWS session token from (`-` reads stdin)
    #[clap(long)]
    pub session_token_file: Option<PathBuf>,
}

/// A command to rotate a model credential's secret
#[derive(Parser, Debug)]
pub struct RotateCredential {
    /// The credential to rotate, by ID or name
    pub credential: String,
    /// The file to read the new secret from (`-` reads stdin)
    #[clap(long)]
    pub secret_file: PathBuf,
    /// The file to read a new AWS session token from (`-` reads stdin)
    #[clap(long)]
    pub session_token_file: Option<PathBuf>,
}

/// A command to update a model credential
#[derive(Parser, Debug)]
pub struct UpdateCredential {
    /// The credential to update, by ID or name
    pub credential: String,
    /// When the credential stops working (RFC 3339)
    #[clap(long, conflicts_with = "clear_expires")]
    pub expires: Option<chrono::DateTime<chrono::Utc>>,
    /// Forget when the credential stops working
    #[clap(long)]
    pub clear_expires: bool,
    /// Remove the AWS session token
    #[clap(long)]
    pub clear_session_token: bool,
}

/// The ban commands
#[derive(Parser, Debug)]
pub enum Bans {
    /// List the bans on the model entities in your groups
    #[clap(version, author)]
    Get(GetBans),
    /// Describe one ban
    #[clap(version, author)]
    Describe(DescribeBan),
    /// Ban a model entity or one of an endpoint's catalog models by hand
    #[clap(version, author)]
    Add(AddBan),
    /// Clear a ban from a model entity or one of an endpoint's catalog models
    #[clap(version, author)]
    Clear(ClearBan),
}

/// A command to list bans
#[derive(Parser, Debug)]
pub struct GetBans {
    /// Only list the bans of this endpoint and its catalog models, by ID or name
    #[clap(long)]
    pub endpoint: Option<String>,
    /// The groups to list from (all of your groups by default)
    #[clap(short, long)]
    pub group: Vec<String>,
    /// The format to print in (a table by default)
    #[clap(flatten)]
    pub format: ShortFormatArgs,
}

/// A command to describe a ban
#[derive(Parser, Debug)]
pub struct DescribeBan {
    /// The ID of the ban
    pub ban: uuid::Uuid,
    /// The groups to look in (all of your groups by default)
    #[clap(short, long)]
    pub group: Vec<String>,
    /// The format to describe it in (human readable by default)
    #[clap(flatten)]
    pub format: ShortFormatArgs,
}

/// A command to add a ban
#[derive(Parser, Debug)]
pub struct AddBan {
    /// The endpoint, profile, or credential to ban, by ID or name
    pub entity: String,
    /// Ban this catalog model of the endpoint instead of the endpoint itself
    #[clap(long)]
    pub model: Option<String>,
    /// Why the item is banned
    #[clap(short, long)]
    pub message: String,
}

/// A command to clear a ban
#[derive(Parser, Debug)]
pub struct ClearBan {
    /// The endpoint, profile, or credential holding the ban, by ID or name
    pub entity: String,
    /// The ID of the ban to clear
    pub ban: uuid::Uuid,
}

/// The commands for a user's own model settings
#[derive(Parser, Debug)]
pub enum Settings {
    /// Show your own model endpoints, credentials (only fingerprints), and default profiles
    #[clap(version, author)]
    Get(GetSettings),
    /// Add one of your own model endpoints from a YAML or JSON spec
    #[clap(version, author)]
    AddEndpoint(AddOwnEndpoint),
    /// Disable or enable model ids on one of your own endpoints
    #[clap(version, author)]
    UpdateEndpoint(UpdateOwnEndpoint),
    /// Remove one of your own endpoints and your credential for it
    #[clap(version, author)]
    RemoveEndpoint(RemoveOwnEndpoint),
    /// Set or rotate your own credential for an endpoint from a spec and a secret file
    #[clap(version, author)]
    SetCredential(SetOwnCredential),
    /// Remove your own credential for an endpoint
    #[clap(version, author)]
    RemoveCredential(RemoveOwnCredential),
    /// Set the model profiles to try in order when an agent accepts any model (none clears them)
    #[clap(version, author)]
    SetDefault(SetDefaultProfiles),
}

/// A command to show a user's own model settings
#[derive(Parser, Debug)]
pub struct GetSettings {
    /// The format to print in (human readable by default)
    #[clap(flatten)]
    pub format: ShortFormatArgs,
}

/// A command to add one of a user's own endpoints
#[derive(Parser, Debug)]
pub struct AddOwnEndpoint {
    /// The path to the endpoint's spec (its name, provider, and catalog), as YAML or JSON
    #[clap(long)]
    pub spec: PathBuf,
}

/// A command to disable or enable model ids on one of a user's own endpoints
#[derive(Parser, Debug)]
pub struct UpdateOwnEndpoint {
    /// Your endpoint, by ID or name
    pub endpoint: String,
    /// A model id this endpoint should never resolve to
    #[clap(long)]
    pub disable_model: Vec<String>,
    /// A disabled model id this endpoint may resolve to again
    #[clap(long)]
    pub enable_model: Vec<String>,
}

/// A command to remove one of a user's own endpoints
#[derive(Parser, Debug)]
pub struct RemoveOwnEndpoint {
    /// Your endpoint, by ID or name
    pub endpoint: String,
}

/// A command to set a user's own credential for an endpoint
#[derive(Parser, Debug)]
pub struct SetOwnCredential {
    /// The endpoint the credential authenticates with: one of yours or a group endpoint, by ID
    /// or name
    pub endpoint: String,
    /// The path to the credential's spec (its kind and expiration), as YAML or JSON
    #[clap(long)]
    pub spec: PathBuf,
    /// The file to read the secret from (`-` reads stdin); without it the stored secret is kept
    #[clap(long)]
    pub secret_file: Option<PathBuf>,
    /// The file to read an AWS session token from (`-` reads stdin)
    #[clap(long)]
    pub session_token_file: Option<PathBuf>,
}

/// A command to remove a user's own credential for an endpoint
#[derive(Parser, Debug)]
pub struct RemoveOwnCredential {
    /// The endpoint the credential authenticates with, by ID or name
    pub endpoint: String,
}

/// A command to set a user's default profiles
#[derive(Parser, Debug)]
pub struct SetDefaultProfiles {
    /// The model profiles to try in order, by ID or name
    pub profiles: Vec<String>,
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Bans, Credentials, Endpoints, Models, Profiles, Settings};
    use crate::args::{Args, SubCommands};
    use crate::utils::output::OutputFormat;

    /// Parse thorctl arguments into a models command
    ///
    /// # Arguments
    ///
    /// * `args` - The arguments after `thorctl models`
    fn parse(args: &[&str]) -> Models {
        // prepend the binary and subcommand names
        let mut full = vec!["thorctl", "models"];
        full.extend_from_slice(args);
        // parse them into a models command
        let SubCommands::Models(models) = Args::try_parse_from(full).unwrap().cmd else {
            panic!("expected a models command");
        };
        models
    }

    /// `endpoints update` takes repeated disable and enable flags
    #[test]
    fn endpoint_update_args() {
        // every update flag parses
        let Models::Endpoints(Endpoints::Update(cmd)) = parse(&[
            "endpoints",
            "update",
            "corn",
            "--disable-model",
            "a",
            "--disable-model",
            "b",
            "--enable-model",
            "c",
            "--remove-model",
            "d",
            "--clear-credential",
        ]) else {
            panic!("expected endpoints update");
        };
        assert_eq!(cmd.endpoint, "corn");
        assert_eq!(cmd.disable_model, vec!["a", "b"]);
        assert_eq!(cmd.enable_model, vec!["c"]);
        assert_eq!(cmd.remove_model, vec!["d"]);
        assert!(cmd.clear_credential);
        // setting and clearing a credential conflict
        let id = "69ff8820-f688-4968-bd6f-45a4cb0f78a9";
        let args = [
            "thorctl",
            "models",
            "endpoints",
            "update",
            "corn",
            "--credential",
            id,
            "--clear-credential",
        ];
        assert!(Args::try_parse_from(args).is_err());
    }

    /// `credentials create` and `rotate` read secrets from files, never arguments
    #[test]
    fn credential_args() {
        // create takes a spec, a name, groups, and a secret file
        let Models::Credentials(Credentials::Create(cmd)) = parse(&[
            "credentials",
            "create",
            "--spec",
            "cred.yaml",
            "-n",
            "key",
            "-g",
            "a",
            "--secret-file",
            "-",
        ]) else {
            panic!("expected credentials create");
        };
        assert_eq!(cmd.create.name, "key");
        assert_eq!(cmd.secret_file.unwrap().to_string_lossy(), "-");
        // rotate requires a secret file
        let Models::Credentials(Credentials::Rotate(cmd)) =
            parse(&["credentials", "rotate", "key", "--secret-file", "s.txt"])
        else {
            panic!("expected credentials rotate");
        };
        assert_eq!(cmd.credential, "key");
        let missing = ["thorctl", "models", "credentials", "rotate", "key"];
        assert!(Args::try_parse_from(missing).is_err());
        // there is no flag that takes a secret value
        let flag = [
            "thorctl",
            "models",
            "credentials",
            "rotate",
            "key",
            "--secret",
            "x",
        ];
        assert!(Args::try_parse_from(flag).is_err());
    }

    /// `bans add` and `clear` take their targets and `profiles get` lists groups
    #[test]
    fn ban_and_profile_args() {
        // add takes an entity, an optional model, and a message
        let Models::Bans(Bans::Add(cmd)) =
            parse(&["bans", "add", "corn", "--model", "m", "--message", "paused"])
        else {
            panic!("expected bans add");
        };
        assert_eq!(cmd.entity, "corn");
        assert_eq!(cmd.model.as_deref(), Some("m"));
        assert_eq!(cmd.message, "paused");
        // clear takes an entity and a ban id
        let id = "69ff8820-f688-4968-bd6f-45a4cb0f78a9";
        let Models::Bans(Bans::Clear(cmd)) = parse(&["bans", "clear", "corn", id]) else {
            panic!("expected bans clear");
        };
        assert_eq!(cmd.ban.to_string(), id);
        // profiles get lists groups
        let Models::Profiles(Profiles::Get(cmd)) = parse(&["profiles", "get", "-g", "a"]) else {
            panic!("expected profiles get");
        };
        assert_eq!(cmd.group, vec!["a"]);
    }

    /// `settings` takes the caller's own endpoint, credential, and default profile changes
    #[test]
    fn settings_args() {
        // update-endpoint takes repeated disable and enable flags
        let Models::Settings(Settings::UpdateEndpoint(cmd)) = parse(&[
            "settings",
            "update-endpoint",
            "mine",
            "--disable-model",
            "a",
            "--disable-model",
            "b",
            "--enable-model",
            "c",
        ]) else {
            panic!("expected settings update-endpoint");
        };
        assert_eq!(cmd.endpoint, "mine");
        assert_eq!(cmd.disable_model, vec!["a", "b"]);
        assert_eq!(cmd.enable_model, vec!["c"]);
        // set-credential reads its secret from a file
        let Models::Settings(Settings::SetCredential(cmd)) = parse(&[
            "settings",
            "set-credential",
            "mine",
            "--spec",
            "cred.yaml",
            "--secret-file",
            "-",
        ]) else {
            panic!("expected settings set-credential");
        };
        assert_eq!(cmd.endpoint, "mine");
        assert_eq!(cmd.secret_file.unwrap().to_string_lossy(), "-");
        // set-default takes any number of profiles, including none
        let Models::Settings(Settings::SetDefault(cmd)) =
            parse(&["settings", "set-default", "p1", "p2"])
        else {
            panic!("expected settings set-default");
        };
        assert_eq!(cmd.profiles, vec!["p1", "p2"]);
        let Models::Settings(Settings::SetDefault(cmd)) = parse(&["settings", "set-default"])
        else {
            panic!("expected settings set-default");
        };
        assert_eq!(cmd.profiles, Vec::<String>::new());
        // the other commands parse their targets
        let Models::Settings(Settings::AddEndpoint(cmd)) =
            parse(&["settings", "add-endpoint", "--spec", "e.yaml"])
        else {
            panic!("expected settings add-endpoint");
        };
        assert_eq!(cmd.spec.to_string_lossy(), "e.yaml");
        let Models::Settings(Settings::RemoveEndpoint(cmd)) =
            parse(&["settings", "remove-endpoint", "mine"])
        else {
            panic!("expected settings remove-endpoint");
        };
        assert_eq!(cmd.endpoint, "mine");
        let Models::Settings(Settings::RemoveCredential(cmd)) =
            parse(&["settings", "remove-credential", "mine"])
        else {
            panic!("expected settings remove-credential");
        };
        assert_eq!(cmd.endpoint, "mine");
        assert!(matches!(
            parse(&["settings", "get"]),
            Models::Settings(Settings::Get(_))
        ));
    }

    /// `endpoints edit` takes the shared edit flags in the order they were typed
    #[test]
    fn endpoint_edit_args() {
        // every shared flag parses
        let Models::Endpoints(Endpoints::Edit(cmd)) = parse(&[
            "endpoint",
            "edit",
            "corn",
            "--editor",
            "vim -n",
            "--set",
            "catalog[0].context_window=5",
            "--unset",
            "credential",
            "--set",
            "name=cob",
            "--patch",
            "{\"description\":null}",
            "--file",
            "-",
            "--dry-run",
            "--yes",
            "--force",
            "-o",
            "json",
        ]) else {
            panic!("expected endpoints edit");
        };
        assert_eq!(cmd.entity, "corn");
        assert_eq!(cmd.edit.editor.as_deref(), Some("vim -n"));
        assert_eq!(cmd.edit.ops.len(), 3);
        assert_eq!(cmd.edit.patch, vec!["{\"description\":null}"]);
        assert_eq!(cmd.edit.file.as_deref(), Some("-"));
        assert!(cmd.edit.dry_run && cmd.edit.skip_confirm && cmd.edit.force);
        assert_eq!(cmd.edit.format, Some(OutputFormat::Json));
        // with no edit flags the command opens an editor
        let Models::Endpoints(Endpoints::Edit(cmd)) = parse(&["endpoints", "edit", "corn"]) else {
            panic!("expected endpoints edit");
        };
        assert!(!cmd.edit.has_changes());
        // an edit needs a target
        let missing = ["thorctl", "models", "endpoints", "edit"];
        assert!(Args::try_parse_from(missing).is_err());
    }

    /// `profiles edit` and both `spec` commands parse their targets and output flags
    #[test]
    fn profile_edit_and_spec_args() {
        // profiles edit shares the edit flags
        let Models::Profiles(Profiles::Edit(cmd)) =
            parse(&["profile", "edit", "fast", "--set", "model_id=m"])
        else {
            panic!("expected profiles edit");
        };
        assert_eq!(cmd.entity, "fast");
        assert!(cmd.edit.has_changes());
        // spec prints to stdout, or to a file, as yaml or json
        let Models::Profiles(Profiles::Spec(cmd)) = parse(&["profiles", "spec", "fast"]) else {
            panic!("expected profiles spec");
        };
        assert_eq!(cmd.entity, "fast");
        assert!(cmd.output.is_none() && cmd.format.format.is_none());
        let Models::Endpoints(Endpoints::Spec(cmd)) = parse(&[
            "endpoints",
            "spec",
            "corn",
            "-o",
            "corn.json",
            "--format",
            "json",
        ]) else {
            panic!("expected endpoints spec");
        };
        assert_eq!(cmd.output.unwrap().to_string_lossy(), "corn.json");
        assert_eq!(cmd.format.format, Some(OutputFormat::Json));
    }

    /// `update` keeps its flags and gains the shared edit flags
    #[test]
    fn update_takes_flags_and_edit_flags() {
        // the flags that change one setting each still parse and are noticed
        let Models::Endpoints(Endpoints::Update(cmd)) =
            parse(&["endpoints", "update", "corn", "--disable-model", "a"])
        else {
            panic!("expected endpoints update");
        };
        assert!(cmd.has_flag_changes());
        assert!(!cmd.edit.has_changes());
        // the edit flags parse beside them and are not counted as one of those flags
        let Models::Endpoints(Endpoints::Update(cmd)) = parse(&[
            "endpoints",
            "update",
            "corn",
            "--set",
            "name=x",
            "--patch",
            "{}",
            "--dry-run",
        ]) else {
            panic!("expected endpoints update");
        };
        assert!(!cmd.has_flag_changes());
        assert!(cmd.edit.has_changes() && cmd.edit.dry_run);
        // the same holds for profiles
        let Models::Profiles(Profiles::Update(cmd)) =
            parse(&["profiles", "update", "fast", "--clear-limits"])
        else {
            panic!("expected profiles update");
        };
        assert!(cmd.has_flag_changes() && !cmd.edit.has_changes());
        let Models::Profiles(Profiles::Update(cmd)) = parse(&[
            "profiles", "update", "fast", "--file", "p.yaml", "-o", "yaml",
        ]) else {
            panic!("expected profiles update");
        };
        assert!(!cmd.has_flag_changes());
        assert_eq!(cmd.edit.file.as_deref(), Some("p.yaml"));
        assert_eq!(cmd.edit.format, Some(OutputFormat::Yaml));
    }
}
