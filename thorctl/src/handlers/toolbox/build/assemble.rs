//! Choosing the format of a built toolbox and assembling a current-format toolbox from the legacy
//! and current items a directory holds

use std::future::Future;
use thorium::Error;

use super::BuildOutput;
use super::tool::Items;
use crate::handlers::progress::{self, Bar, BarKind};
use crate::handlers::toolbox::convert::{ConvertOptions, convert_document};
use crate::handlers::toolbox::v2::load::ConvertReport;
use crate::handlers::toolbox::v2::types::ToolboxManifestV2;
use crate::handlers::toolbox::v2::validate::{Severity, validate_manifest};

/// The format of the toolbox `toolbox build` writes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Format {
    /// The legacy `images` and `pipelines` lexicon
    Legacy,
    /// Tools, toolsets, agents, workflows, pipelines with stage tools, and base builds
    Current,
}

/// Choose the format to write
///
/// # Arguments
///
/// * `requested` - The value of `--format-version`, if given
/// * `has_items` - Whether the directory holds manifests only the current format can hold
/// * `rewrite` - Whether `--rewrite-manifests` was given
pub(super) fn pick_format(
    requested: Option<&str>,
    has_items: bool,
    rewrite: bool,
) -> Result<Format, Error> {
    match requested {
        // the legacy format has no place for tools, so refuse rather than drop them
        Some("1") if has_items => Err(Error::new(
            "This directory has tool, build, toolset, agent, workflow, or stage-tool manifests, \
             which only format 2 can hold; drop --format-version 1 or pass --format-version 2"
                .to_owned(),
        )),
        // a rewrite converts to tools, which the legacy format can't hold either
        Some("1") if rewrite => Err(Error::new(
            "--rewrite-manifests writes format 2 and can't be combined with --format-version 1"
                .to_owned(),
        )),
        Some("1") => Ok(Format::Legacy),
        Some(_) => Ok(Format::Current),
        // with no choice, follow the manifests found
        None if has_items || rewrite => Ok(Format::Current),
        None => Ok(Format::Legacy),
    }
}

/// Run a future to completion from synchronous code
///
/// `toolbox build` runs on a blocking thread of the runtime, where the runtime's handle can drive
/// a future; anywhere else (such as a plain test) a small runtime is made for it.
///
/// # Arguments
///
/// * `future` - The future to run
fn block_on<F: Future>(future: F) -> F::Output {
    // drive the future with the surrounding runtime when there is one
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => handle.block_on(future),
        Err(_) => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a current thread runtime")
            .block_on(future),
    }
}

/// Convert the legacy images and pipelines of a walk into a current-format manifest
///
/// # Arguments
///
/// * `legacy` - The legacy part of the walk
fn convert_legacy(legacy: &BuildOutput) -> Result<ToolboxManifestV2, Error> {
    // the conversion reads a document, so write the legacy output as one without the settings
    // that only `export` reads
    let mut document = serde_json::to_value(legacy)
        .map_err(|err| Error::new(format!("Failed to serialize the legacy toolbox: {err}")))?;
    // drop the keys the conversion's loader does not read
    if let Some(object) = document.as_object_mut() {
        for key in ["export_image_path", "export_pipeline_path", "base_image"] {
            object.remove(key);
        }
    }
    // the loader reads bytes, so render the document as JSON
    let bytes = serde_json::to_vec(&document)
        .map_err(|err| Error::new(format!("Failed to serialize the legacy toolbox: {err}")))?;
    // convert it exactly as `toolbox convert` does, without drawing a progress bar
    let bar = Bar::new_or_quiet(
        "toolbox build",
        "Converting legacy manifests...",
        BarKind::UnboundIO,
        true,
    );
    let converted = block_on(convert_document(
        &bytes,
        None,
        &ConvertOptions::default(),
        &bar,
    ))?;
    // a name two sections both define can't be converted
    if !converted.summary.collisions.is_empty() {
        return Err(Error::new(format!(
            "Failed to convert the legacy manifests: {} is defined more than once",
            converted.summary.collisions.join(", ")
        )));
    }
    // say what the conversion left out, as `toolbox convert` does
    for issue in &converted.summary.issues {
        progress::warn(format!(
            "{}: {} ({})",
            issue.item, issue.message, issue.code
        ));
    }
    Ok(converted.manifest)
}

