//! Arguments for `thorctl toolbox export`

use clap::Parser;
use std::path::PathBuf;

use crate::args::helpers::FormatArgs;

/// Export Thorium tools and pipelines (and optionally toolsets, agents, and workflows) into a
/// toolbox directory
#[derive(Parser, Debug)]
#[allow(clippy::struct_excessive_bools)]
pub struct ExportToolbox {
    /// Export everything of the selected kinds from this Thorium group (the source group to read from)
    ///
    /// Tools and pipelines are exported by default and `--all-kinds` adds toolsets, agents, and
    /// workflows. Exported items are written under this same group unless `--group-override` sets a
    /// different destination group. When re-exporting into an existing toolbox whose items live under
    /// a different group, `--overwrite` rewrites them in place to this group (no `--group-override`
    /// needed); without `--overwrite` a differing item is an error rather than a duplicate. With any
    /// of `--tools`, `--pipelines`, `--toolsets`, `--agents`, or `--workflows`, this is only the
    /// default group of the names given without one.
    #[clap(short = 'g', long = "group", value_name = "GROUP")]
    pub group: Option<String>,
    /// Export specific pipelines. Format: `group/name[=dir]` (or `name[=dir]` with --group),
    /// comma-separated for multiple.
    ///
    /// Tools used by exported pipelines are auto-included. The optional `[=dir]` suffix writes that
    /// pipeline's files to `dir`, e.g. `static/av=pipelines/av`; without it the default layout
    /// (`pipelines/<group>/<name>`) is used. `dir` must be relative to the toolbox root and must not
    /// leave it. `[=dir]` is placement only; it never changes which pipeline (or its tools) is
    /// selected.
    #[clap(
        short = 'p',
        long = "pipelines",
        value_name = "GROUP/NAME[=DIR]",
        value_delimiter = ','
    )]
    pub pipelines: Vec<String>,
    /// Export specific tools. Format: `group/name[=dir]` (or `name[=dir]` with --group),
    /// comma-separated for multiple.
    ///
    /// Every version of a tool is exported (deprecated versions only with `--include-deprecated`).
    /// The optional `[=dir]` suffix writes that tool's files to `dir`, e.g.
    /// `static/clamav=tools/clamav`; naming an auto-included dependency tool this way also
    /// redirects it. `dir` must be relative to the toolbox root and must not leave it. `[=dir]` is
    /// placement only. `-i/--images` is a hidden alias of this flag for older scripts.
    #[clap(
        short = 't',
        long = "tools",
        alias = "images",
        short_alias = 'i',
        value_name = "GROUP/NAME[=DIR]",
        value_delimiter = ','
    )]
    pub tools: Vec<String>,
    /// Export specific toolsets. Format: `group/name[=dir]` (or `name[=dir]` with --group),
    /// comma-separated for multiple.
    ///
    /// The tools a toolset includes are auto-included.
    #[clap(
        long = "toolsets",
        value_name = "GROUP/NAME[=DIR]",
        value_delimiter = ','
    )]
    pub toolsets: Vec<String>,
    /// Export specific agents. Format: `group/name[=dir]` (or `name[=dir]` with --group),
    /// comma-separated for multiple.
    ///
    /// The harness, tools, toolsets, and sub-agents an agent names are auto-included.
    #[clap(
        long = "agents",
        value_name = "GROUP/NAME[=DIR]",
        value_delimiter = ','
    )]
    pub agents: Vec<String>,
    /// Export specific workflows. Format: `group/name[=dir]` (or `name[=dir]` with --group),
    /// comma-separated for multiple.
    ///
    /// The agents a workflow runs, and everything they name, are auto-included.
    #[clap(
        long = "workflows",
        value_name = "GROUP/NAME[=DIR]",
        value_delimiter = ','
    )]
    pub workflows: Vec<String>,
    /// With --group and no named items, also export the group's toolsets, agents, and workflows
    #[clap(long)]
    pub all_kinds: bool,
    /// Also export deprecated tool versions (by default they are skipped)
    ///
    /// A deprecated version that something pins (an agentic tool's container, a harness's provided
    /// tool) is exported anyway so the reference resolves.
    #[clap(long)]
    pub include_deprecated: bool,
    /// Only export each tool's latest version
    ///
    /// A version that something pins is exported anyway so the reference resolves.
    #[clap(long)]
    pub latest_only: bool,
    /// Destination group: write every exported item under this group instead of its source group
    ///
    /// Use it to read from one Thorium group (`-g`/`--pipelines`/`--tools`) but store the items under
    /// a different group in the toolbox, or to align an export with an existing toolbox's group so
    /// the items reconcile and update in place. Two items of one kind that share a name can't both
    /// be exported, with or without this flag.
    #[clap(long, value_name = "GROUP")]
    pub group_override: Option<String>,
    /// Root directory for the exported toolbox
    ///
    /// Defaults to the `--config` directory when `--config` is given (so pointing at a toolbox's
    /// config.toml exports into that toolbox), otherwise `./toolbox` for a brand-new toolbox. An
    /// explicit value always wins; pass it to seed settings from one toolbox into a different
    /// output directory.
    #[clap(short = 'o', long = "output", value_name = "DIR")]
    pub output: Option<PathBuf>,
    /// Seed the toolbox-wide settings (name, registry, registries, `image_path_prefix`,
    /// `[base_image]`) from *another* toolbox's config.toml instead of --name/--registry
    ///
    /// This is for starting a new toolbox from an existing one's settings. Appending into a
    /// toolbox that already has a config.toml does NOT need this: an existing
    /// `<output>/config.toml` is auto-detected, reused, and preserved, and while it is kept it takes
    /// precedence over --config (which is then ignored with a warning). Settings-source priority
    /// is: kept `<output>/config.toml` > --config > --name/--registry. With --overwrite-config, an
    /// explicit --name/--registry is applied. Mutually exclusive with --name and --registry. Bundling
    /// is never inherited; it always follows --with-images.
    ///
    /// Giving --config also anchors --output to the config's directory unless --output is set, so
    /// `export -c mytb/config.toml ...` exports into `mytb/`. If the --config file doesn't exist, a
    /// warning is printed and a new toolbox is created there with the default name and no registry
    /// (--name/--registry can't be combined with --config).
    #[clap(short = 'c', long = "config", value_name = "CONFIG.TOML", conflicts_with_all = ["name", "registry"])]
    pub config: Option<PathBuf>,
    /// Toolbox name for config.toml (default: "My Toolbox" for a new toolbox)
    ///
    /// Ignored with a warning while an existing config.toml is kept; applied over it with
    /// --overwrite-config.
    #[clap(long, value_name = "NAME")]
    pub name: Option<String>,
    /// Container registry for config.toml, e.g. ghcr.io/org/repo
    ///
    /// Optional: when omitted, the exported toolbox declares no central registry and relies on
    /// each image tool's own `image` url (captured on export unless `--strip-registry` is used).
    #[clap(long, value_name = "REGISTRY")]
    pub registry: Option<String>,
    /// Skip on-disk conflicts: write new items and leave differing existing ones untouched with a
    /// warning (use --overwrite to overwrite instead)
    ///
    /// Without --overwrite or --skip-conflicts, an item that already exists in the output with
    /// different contents stops the export before anything is written.
    #[clap(long)]
    pub skip_conflicts: bool,
    /// Open each exported item in an editor to review/tweak it before writing (requires a terminal)
    #[clap(long)]
    pub review: bool,
    /// The editor to open with --review (default: the configured editor, then $VISUAL, then $EDITOR)
    #[clap(long, value_name = "EDITOR", requires = "review")]
    pub editor: Option<String>,
    /// Update a matched in-toolbox item that differs (otherwise the export stops listing it)
    ///
    /// When an item with the same kind and name already exists in the output and differs, this
    /// replaces it (its manifest.toml and spec files are rewritten and its toolbox.json entry is
    /// replaced); unchanged items are not rewritten. It does NOT touch config.toml; use
    /// --overwrite-config for that. With no selection and no --group, `--overwrite` refreshes every
    /// item already in the output from the same groups.
    #[clap(long, conflicts_with = "skip_conflicts")]
    pub overwrite: bool,
    /// Overwrite an existing config.toml with this run's settings
    ///
    /// By default an existing config.toml is preserved (and its settings reused), so exporting into
    /// an existing toolbox never clobbers its settings. Pass this to replace it. Distinct from
    /// --overwrite, which covers per-item files.
    #[clap(long)]
    pub overwrite_config: bool,
    /// Bundle each image tool's container image file into the toolbox for offline transfer
    ///
    /// Pulls and saves each image tool's container with the configured container runtime (docker or
    /// podman, see --container-runtime) into its directory as `<dir>/<name>.tar.gz`. Only K8s image
    /// tools run from a container, so tools with other scalers are exported without a tarball; a
    /// K8s tool with no container url fails the export. Requires a container runtime.
    #[clap(long)]
    pub with_images: bool,
    /// Write each image tool's container url as empty so the export carries no hard-coded registry
    /// path
    ///
    /// Clears the `image` url in every exported image tool's K8s environment, so a rebuild derives
    /// each image's path from the toolbox's own `config.toml` registry/`image_path_prefix` instead of
    /// a pinned url. Requires a registry in the effective config (--registry, --config, or the
    /// existing config.toml). **Conflicts with `--with-images`** (a bundled import needs the url to
    /// tag and push the saved tarball). A tool exported this way has a different spec hash than the
    /// tool in Thorium.
    #[clap(long, conflicts_with = "with_images")]
    pub strip_registry: bool,
    /// The format the export report is printed in
    #[clap(flatten)]
    pub format: FormatArgs,
}

