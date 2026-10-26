//! Arguments for `thorctl toolbox validate`

use clap::Parser;

use super::ManifestLocation;
use crate::args::helpers::FormatArgs;

/// Check a toolbox manifest without talking to Thorium
#[derive(Parser, Debug)]
pub struct ValidateToolbox {
    /// The toolbox to check. Format: `path | url | dir`
    #[clap(value_name = "PATH | URL | DIR")]
    pub manifest: ManifestLocation,
    /// Exit with code 1 on warnings and skipped items too, not only on errors
    #[clap(long)]
    pub strict: bool,
    /// The format to print in
    #[clap(flatten)]
    pub format: FormatArgs,
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;
    use crate::utils::output::OutputFormat;

    /// `--strict` and `--format` parse, and a manifest is required
    #[test]
    fn validate_flags_parse() {
        // no flags is a non-strict check in the default format
        let cmd = ValidateToolbox::try_parse_from(["validate", "toolbox.json"]).unwrap();
        assert!(!cmd.strict);
        assert_eq!(cmd.format.format, None);
        // strict and a format are taken
        let cmd = ValidateToolbox::try_parse_from([
            "validate",
            "toolbox.json",
            "--strict",
            "--format",
            "yaml",
        ])
        .unwrap();
        assert!(cmd.strict);
        assert_eq!(cmd.format.format, Some(OutputFormat::Yaml));
        // a manifest is required
        assert!(ValidateToolbox::try_parse_from(["validate"]).is_err());
    }
}
