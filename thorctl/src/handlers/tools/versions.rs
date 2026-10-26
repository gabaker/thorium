//! Editing a tool version's spec, which publishes a new version
//!
//! A version is immutable, so saving an edited spec never changes the version that was edited:
//! it publishes the result as a new version of the same tool under a label the user picks with
//! `--version`, bumps with `--bump`, or is asked for.

use serde::{Deserialize, Serialize};
use std::io::IsTerminal;
use thorium::models::{
    Entity, EntityMetadataUpdate, EntityUpdate, ToolEntity, ToolInstanceSummary, ToolKind,
    ToolSpec, ToolVersion,
};
use thorium::{Error, Thorium};
use uuid::Uuid;

use super::spec::SPEC_KEYS;
use super::target::resolve;
use crate::args::tools::Bump;
use crate::handlers::edit::{Editable, default_header};

/// A spec as a document holds it
///
/// A spec is an enum whose variants hold data, which YAML text names with a map key and which the
/// YAML reader only accepts as a tag. Reading the document as a plain value first accepts the map
/// form that `tools spec` and the editor show.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(transparent)]
pub struct SpecDoc(pub ToolSpec);

impl SpecDoc {
    /// Read a spec from a document value, refusing unknown fields and kinds
    ///
    /// # Arguments
    ///
    /// * `value` - The document, with the kind of tool as its one top-level key
    pub fn from_value(value: &serde_json::Value) -> Result<Self, Error> {
        // parse the typed spec the document describes
        ToolSpec::parse_request(&value.to_string())
            .map(SpecDoc)
            .map_err(Error::new)
    }
}

impl<'de> Deserialize<'de> for SpecDoc {
    /// Read a spec from its map form
    ///
    /// # Arguments
    ///
    /// * `deserializer` - The deserializer holding the document
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // read the spec as a plain value, then as the typed spec it describes
        let value = serde_json::Value::deserialize(deserializer)?;
        SpecDoc::from_value(&value).map_err(serde::de::Error::custom)
    }
}

/// What picks the version to edit and how the new version is labeled
#[derive(Debug, Clone, Default)]
pub struct SpecTarget {
    /// The tool, `TOOL@VERSION`, or the id of a tool or version (the latest version by default)
    pub target: String,
    /// The label of the new version
    pub version: Option<String>,
    /// How to bump the highest semantic version into the new label
    pub bump: Option<Bump>,
    /// Whether a person is editing and can be asked for a label
    pub interactive: bool,
}

/// The version being edited, with what the new version's label needs
#[derive(Debug, Clone)]
pub struct SpecData {
    /// The tool entity
    pub tool: Entity,
    /// The tool's own settings, with its versions
    pub meta: ToolEntity,
    /// The id of the version being edited
    pub instance: Uuid,
    /// The label of the version being edited
    pub label: ToolVersion,
    /// The spec of the version being edited
    pub spec: ToolSpec,
    /// The native tools the version provides (carried to the new version)
    pub provides: Vec<Uuid>,
    /// How the new version is labeled
    pub options: SpecTarget,
}

/// The version a spec edit publishes
#[derive(Debug, Clone)]
pub struct SpecUpdate {
    /// The new version's label
    pub label: String,
    /// The JSON encoded spec
    pub spec: String,
    /// The native tools the new version provides
    pub provides: Vec<Uuid>,
}

/// How a new version gets its label
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Label {
    /// The label is decided
    Chosen(String),
    /// Nothing decides it, so a person must be asked
    Ask,
}

/// Find the highest semantic version among a tool's versions
///
/// # Arguments
///
/// * `versions` - The tool's versions
fn highest_semver(versions: &[ToolInstanceSummary]) -> Option<ToolVersion> {
    // only semantic labels compare
    versions
        .iter()
        .filter_map(|summary| match &summary.version {
            ToolVersion::SemVer(version) => Some(version),
            ToolVersion::Custom(_) => None,
        })
        .max()
        .map(|version| ToolVersion::SemVer(version.clone()))
}

