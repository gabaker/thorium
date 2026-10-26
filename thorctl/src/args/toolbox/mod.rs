//! Arguments for toolbox-related Thorctl commands
//!
//! Short-flag policy (applies to every toolbox subcommand, and mirrors the wider `thorctl` CLI):
//! - Short flags are reserved for non-destructive selection/IO inputs and standard run toggles, used
//!   consistently across commands: `-g` group, `-i` images, `-p` pipelines, `-o` output,
//!   `-c` config, `-n` non-interactive, `-e` exit-on-error.
//! - Destructive and scope/reconciliation flags are intentionally **long-only** (no short), so a
//!   stray short can't trigger a less-reversible action: `--skip-confirm`, `--dry-run`,
//!   `--overwrite`, `--overwrite-config`, `--skip-conflicts`, `--group-override`,
//!   `--update-network-policy`, `--exit-code`, `--strip-registry`, `--with-images`, `--review`,
//!   `--rollback-on-failure`, `--push`.
//! - Known pre-existing cross-command exceptions (left as-is to avoid breaking users): `-g` vs `-G`
//!   are both used for groups elsewhere in `thorctl`, and `-c` means `--config` here but a boolean
//!   toggle in some other commands. Don't extend these; new flags should follow the rules above.

use clap::Parser;
use std::path::PathBuf;
use url::Url;

mod build;
mod convert;
mod diff;
mod export;
mod import;
mod init;
mod list;
mod remove;
mod update;
mod validate;

pub use build::{BuildImagesToolbox, BuildToolbox};
pub use convert::ConvertToolbox;
pub use diff::DiffToolbox;
pub use export::{ExportToolbox, ResourceSpec};
pub use import::{ImportOpts, ImportToolbox, OnlyKind};
pub use init::{Init, InitImage, InitPipeline, InitTool, InitToolKind, InitToolbox, PipelineSpec};
pub use list::{ListKind, ListToolbox};
pub use remove::RemoveToolbox;
pub use update::UpdateToolbox;
pub use validate::ValidateToolbox;