/// Assemble a current-format manifest from the legacy and current items a directory holds
///
/// The legacy images and pipelines are converted exactly as `toolbox convert` converts them, the
/// items are added beside them, and the whole manifest is checked the way `toolbox validate`
/// checks it: errors stop the build, warnings are printed.
///
/// # Arguments
///
/// * `legacy` - The legacy part of the walk (also carries the toolbox's settings)
/// * `items` - The tools, base builds, toolsets, agents, workflows, and pipelines with stage tools
pub(super) fn assemble_current(
    legacy: &BuildOutput,
    items: Items,
) -> Result<ToolboxManifestV2, Error> {
    // convert the legacy part, or start from the toolbox's settings when there is none
    let mut manifest = if legacy.images.is_empty() && legacy.pipelines.is_empty() {
        let mut manifest = ToolboxManifestV2::new(legacy.name.clone());
        manifest.registry.clone_from(&legacy.registry);
        manifest.registries.clone_from(&legacy.registries);
        manifest
            .image_path_prefix
            .clone_from(&legacy.image_path_prefix);
        manifest.bundled_images = legacy.bundled_images;
        manifest
    } else {
        convert_legacy(legacy)?
    };
    // a name the legacy part and the items both define can't be one thing
    let mut clashes: Vec<String> = Vec::new();
    clashes.extend(
        items
            .tools
            .keys()
            .filter(|name| manifest.tools.contains_key(*name))
            .map(|name| format!("tool '{name}' (an image manifest and a tool manifest)")),
    );
    clashes.extend(
        items
            .pipelines
            .keys()
            .filter(|name| manifest.pipelines.contains_key(*name))
            .map(|name| format!("pipeline '{name}' (an images table and a stage_tools table)")),
    );
    for (name, labels) in &items.builds {
        clashes.extend(
            labels
                .keys()
                .filter(|label| {
                    manifest
                        .builds
                        .get(name)
                        .is_some_and(|converted| converted.contains_key(*label))
                })
                .map(|label| {
                    format!("build '{name}@{label}' (an image manifest and a build manifest)")
                }),
        );
    }
    if !clashes.is_empty() {
        return Err(Error::new(format!(
            "Failed to build the toolbox: {} defined twice",
            clashes.join(", ")
        )));
    }
    // bundle the tools' own network policies at the top too, deduplicated by name with the
    // converted ones first, as converting their legacy images bundles them
    for version in items.tools.values().flat_map(|tool| tool.versions.values()) {
        for policy in &version.network_policies {
            if !manifest
                .network_policies
                .iter()
                .any(|existing| existing.name == policy.name)
            {
                manifest.network_policies.push(policy.clone());
            }
        }
    }
    // add the items beside the converted ones, and the base builds beside the converted labels
    manifest.tools.extend(items.tools);
    for (name, labels) in items.builds {
        manifest.builds.entry(name).or_default().extend(labels);
    }
    manifest.toolsets.extend(items.toolsets);
    manifest.agents.extend(items.agents);
    manifest.workflows.extend(items.workflows);
    manifest.pipelines.extend(items.pipelines);
    // check the whole manifest: errors stop the build and warnings are printed
    let issues = validate_manifest(&manifest, &ConvertReport::default(), &[]);
    let errors: Vec<String> = issues
        .iter()
        .filter(|issue| issue.severity == Severity::Error)
        .map(|issue| format!("{}: {} ({})", issue.item, issue.message, issue.code))
        .collect();
    for issue in issues
        .iter()
        .filter(|issue| issue.severity == Severity::Warning)
    {
        progress::warn(format!(
            "{}: {} ({})",
            issue.item, issue.message, issue.code
        ));
    }
    if !errors.is_empty() {
        return Err(Error::new(format!(
            "The toolbox has {} error(s):\n  {}",
            errors.len(),
            errors.join("\n  ")
        )));
    }
    Ok(manifest)
}