/// Bump a semantic version, leaving a custom label as it is
///
/// # Arguments
///
/// * `version` - The version to bump
/// * `bump` - Which part to bump; lower parts reset to zero and pre-release tags are dropped
#[must_use]
pub fn bumped(version: &ToolVersion, bump: Bump) -> ToolVersion {
    // only semantic versions have parts to raise
    let ToolVersion::SemVer(version) = version else {
        return version.clone();
    };
    // raise the chosen part and zero the parts below it, dropping pre-release and build tags
    let (major, minor, patch) = match bump {
        Bump::Patch => (version.major, version.minor, version.patch + 1),
        Bump::Minor => (version.major, version.minor + 1, 0),
        Bump::Major => (version.major + 1, 0, 0),
    };
    ToolVersion::from(format!("{major}.{minor}.{patch}").as_str())
}

/// Decide the label of a tool's next version
///
/// # Arguments
///
/// * `versions` - The tool's versions
/// * `latest` - The tool's latest version label
/// * `explicit` - The label asked for with `--version`
/// * `bump` - The bump asked for with `--bump`
pub fn pick_label(
    versions: &[ToolInstanceSummary],
    latest: Option<&ToolVersion>,
    explicit: Option<&str>,
    bump: Option<Bump>,
) -> Result<Label, Error> {
    // an explicit label is used as given; the API refuses one that is taken
    if let Some(label) = explicit {
        ToolVersion::parse_label(label).map_err(Error::new)?;
        return Ok(Label::Chosen(label.to_owned()));
    }
    // a bump raises the highest semantic version, and defaults to a patch bump on a semantic latest
    let bump = match (bump, latest) {
        (Some(bump), _) => Some(bump),
        (None, Some(ToolVersion::SemVer(_))) => Some(Bump::Patch),
        (None, _) => None,
    };
    match (bump, highest_semver(versions)) {
        (Some(bump), Some(highest)) => Ok(Label::Chosen(bumped(&highest, bump).to_string())),
        (Some(_), None) => Err(Error::new(
            "None of the tool's versions is a semantic version to bump; pass --version",
        )),
        (None, _) => Ok(Label::Ask),
    }
}

/// Settle a label, asking a person when nothing decides it
///
/// # Arguments
///
/// * `label` - What [`pick_label`] decided
/// * `interactive` - Whether a person is editing and can be asked
fn settle_label(label: Label, interactive: bool) -> Result<String, Error> {
    // a decided label is used
    let Label::Chosen(label) = label else {
        // a person at a terminal is asked
        if interactive && std::io::stdin().is_terminal() && std::io::stderr().is_terminal() {
            return dialoguer::Input::<String>::new()
                .with_prompt("Label of the new version")
                .interact_text()
                .map_err(|err| {
                    // Ctrl-C ends the command like an interrupt instead of failing the edit and
                    // offering to reopen the editor
                    if matches!(&err, dialoguer::Error::IO(io) if io.kind() == std::io::ErrorKind::Interrupted) {
                        crate::utils::interrupt::exit_interrupted();
                    }
                    Error::new(format!("Failed to read user input: {err}"))
                });
        }
        // anyone else must say
        return Err(Error::new(
            "The new version's label can't be chosen from a non-semantic latest version; pass --version",
        ));
    };
    Ok(label)
}

/// Build the update that publishes an edited spec as a new version
///
/// Returns `None` when the spec is the one being edited.
///
/// # Arguments
///
/// * `data` - The version as fetched
/// * `view` - The edited spec
pub fn diff(data: &SpecData, view: &SpecDoc) -> Result<Option<SpecUpdate>, Error> {
    // a spec identical to the version's publishes nothing
    if view.0 == data.spec {
        return Ok(None);
    }
    // choose the new version's label
    let label = pick_label(
        &data.meta.instances,
        data.meta.latest_version.as_ref(),
        data.options.version.as_deref(),
        data.options.bump,
    )?;
    let label = settle_label(label, data.options.interactive)?;
    Ok(Some(SpecUpdate {
        label,
        spec: serde_json::to_string(&view.0)?,
        provides: data.provides.clone(),
    }))
}