/// A command to interact with Thorium toolboxes
#[derive(Parser, Debug)]
pub enum Toolbox {
    /// Import a toolbox into Thorium
    ///
    /// A Thorium toolbox is an external collection of tools, toolsets, agents, workflows, and
    /// pipelines pre-configured and ready to run in Thorium. Legacy toolboxes (images and
    /// pipelines) are converted in memory first, and the issues the conversion finds are
    /// reported. Tools are created and their versions published in dependency order, and
    /// importing the same toolbox again changes nothing.
    ///
    /// Tool versions are told apart by the hash of their spec. A version that matches a
    /// published one is left alone; one with a new label is published; one whose label is taken
    /// by a different spec is published as LABEL+HASH, since published versions never change.
    /// If items already exist and differ you are asked per item to Apply, Skip, or Quit; this
    /// needs a terminal. Use --overwrite to apply every difference without asking, or
    /// --skip-conflicts to leave existing items untouched. Use --dry-run to see the plan.
    #[clap(version, author)]
    Import(ImportToolbox),
    /// Build a toolbox manifest from tool, image, and pipeline manifests
    ///
    /// Walks the directory containing --config (or --path, if given) for manifest.toml files
    /// (tools, base builds, toolsets, agents, workflows, pipelines, and legacy images) and reads
    /// the specs and configs they point at to produce a toolbox.json file suitable for import into
    /// Thorium.
    ///
    /// The format follows the manifests: any `type = "tool"`, `"build"`, `"toolset"`, `"agent"`,
    /// or `"workflow"` manifest writes the current format (format 2), and legacy `type = "image"`
    /// manifests alone write the legacy format (format 1) with a hint to convert. Use
    /// --format-version to choose, and --rewrite-manifests to convert a directory of legacy
    /// manifests in place.
    #[clap(version, author)]
    Build(BuildToolbox),
    /// Initialize toolbox, tool, image, or pipeline scaffolding
    ///
    /// Generate default manifest.toml files with their spec or JSON config. Existing files
    /// are skipped unless --overwrite (per-tool files) or --overwrite-config
    /// (config.toml, `init toolbox` only) is given.
    #[clap(version, author, subcommand)]
    Init(Init),
    /// Export Thorium tools and pipelines into a toolbox directory structure
    ///
    /// Reads tools (with every version's spec), pipelines, and with --all-kinds toolsets, agents,
    /// and workflows from a running Thorium instance, and writes a toolbox directory with a
    /// manifest.toml per item and a v2 toolbox.json ready to validate, diff, and import elsewhere.
    /// Container images stay in their registry; model endpoints, profiles, and credentials are
    /// never exported.
    #[clap(version, author)]
    Export(ExportToolbox),
    /// Remove a previously imported toolbox from Thorium
    ///
    /// Deletes the pipelines, workflows, agents, toolsets, and tools named by a toolbox manifest
    /// from the target instance, in dependency order: pipelines, workflows, agents, and toolsets
    /// first, then each tool's versions and the tool. Items that don't exist are reported and
    /// skipped, and a tool still used by something outside the toolbox is left in place and
    /// reported. Groups and network policies are never deleted.
    #[clap(version, author)]
    Remove(RemoveToolbox),
    /// Diff an on-disk toolbox against what a Thorium instance has imported
    ///
    /// Shows what an import of this toolbox would change: each tool version is listed as new,
    /// unchanged (counted only), or a label conflict, and toolsets, agents, workflows, and pipelines
    /// that differ show unified hunks of their normalized definitions. A summary line counts each
    /// kind. Use --exit-code to fail when an import would change something.
    #[clap(version, author)]
    Diff(DiffToolbox),
    /// Build (and optionally push) the container images in a toolbox
    ///
    /// Walks a toolbox.json (a legacy images toolbox, or a v2 toolbox with tool versions that have
    /// a `build` section and a `builds` section of base contexts) for entries with build enabled,
    /// builds each entry's docker context with its first tag, aliases the remaining tags, and
    /// pushes all tags with --push. Base contexts are built before the tools that start from
    /// them. For forks without CI: build, push, then import as usual.
    #[clap(version, author, name = "build-images")]
    BuildImages(BuildImagesToolbox),
    /// Convert a legacy toolbox manifest to the current format
    ///
    /// Reads a legacy toolbox (a toolbox.json with images and pipelines, given as a path, a URL,
    /// or a toolbox directory) and writes the same toolbox as tools with their specs, bundled
    /// build contexts, and pipelines whose stages name tools. Nothing is sent to Thorium and no
    /// login is needed. The input is never changed unless --in-place is given.
    ///
    /// By default the result is written to toolbox.v2.json next to the input. Images with no
    /// container image are skipped, exactly as the migration of stored images skips them, and a
    /// pipeline with a stage whose tool was not converted is left out; both are listed in the
    /// report together with every other issue. A manifest that is already in the current format
    /// is accepted and converts to itself.
    #[clap(version, author)]
    Convert(ConvertToolbox),
    /// List what a toolbox manifest contains
    ///
    /// Reads a toolbox manifest (a legacy or a v2 toolbox.json, a path, URL, or toolbox
    /// directory) and lists its tools, toolsets, agents, workflows, pipelines, and
    /// build-only image contexts with their groups and versions. Nothing is sent to Thorium.
    #[clap(version, author)]
    List(ListToolbox),
    /// Check a toolbox manifest without talking to Thorium
    ///
    /// Reads a toolbox manifest (a legacy or a v2 toolbox.json, a path, URL, or toolbox
    /// directory) and reports every problem with it: unusable names, specs that don't parse,
    /// unresolved references, dependency cycles, pipeline stages without a tool, and more. Exits
    /// with code 1 when there are errors, or with --strict when there are warnings too.
    #[clap(version, author)]
    Validate(ValidateToolbox),
    /// Refresh the items of a toolbox that were imported earlier
    ///
    /// Works like import, but only touches the tools, toolsets, agents, workflows, and
    /// pipelines that already exist in the target groups: nothing is created, no group is
    /// created, and the items that are missing are listed as not imported. Use it to bring what
    /// you imported before up to date with a newer copy of the toolbox.
    #[clap(version, author)]
    Update(UpdateToolbox),
}

