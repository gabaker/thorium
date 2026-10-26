//! The trait a resource implements to be edited, and what an edit reports back

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::path::PathBuf;
use thorium::Error;

/// A resource that can be edited as a YAML or JSON view and updated in Thorium
///
/// One implementation serves `edit` (the editor), `update` (`--file`, `--set`, `--unset` and
/// `--patch`) and every other way of changing the view, so they all share validation, diffing,
/// confirmation, the remote-change guard and result output.
///
/// The view names things (agents, tools, groups) rather than carrying ids, must refuse unknown
/// keys (`#[serde(deny_unknown_fields)]`), and must not hold editable values that change on every
/// read, because the remote-change guard compares the editable fields of two reads of it.
/// Read-only fields may change between reads; the guard leaves them out.
pub trait Editable {
    /// What the flow talks to Thorium through (the Thorium client, or a fake in tests)
    type Client;
    /// What picks the resource to edit (an id, a name and group, a tool and version)
    type Target;
    /// The resource as Thorium returns it
    type Data: Clone;
    /// The document users edit: serializable, refusing unknown keys, cloneable
    type View: Serialize + DeserializeOwned + Clone;
    /// The request that applies a change
    type Update;
    /// The human-readable kind used in messages (`Tool`, `Agent`)
    const KIND: &'static str;
    /// The top-level key order of the editor view (see [`crate::utils::curated_yaml`])
    const FIELD_ORDER: &'static [&'static str];

    /// Top-level view keys that are shown for context and ignored on save
    ///
    /// Keys written `*like-this*` in the view are always read-only; list any others here by name.
    #[must_use]
    fn static_fields() -> &'static [&'static str] {
        &[]
    }

    /// Fetch the current resource
    ///
    /// # Arguments
    ///
    /// * `client` - What talks to Thorium
    /// * `target` - Which resource to fetch
    async fn fetch(client: &Self::Client, target: &Self::Target) -> Result<Self::Data, Error>;

    /// Build the view users edit from a resource
    ///
    /// # Arguments
    ///
    /// * `data` - The resource as fetched
    fn to_view(data: &Self::Data) -> Self::View;

    /// The resource's id, for results
    ///
    /// # Arguments
    ///
    /// * `data` - The resource as fetched
    fn id(data: &Self::Data) -> String;

    /// The resource's name, for messages
    ///
    /// # Arguments
    ///
    /// * `data` - The resource as fetched
    fn name(data: &Self::Data) -> String;

    /// Check an edited view before anything is sent (names that exist, required fields)
    ///
    /// # Arguments
    ///
    /// * `view` - The edited view
    /// * `data` - The resource as fetched
    fn validate(view: &Self::View, data: &Self::Data) -> Result<(), Error> {
        // nothing to check beyond the view's own types by default
        let _ = (view, data);
        Ok(())
    }

    /// Turn an edited view into the request that applies it
    ///
    /// Returns `None` when the view holds no change. May use the client to resolve names to ids.
    ///
    /// # Arguments
    ///
    /// * `client` - What talks to Thorium
    /// * `data` - The resource as fetched
    /// * `view` - The edited view
    async fn calculate_update(
        client: &Self::Client,
        data: &Self::Data,
        view: Self::View,
    ) -> Result<Option<Self::Update>, Error>;

    /// Send the update to Thorium
    ///
    /// # Arguments
    ///
    /// * `client` - What talks to Thorium
    /// * `target` - Which resource to update
    /// * `data` - The resource as fetched
    /// * `update` - The update to send
    async fn send_update(
        client: &Self::Client,
        target: &Self::Target,
        data: &Self::Data,
        update: &Self::Update,
    ) -> Result<(), Error>;

    /// The comment block at the top of the editor buffer
    ///
    /// Every line starts with `#`, so it is ignored when the buffer is read back.
    ///
    /// # Arguments
    ///
    /// * `data` - The resource as fetched
    #[must_use]
    fn header(data: &Self::Data) -> String {
        default_header::<Self>(data)
    }
}

/// The standard editor header for a resource
///
/// # Arguments
///
/// * `data` - The resource as fetched
#[must_use]
pub fn default_header<E: Editable + ?Sized>(data: &E::Data) -> String {
    // say what is being edited
    let mut header = format!(
        "# Editing {} '{}' (id {})\n",
        E::KIND.to_lowercase(),
        E::name(data),
        E::id(data)
    );
    // say how to apply and how to cancel
    header.push_str(
        "# Save and quit to review the changes. Leave the file unchanged or empty to cancel.\n",
    );
    header.push_str("# Lines starting with '#' are comments.\n");
    // explain the read-only fields if the view has any
    let view = serde_json::to_value(E::to_view(data)).unwrap_or(Value::Null);
    let has_read_only = view.as_object().is_some_and(|map| {
        map.keys()
            .any(|key| super::flow::is_static_key(key, E::static_fields()))
    });
    if has_read_only {
        header.push_str("# Fields written as *field* are read-only: edits to them are ignored.\n");
    }
    header
}

/// What an edit did
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditStatus {
    /// The update was sent and accepted
    Updated,
    /// The edited view held no change
    NoChanges,
    /// The changes were shown and not applied because of `--dry-run`
    DryRun,
    /// The user cancelled, or declined the confirmation
    Cancelled,
}

impl EditStatus {
    /// The status as the lowercase word the JSON result uses
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            EditStatus::Updated => "updated",
            EditStatus::NoChanges => "no_changes",
            EditStatus::DryRun => "dry_run",
            EditStatus::Cancelled => "cancelled",
        }
    }
}

/// One field of a view that an edit changes
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FieldChange {
    /// The dotted path of the field (`config.max_turns`, `groups[1]`)
    pub path: String,
    /// The value before the edit, `null` when the field was absent
    pub old: Value,
    /// The value after the edit, `null` when the field was removed
    pub new: Value,
}

/// The result of an edit, returned to the command that ran it
#[derive(Debug, Clone)]
pub struct EditOutcome {
    /// The kind of resource (`Tool`)
    pub kind: String,
    /// The resource's id
    pub id: String,
    /// The resource's name
    pub name: String,
    /// What the edit did
    pub status: EditStatus,
    /// The fields the edit changes (or would change, for a dry run)
    pub diff: Vec<FieldChange>,
    /// The resource's view after the update, when the result is printed as JSON or YAML
    pub resource: Option<Value>,
    /// Where the edit text was saved when it was abandoned or could not be applied
    pub saved_to: Option<PathBuf>,
}

impl EditOutcome {
    /// Whether the resource was changed in Thorium
    #[must_use]
    pub fn changed(&self) -> bool {
        self.status == EditStatus::Updated
    }

    /// The outcome as the JSON document `--format json` prints
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "kind": self.kind.to_lowercase(),
            "id": self.id,
            "name": self.name,
            "status": self.status.as_str(),
            "changed": self.changed(),
            "dry_run": self.status == EditStatus::DryRun,
            "diff": self.diff,
            "resource": self.resource,
        })
    }
}