/// Build the request that publishes a version
///
/// # Arguments
///
/// * `label` - The new version's label
/// * `spec` - The JSON encoded spec
/// * `provides` - The native tools the new version provides
#[must_use]
pub fn publish_update(label: &str, spec: String, provides: Vec<Uuid>) -> EntityUpdate {
    // a version and a spec publish a new version
    EntityUpdate::default().metadata(EntityMetadataUpdate::Tool {
        version: Some(label.to_owned()),
        spec: Some(spec),
        provides,
        harness_env: None,
        clear_harness_env: None,
        disable_models: std::collections::BTreeSet::new(),
        enable_models: std::collections::BTreeSet::new(),
    })
}

/// [`Editable`] for a tool version's spec
pub struct Spec;

impl Editable for Spec {
    /// Tool specs are edited through the Thorium client
    type Client = Thorium;
    /// The tool, the version to start from, and the new version's label options
    type Target = SpecTarget;
    /// The version being edited, its tool, and the edit options
    type Data = SpecData;
    /// The version's spec document
    type View = SpecDoc;
    /// The new version's label, spec, and provided capabilities
    type Update = SpecUpdate;
    /// Specs are called `Tool spec` in messages
    const KIND: &'static str = "Tool spec";
    /// The spec document's keys, in the order specs are written
    const FIELD_ORDER: &'static [&'static str] = SPEC_KEYS;

    /// Read the version being edited
    ///
    /// # Arguments
    ///
    /// * `client` - The Thorium client
    /// * `target` - The tool, version, and label options
    async fn fetch(client: &Thorium, target: &SpecTarget) -> Result<SpecData, Error> {
        // find the tool and the version, which is the latest unless the target names one
        let found = resolve(client, &target.target, true).await?;
        let version = found.instance_meta("edit")?;
        Ok(SpecData {
            instance: found
                .instance
                .as_ref()
                .map_or_else(Uuid::nil, |entity| entity.id),
            label: version.version.clone(),
            spec: version.spec.clone(),
            provides: version.provides.clone(),
            tool: found.tool,
            meta: found.meta,
            options: target.clone(),
        })
    }

    /// Show the version's spec
    ///
    /// # Arguments
    ///
    /// * `data` - The version as fetched
    fn to_view(data: &SpecData) -> SpecDoc {
        SpecDoc(data.spec.clone())
    }

    /// The edited version's id
    ///
    /// # Arguments
    ///
    /// * `data` - The version as fetched
    fn id(data: &SpecData) -> String {
        data.instance.to_string()
    }

    /// The tool's name and the label of the edited version
    ///
    /// # Arguments
    ///
    /// * `data` - The version as fetched
    fn name(data: &SpecData) -> String {
        format!("{}@{}", data.tool.name, data.label)
    }

    /// Refuse a spec of another kind than the tool's
    ///
    /// # Arguments
    ///
    /// * `view` - The edited spec
    /// * `data` - The version as fetched
    fn validate(view: &SpecDoc, data: &SpecData) -> Result<(), Error> {
        // a tool's kind is fixed for the life of the tool
        let kind: ToolKind = view.0.kind();
        if kind != data.meta.kind {
            return Err(Error::new(format!(
                "{} is a {} tool, so its spec must be a {} spec, not a {kind} spec",
                data.tool.name, data.meta.kind, data.meta.kind
            )));
        }
        Ok(())
    }

    /// Build the update that publishes the edited spec
    ///
    /// # Arguments
    ///
    /// * `_client` - The Thorium client, unused here
    /// * `data` - The version as fetched
    /// * `view` - The edited spec
    // the trait's method is async for resources that look names up, and this one doesn't
    #[allow(clippy::unused_async_trait_impl)]
    async fn calculate_update(
        _client: &Thorium,
        data: &SpecData,
        view: SpecDoc,
    ) -> Result<Option<SpecUpdate>, Error> {
        // the update needs nothing from Thorium
        diff(data, &view)
    }

    /// Publish the new version
    ///
    /// # Arguments
    ///
    /// * `client` - The Thorium client
    /// * `_target` - The tool, version, and label options, unused since `data` names the version
    /// * `data` - The version as fetched
    /// * `update` - The new version to publish
    async fn send_update(
        client: &Thorium,
        _target: &SpecTarget,
        data: &SpecData,
        update: &SpecUpdate,
    ) -> Result<(), Error> {
        // publish the version through a tool update
        let request = publish_update(&update.label, update.spec.clone(), update.provides.clone());
        client.entities.update(data.tool.id, request).await?;
        // say which version was published, on stderr so a printed result stays one document
        eprintln!("Published {} version {}", data.tool.name, update.label);
        Ok(())
    }

    /// The standard header plus how the new version is labeled
    ///
    /// # Arguments
    ///
    /// * `data` - The version as fetched
    fn header(data: &SpecData) -> String {
        // start from the standard header
        let mut header = default_header::<Self>(data);
        // explain that saving publishes a version
        header.push_str("# Saving publishes a NEW version of the tool; versions never change.\n");
        header.push_str("# The label comes from --version, or --bump patch|minor|major.\n");
        header.push_str("# Run `thorctl tools spec --help` for how a spec is written.\n");
        header
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use thorium::models::{ToolInstanceSummary, ToolVersion};
    use uuid::Uuid;

    use super::{Label, bumped, pick_label};
    use crate::args::tools::Bump;

    /// Build version summaries from labels, newest first
    ///
    /// # Arguments
    ///
    /// * `labels` - The version labels
    fn versions(labels: &[&str]) -> Vec<ToolInstanceSummary> {
        labels
            .iter()
            .map(|label| ToolInstanceSummary {
                id: Uuid::new_v4(),
                version: ToolVersion::from(*label),
                spec_hash: "hash".to_owned(),
                deprecated: false,
                created: Utc::now(),
            })
            .collect()
    }

    /// A bump raises one part, zeroes the parts below it, and drops pre-release tags
    #[test]
    fn bumps_semantic_versions() {
        // each part resets the ones below it
        let version = ToolVersion::from("1.9.3-rc.1+build");
        assert_eq!(bumped(&version, Bump::Patch).to_string(), "1.9.4");
        assert_eq!(bumped(&version, Bump::Minor).to_string(), "1.10.0");
        assert_eq!(bumped(&version, Bump::Major).to_string(), "2.0.0");
        // a custom label has nothing to bump
        let custom = ToolVersion::from("dev");
        assert_eq!(bumped(&custom, Bump::Patch).to_string(), "dev");
    }

    /// The label comes from `--version`, else `--bump`, else a patch bump of a semantic latest,
    /// else a question for a person
    #[test]
    fn picks_labels() {
        let all = versions(&["1.10.0", "1.9.0", "dev"]);
        let latest = ToolVersion::from("1.10.0");
        // an explicit label is used as given unless it is empty
        let named = pick_label(&all, Some(&latest), Some("2.0.0-beta"), None).unwrap();
        assert_eq!(named, Label::Chosen("2.0.0-beta".into()));
        assert!(pick_label(&all, Some(&latest), Some(""), None).is_err());
        // a semantic latest defaults to a patch bump of the highest semantic version
        let bump = pick_label(&all, Some(&latest), None, None).unwrap();
        assert_eq!(bump, Label::Chosen("1.10.1".into()));
        // --bump chooses the part, ordering 1.10.0 above 1.9.0 numerically
        let minor = pick_label(&all, Some(&latest), None, Some(Bump::Minor)).unwrap();
        assert_eq!(minor, Label::Chosen("1.11.0".into()));
        // a custom latest asks a person unless a bump is given
        let custom = ToolVersion::from("dev");
        assert_eq!(
            pick_label(&all, Some(&custom), None, None).unwrap(),
            Label::Ask
        );
        let major = pick_label(&all, Some(&custom), None, Some(Bump::Major)).unwrap();
        assert_eq!(major, Label::Chosen("2.0.0".into()));
        // a bump needs a semantic version to start from
        let none = versions(&["dev", "stable"]);
        assert!(pick_label(&none, Some(&custom), None, Some(Bump::Patch)).is_err());
    }
}

