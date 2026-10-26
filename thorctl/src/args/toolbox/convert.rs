//! Arguments for `thorctl toolbox convert`

use clap::Parser;
use std::path::PathBuf;

use super::ManifestLocation;
use crate::args::helpers::FormatArgs;

/// Convert a legacy toolbox manifest to the current format
// each option is its own flag
#[allow(clippy::struct_excessive_bools)]
#[derive(Parser, Debug, Clone)]
pub struct ConvertToolbox {
    /// The toolbox to convert. Format: `path | url | dir`
    #[clap(value_name = "PATH | URL | DIR")]
    pub input: ManifestLocation,
    /// Where to write the converted manifest, or `-` for stdout (required for a URL input)
    #[clap(short, long, value_name = "PATH | -")]
    pub output: Option<PathBuf>,
    /// Write the converted manifest over the input, first saving the input as `INPUT.bak`
    #[clap(long, conflicts_with = "output")]
    pub in_place: bool,
    /// Do not save a `.bak` copy of the input with --in-place (an existing `.bak` is an error
    /// without this flag)
    #[clap(long, requires = "in_place")]
    pub no_backup: bool,
    /// Write nothing and exit with code 1 when the converted manifest differs from the output
    /// file (or is missing it), printing a diff
    #[clap(long)]
    pub check: bool,
    /// Write nothing; print the report and, when the output file exists, a diff against it
    #[clap(long)]
    pub dry_run: bool,
    /// Write the report as JSON to this file, or to stdout with `-`, instead of printing it on
    /// stderr
    #[clap(long, value_name = "FILE | -")]
    pub report: Option<PathBuf>,
    /// Write nothing and exit with code 1 when any item was skipped or any issue was raised
    #[clap(long)]
    pub strict: bool,
    /// Put every tool and pipeline in this group instead of the groups the legacy entries name
    #[clap(long, value_name = "GROUP")]
    pub group_override: Option<String>,
    /// Make pipeline stages follow their tool's latest version instead of pinning the version
    /// the legacy pipeline named
    #[clap(long)]
    pub follow_latest: bool,
    /// Give a container image entry with no `image` the first of its build `image_tags`
    /// (default: skip the entry)
    #[clap(long)]
    pub fill_image_from_tags: bool,
    /// Keep a pipeline that has a stage with no converted tool, flagged `incomplete` (default:
    /// leave the pipeline out)
    #[clap(long)]
    pub keep_incomplete_pipelines: bool,
    /// The format to print the report in (when it is not written by --report)
    #[clap(flatten)]
    pub format: FormatArgs,
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;
    use crate::utils::output::OutputFormat;

    /// Parse a `convert` command line
    ///
    /// # Arguments
    ///
    /// * `argv` - The arguments after `convert`
    fn parse<'a>(argv: impl IntoIterator<Item = &'a str>) -> Result<ConvertToolbox, clap::Error> {
        // the parser takes the program name first
        ConvertToolbox::try_parse_from(std::iter::once("convert").chain(argv))
    }

    /// Only an input is required and every option starts off
    #[test]
    fn defaults_change_nothing() {
        // a bare input converts next to itself with no extra behavior
        let cmd = parse(["toolbox.json"]).unwrap();
        assert!(cmd.output.is_none() && cmd.report.is_none() && cmd.group_override.is_none());
        assert!(!cmd.in_place && !cmd.no_backup && !cmd.check && !cmd.dry_run && !cmd.strict);
        assert!(!cmd.follow_latest && !cmd.fill_image_from_tags && !cmd.keep_incomplete_pipelines);
        assert_eq!(cmd.format.format, None);
        // an input is required
        assert!(parse([]).is_err());
    }

    /// Every flag of the command parses
    #[test]
    fn every_flag_parses() {
        let cmd = parse([
            "toolbox.json",
            "-o",
            "out.json",
            "--check",
            "--dry-run",
            "--report",
            "-",
            "--strict",
            "--group-override",
            "static",
            "--follow-latest",
            "--fill-image-from-tags",
            "--keep-incomplete-pipelines",
            "--format",
            "json",
        ])
        .unwrap();
        assert_eq!(cmd.output, Some(PathBuf::from("out.json")));
        assert!(cmd.check && cmd.dry_run && cmd.strict && cmd.follow_latest);
        assert!(cmd.fill_image_from_tags && cmd.keep_incomplete_pipelines);
        assert_eq!(cmd.report, Some(PathBuf::from("-")));
        assert_eq!(cmd.group_override.as_deref(), Some("static"));
        assert_eq!(cmd.format.format, Some(OutputFormat::Json));
    }

    /// `--in-place` replaces the output flag, and only it can skip the backup
    #[test]
    fn in_place_rules() {
        // in place with a backup choice parses
        let cmd = parse(["toolbox.json", "--in-place", "--no-backup"]).unwrap();
        assert!(cmd.in_place && cmd.no_backup);
        // it cannot also name an output
        assert!(parse(["toolbox.json", "--in-place", "-o", "out.json"]).is_err());
        // skipping the backup means nothing without it
        assert!(parse(["toolbox.json", "--no-backup"]).is_err());
    }
}
