//! Arguments for `thorctl toolbox remove`

use clap::Parser;

use super::ManifestLocation;

/// Remove a toolbox's pipelines, workflows, agents, toolsets, and tools from Thorium
#[derive(Parser, Debug)]
pub struct RemoveToolbox {
    /// The toolbox manifest to use: a toolbox.json path or an http(s) URL, or a toolbox directory.
    /// Format: `path | url | dir`
    ///
    /// A legacy toolbox is converted in memory first, so its images are removed as the tools an
    /// import created. A directory is read through its toolbox.json.
    #[clap(value_name = "PATH | URL | DIR")]
    pub manifest: ManifestLocation,
    /// Remove from this group instead of the groups recorded in the manifest
    ///
    /// Use this when the toolbox was imported with --group-override.
    #[clap(long, value_name = "GROUP")]
    pub group_override: Option<String>,
    /// Skip the confirmation dialog
    ///
    /// The resources being removed are still listed before anything is deleted.
    #[clap(long, visible_alias = "yes", conflicts_with = "dry_run")]
    pub skip_confirm: bool,
    /// List what would be removed without deleting anything
    ///
    /// Needs no terminal, so it can preview a removal in CI.
    #[clap(long)]
    pub dry_run: bool,
}