#[cfg(test)]
mod diff_tests {
    use chrono::Utc;
    use thorium::models::{
        AgenticTool, Entity, EntityKinds, EntityMetadata, ToolEntity, ToolInstanceSummary,
        ToolKind, ToolSpec, ToolVersion,
    };
    use uuid::Uuid;

    use super::{Spec, SpecData, SpecDoc, SpecTarget, diff};
    use crate::args::tools::Bump;
    use crate::handlers::edit::Editable;

    /// Build the data of a tool whose latest version is `latest`
    ///
    /// # Arguments
    ///
    /// * `latest` - The latest version's label
    /// * `options` - How the new version is labeled
    fn data(latest: &str, options: SpecTarget) -> SpecData {
        // a tool with one version
        let mut meta = ToolEntity::new(ToolKind::Agentic, Utc::now());
        meta.latest_version = Some(ToolVersion::from(latest));
        meta.instances = vec![ToolInstanceSummary {
            id: Uuid::new_v4(),
            version: ToolVersion::from(latest),
            spec_hash: "hash".to_owned(),
            deprecated: false,
            created: Utc::now(),
        }];
        let tool = Entity {
            id: Uuid::new_v4(),
            name: "scan".to_owned(),
            kind: EntityKinds::Tool,
            metadata: EntityMetadata::Tool(meta.clone()),
            description: None,
            submitter: "mcarson".to_owned(),
            groups: vec!["corn".to_owned()],
            tags: std::collections::HashMap::new(),
            image: None,
            created: Utc::now(),
        };
        SpecData {
            tool,
            meta,
            instance: Uuid::new_v4(),
            label: ToolVersion::from(latest),
            spec: ToolSpec::Agentic(AgenticTool::default()),
            provides: vec![Uuid::new_v4()],
            options,
        }
    }