/// The location of the toolbox manifest, either by URL or by file path
#[derive(Debug, Clone)]
pub enum ManifestLocation {
    /// The manifest is at this URL
    Url(Url),
    /// The manifest is at this file path
    Path(PathBuf),
}

impl std::str::FromStr for ManifestLocation {
    type Err = String;

    /// Parse a string into a [`ManifestLocation`]
    ///
    /// Only `http://` and `https://` URLs are fetched remotely, since those are the
    /// only schemes the downloader supports. A `file://` URL is converted to its local
    /// path, and everything else — including Windows drive paths like `C:\tb\toolbox.json`,
    /// which parse as URLs with a single-letter scheme — is treated as a local path.
    ///
    /// # Arguments
    ///
    /// * `s` - The raw manifest location string (a URL or a filesystem path)
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match Url::parse(s) {
            // remote manifests are only fetched over http(s)
            Ok(url) if matches!(url.scheme(), "http" | "https") => Ok(Self::Url(url)),
            // a file url names a local file, so read it from disk
            Ok(url) if url.scheme() == "file" => url
                .to_file_path()
                .map(Self::Path)
                .map_err(|()| format!("'{s}' is not a valid local file URL")),
            // any other explicit `scheme://` can't be downloaded, so reject it clearly
            // instead of reporting a missing local file
            Ok(url) if url.scheme().len() > 1 && s.contains("://") => Err(format!(
                "unsupported URL scheme '{}' in '{s}'; use an http(s) URL or a local path",
                url.scheme()
            )),
            // anything else (a relative path, or a drive path parsed as a one-letter
            // scheme) is a local filesystem path
            _ => Ok(Self::Path(PathBuf::from(s))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only http(s) URLs are remote; drive paths and relative paths are local files
    #[test]
    fn manifest_location_only_http_is_url() {
        use std::str::FromStr;
        // http and https are fetched remotely
        assert!(matches!(
            ManifestLocation::from_str("https://example.com/toolbox.json"),
            Ok(ManifestLocation::Url(_))
        ));
        assert!(matches!(
            ManifestLocation::from_str("http://example.com/toolbox.json"),
            Ok(ManifestLocation::Url(_))
        ));
        // Windows drive paths (either slash style) parse as a one-letter scheme but are paths
        for raw in ["C:\\foo\\toolbox.json", "C:/foo", "c:\\toolbox.json"] {
            match ManifestLocation::from_str(raw) {
                Ok(ManifestLocation::Path(path)) => assert_eq!(path, PathBuf::from(raw)),
                other => panic!("expected a path for {raw}, got {other:?}"),
            }
        }
        // relative and absolute unix paths stay paths
        assert!(matches!(
            ManifestLocation::from_str("./toolbox.json"),
            Ok(ManifestLocation::Path(_))
        ));
        assert!(matches!(
            ManifestLocation::from_str("/tmp/toolbox.json"),
            Ok(ManifestLocation::Path(_))
        ));
        // a file url becomes its local path (a drive-less file url is only valid on unix)
        if cfg!(unix) {
            match ManifestLocation::from_str("file:///tmp/toolbox.json") {
                Ok(ManifestLocation::Path(path)) => {
                    assert_eq!(path, PathBuf::from("/tmp/toolbox.json"));
                }
                other => panic!("expected a path for a file url, got {other:?}"),
            }
        }
        // other explicit schemes can't be downloaded and are rejected
        assert!(ManifestLocation::from_str("ftp://example.com/toolbox.json").is_err());
    }

    /// The paths of every `toolbox` command whose help output is pinned by the golden file
    const HELP_PATHS: [&[&str]; 16] = [
        &[],
        &["import"],
        &["build"],
        &["init"],
        &["init", "toolbox"],
        &["init", "image"],
        &["init", "pipeline"],
        &["init", "tool"],
        &["export"],
        &["remove"],
        &["diff"],
        &["build-images"],
        &["list"],
        &["validate"],
        &["convert"],
        &["update"],
    ];

    /// Render the long `--help` text of a `thorctl toolbox` command path
    ///
    /// # Arguments
    ///
    /// * `path` - The subcommand path under `toolbox` to render help for
    fn render_help(path: &[&str]) -> String {
        // build the full thorctl command at a fixed width so the output doesn't depend on
        // the terminal the tests run in
        let command = <crate::args::Args as clap::CommandFactory>::command().term_width(100);
        // ask for the help of the requested path, which clap reports as a display error
        let mut argv = vec!["thorctl", "toolbox"];
        argv.extend_from_slice(path);
        argv.push("--help");
        let err = command
            .try_get_matches_from(argv)
            .expect_err("--help always ends parsing");
        // clap renders help without styling when converted to a string
        err.to_string()
    }

    /// Split the golden help file into its `=== thorctl ...` sections, keyed by path
    ///
    /// # Arguments
    ///
    /// * `golden` - The contents of the golden help file
    fn golden_sections(golden: &str) -> Vec<(String, String)> {
        // each section starts with a header line naming the command path
        let mut sections: Vec<(String, String)> = Vec::new();
        for line in golden.lines() {
            if let Some(header) = line.strip_prefix("=== thorctl ") {
                // the path is everything before the recorded exit status
                let path = header
                    .split(" (exit")
                    .next()
                    .unwrap_or_default()
                    .to_string();
                sections.push((path, String::new()));
            } else if let Some((_, body)) = sections.last_mut() {
                body.push_str(line);
                body.push('\n');
            }
        }
        sections
    }

    /// Render the golden file's contents for every pinned command path
    fn render_golden() -> String {
        // each section is a header naming the path followed by that command's help text
        let sections: Vec<String> = HELP_PATHS
            .iter()
            .map(|path| {
                let mut section = "=== thorctl toolbox".to_owned();
                for part in *path {
                    section.push(' ');
                    section.push_str(part);
                }
                format!("{section} (exit 0)\n{}\n", render_help(path).trim_end())
            })
            .collect();
        // sections are separated by a blank line
        sections.join("\n")
    }

    /// The `toolbox` help output and every subcommand's help output match the golden file
    ///
    /// Set `THORCTL_UPDATE_GOLDEN` to rewrite the golden file from the current help text.
    #[test]
    fn toolbox_help_matches_golden() {
        // an update run records the current help text instead of checking it
        if std::env::var_os("THORCTL_UPDATE_GOLDEN").is_some() {
            let path = concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/data/toolbox_help.golden"
            );
            std::fs::write(path, render_golden()).expect("write the golden file");
            return;
        }
        // read the pinned help text captured from the released command tree
        let sections = golden_sections(include_str!("../../../tests/data/toolbox_help.golden"));
        // every pinned path must have a section, and no section may be unaccounted for
        assert_eq!(sections.len(), HELP_PATHS.len());
        for path in HELP_PATHS {
            // the golden section is keyed by the space separated command path
            let key = if path.is_empty() {
                "toolbox".to_string()
            } else {
                format!("toolbox {}", path.join(" "))
            };
            let (_, expected) = sections
                .iter()
                .find(|(name, _)| *name == key)
                .unwrap_or_else(|| panic!("no golden section for {key}"));
            // compare ignoring only the blank lines that separate sections
            assert_eq!(render_help(path).trim_end(), expected.trim_end(), "{key}");
        }
    }
}
