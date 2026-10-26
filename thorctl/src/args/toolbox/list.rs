//! Arguments for `thorctl toolbox list`

use clap::{Parser, ValueEnum};

use super::ManifestLocation;
use crate::args::helpers::ShortFormatArgs;

/// The kinds of item `toolbox list` can be limited to
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ListKind {
    /// Tools
    Tools,
    /// Toolsets
    Toolsets,
    /// Agents
    Agents,
    /// Workflows
    Workflows,
    /// Pipelines
    Pipelines,
    /// Build-only image contexts
    Builds,
}

/// List what a toolbox manifest contains
#[derive(Parser, Debug)]
pub struct ListToolbox {
    /// The toolbox to list. Format: `path | url | dir`
    #[clap(value_name = "PATH | URL | DIR")]
    pub manifest: ManifestLocation,
    /// Only list this kind of item
    #[clap(long, value_enum, ignore_case = true)]
    pub kind: Option<ListKind>,
    /// The format to print in
    #[clap(flatten)]
    pub format: ShortFormatArgs,
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;
    use crate::utils::output::OutputFormat;

    /// The kind filter and the short format flag parse, and a bad kind is refused
    #[test]
    fn list_flags_parse() {
        // no flags lists everything in the default format
        let cmd = ListToolbox::try_parse_from(["list", "toolbox.json"]).unwrap();
        assert_eq!(cmd.kind, None);
        assert_eq!(cmd.format.format, None);
        // a kind and a format are taken, in any case
        let cmd = ListToolbox::try_parse_from([
            "list",
            "toolbox.json",
            "--kind",
            "Pipelines",
            "-o",
            "json",
        ])
        .unwrap();
        assert_eq!(cmd.kind, Some(ListKind::Pipelines));
        assert_eq!(cmd.format.format, Some(OutputFormat::Json));
        // an unknown kind is refused
        assert!(ListToolbox::try_parse_from(["list", "toolbox.json", "--kind", "images"]).is_err());
        // a manifest is required
        assert!(ListToolbox::try_parse_from(["list"]).is_err());
    }
}