/// A parsed group/name resource reference, with an optional on-disk destination
#[derive(Debug, Clone)]
pub struct ResourceSpec {
    /// The group the resource belongs to
    pub group: String,
    /// The name of the resource
    pub name: String,
    /// The optional per-resource destination directory (relative to the toolbox root) parsed from
    /// a `group/name=dest` selection; `None` uses the toolbox's configured/default export layout.
    /// Placement only — it never affects which resource is selected.
    pub dest: Option<String>,
}

impl ResourceSpec {
    /// Parse `group/name`, `name`, or either with an optional `=dest` placement suffix
    ///
    /// `static/clamav=tools/clamav` selects `static/clamav` and writes its files into `tools/clamav`;
    /// the `=dest` is placement only. Only emptiness is checked here — the dest may be absolute or
    /// relative (and may contain `..`); the export refuses a dest that is absolute or leaves the
    /// toolbox (see `dir_overrides` in the toolbox export handler). An empty group or name (such as
    /// `/clamav`, `static/`, or the empty element a trailing comma produces) is rejected.
    ///
    /// # Arguments
    ///
    /// * `s` - The resource reference to parse
    /// * `default_group` - The group to fall back to when the reference has no group prefix
    pub fn parse(s: &str, default_group: Option<&str>) -> Result<Self, String> {
        // split off an optional `=dest` placement suffix on the FIRST `=`, so the left side is the
        // group/name reference and the right side is the destination directory. The dest may be
        // absolute or relative (a relative dest is later interpreted against the toolbox root); it is
        // only checked for emptiness here — `export` resolves it and verifies it lands inside the
        // toolbox once the output root is known
        let (reference, dest) = match s.split_once('=') {
            Some((reference, dest)) => {
                if dest.is_empty() {
                    return Err(format!("'{s}' has an empty destination path after '='"));
                }
                (reference, Some(dest.to_string()))
            }
            None => (s, None),
        };
        // split the reference on the FIRST slash so an explicit `group/name` always wins; everything
        // after the first slash is the name (resource names may themselves contain slashes)
        let (group, name) = if let Some((group, name)) = reference.split_once('/') {
            (group.to_string(), name.to_string())
        } else {
            // with no `group/` prefix the reference is bare, so it can only be resolved when
            // a default group was supplied (from `--group`); otherwise the group is ambiguous
            match default_group {
                Some(group) => (group.to_string(), reference.to_string()),
                // reject rather than guess a group so a bare name can't silently land in the
                // wrong place when the caller never set one
                None => {
                    return Err(format!(
                        "'{reference}' must be in group/name format when --group is not set"
                    ));
                }
            }
        };
        // an empty group or name can't identify a resource; the usual cause is a stray
        // slash or a trailing comma in a comma-separated list
        if group.trim().is_empty() {
            return Err(format!("'{s}' has an empty group"));
        }
        if name.trim().is_empty() {
            return Err(format!(
                "'{s}' has an empty name (check for a stray '/' or trailing comma)"
            ));
        }
        Ok(Self { group, name, dest })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `group/name` reference parses into its parts with no destination
    #[test]
    fn resource_spec_parses_group_name() {
        let spec = ResourceSpec::parse("static/clamav", None).expect("group/name parses");
        assert_eq!(spec.group, "static");
        assert_eq!(spec.name, "clamav");
        assert!(spec.dest.is_none());
        // a bare name resolves only with a default group
        let bare = ResourceSpec::parse("clamav", Some("static")).expect("bare name with --group");
        assert_eq!(bare.group, "static");
        assert_eq!(bare.name, "clamav");
        assert!(ResourceSpec::parse("clamav", None).is_err());
    }

    /// An `=dest` suffix sets the placement directory, split on the first `=`
    #[test]
    fn resource_spec_parses_destination() {
        // the reference and destination split on the first '='
        let spec = ResourceSpec::parse("static/clamav=tools/clamav", None).expect("dest parses");
        assert_eq!(spec.group, "static");
        assert_eq!(spec.name, "clamav");
        assert_eq!(spec.dest.as_deref(), Some("tools/clamav"));
        // a bare name with a destination still needs a default group for the reference
        let bare = ResourceSpec::parse("clamav=tools/clamav", Some("static")).expect("bare + dest");
        assert_eq!(bare.name, "clamav");
        assert_eq!(bare.dest.as_deref(), Some("tools/clamav"));
        // absolute and parent-escaping destinations parse here (export resolves them against the
        // toolbox root and rejects only those that land outside it); an empty dest is rejected
        assert_eq!(
            ResourceSpec::parse("static/clamav=/abs/path", None)
                .expect("absolute dest parses")
                .dest
                .as_deref(),
            Some("/abs/path")
        );
        assert_eq!(
            ResourceSpec::parse("static/clamav=../rel", None)
                .expect("parent-relative dest parses")
                .dest
                .as_deref(),
            Some("../rel")
        );
        assert!(ResourceSpec::parse("static/clamav=", None).is_err());
    }

    /// An empty group or name is rejected, including the empty element of a trailing comma
    #[test]
    fn resource_spec_rejects_empty_parts() {
        // a leading slash leaves the group empty
        assert!(ResourceSpec::parse("/clamav", None).is_err());
        // a trailing slash leaves the name empty
        assert!(ResourceSpec::parse("static/", None).is_err());
        // a trailing comma yields an empty element, which is an empty name with --group
        assert!(ResourceSpec::parse("", Some("static")).is_err());
        // whitespace-only parts are just as empty
        assert!(ResourceSpec::parse(" /clamav", None).is_err());
        assert!(ResourceSpec::parse("static/ =dir", None).is_err());
    }
}