    /// A changed spec of the same kind publishes a new version, an unchanged one nothing
    #[test]
    fn publishes_only_changes() {
        // an unchanged spec publishes nothing
        let data = data("1.0.0", SpecTarget::default());
        let same = Spec::to_view(&data);
        assert!(diff(&data, &same).unwrap().is_none());
        // a changed spec publishes a patch bump and carries the provided tools
        let spec = AgenticTool {
            timeout: Some(120),
            ..AgenticTool::default()
        };
        let changed = SpecDoc(ToolSpec::Agentic(spec));
        let update = diff(&data, &changed).unwrap().unwrap();
        assert_eq!(update.label, "1.0.1");
        assert_eq!(update.provides, data.provides);
        assert!(update.spec.contains("\"timeout\":120"), "{}", update.spec);
        // --version and --bump choose the label
        let options = SpecTarget {
            version: Some("3.0.0".to_owned()),
            ..SpecTarget::default()
        };
        let explicit = data_with(&data, options);
        assert_eq!(diff(&explicit, &changed).unwrap().unwrap().label, "3.0.0");
        let options = SpecTarget {
            bump: Some(Bump::Major),
            ..SpecTarget::default()
        };
        let bumped = data_with(&data, options);
        assert_eq!(diff(&bumped, &changed).unwrap().unwrap().label, "2.0.0");
    }

    /// Copy tool data with other label options
    ///
    /// # Arguments
    ///
    /// * `data` - The data to copy
    /// * `options` - The options to give the copy
    fn data_with(data: &SpecData, options: SpecTarget) -> SpecData {
        // only the options differ
        SpecData {
            options,
            ..data.clone()
        }
    }

    /// A custom latest label can't be bumped, so a non-interactive edit must name the label
    #[test]
    fn custom_latest_needs_a_version() {
        let data = data("dev", SpecTarget::default());
        let spec = AgenticTool {
            timeout: Some(5),
            ..AgenticTool::default()
        };
        let err = diff(&data, &SpecDoc(ToolSpec::Agentic(spec))).unwrap_err();
        assert!(err.to_string().contains("--version"), "{err}");
    }

    /// A spec of another kind than the tool's is refused
    #[test]
    fn refuses_another_kind() {
        let data = data("1.0.0", SpecTarget::default());
        let image = SpecDoc(ToolSpec::Image(thorium::models::WorkloadSpec::default()));
        let err = Spec::validate(&image, &data).unwrap_err();
        assert!(err.to_string().contains("Agentic"), "{err}");
        // and a document with an unknown key is not a spec
        let bad = serde_json::json!({"Agentic": {"colour": "red"}});
        assert!(SpecDoc::from_value(&bad).is_err());
    }
}
