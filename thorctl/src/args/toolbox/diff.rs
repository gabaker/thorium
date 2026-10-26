//! Arguments for `thorctl toolbox diff`

use clap::Parser;

use super::ManifestLocation;
use crate::args::helpers::FormatArgs;

/// Diff an on-disk toolbox against a running Thorium instance
#[derive(Parser, Debug)]
pub struct DiffToolbox {
    /// A toolbox.json (path or http(s) URL) or a toolbox repo directory. Format: `path | url | dir`
    ///
    /// A legacy toolbox is converted in memory first, so its images are compared as the tools an
    /// import would create. A directory is read through its toolbox.json.
    #[clap(value_name = "PATH | URL | DIR")]
    pub manifest: ManifestLocation,
    /// Compare against this group instead of the groups recorded in the toolbox
    ///
    /// Use this when the toolbox was imported with --group-override.
    #[clap(long, value_name = "GROUP")]
    pub group_override: Option<String>,
    /// Exit with code 1 when an import would change something (git diff --exit-code semantics)
    ///
    /// Counts tools that are new or gain a version, tool versions whose label exists with another
    /// spec, toolsets, agents, workflows, and pipelines that are new or differ, and differing
    /// network policies. Resources only in the instance never count, since an import doesn't
    /// delete them. Errors also exit with code 1, so exit code 1 alone doesn't distinguish drift
    /// from a failed diff.
    #[clap(long)]
    pub exit_code: bool,
    /// Target registry base path for images bundled in the toolbox
    ///
    /// Only used when the toolbox bundles container images (exported with `--with-images`). Each
    /// bundled image's url is compared as `<image-path-prefix>/<group>/<name>:<tag>`, the url an
    /// import with the same prefix would store. If omitted for a bundled toolbox, the prefix
    /// recorded in the manifest is used; with neither, urls are compared as-is. Never prompts.
    #[clap(long, value_name = "REGISTRY/BASE")]
    pub image_path_prefix: Option<String>,
    /// The format the diff is printed in: human (the default), table, json, or yaml
    #[clap(flatten)]
    pub format: FormatArgs,
}
