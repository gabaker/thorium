//! Arguments for `thorctl toolbox update`

use clap::Parser;

use super::import::ImportOpts;

/// Refresh the items of a toolbox that were imported earlier
#[derive(Parser, Debug)]
pub struct UpdateToolbox {
    /// What to refresh and how; the settings are those of `toolbox import`
    #[clap(flatten)]
    pub opts: ImportOpts,
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    /// An update takes the import settings but cannot create groups
    #[test]
    fn update_takes_import_flags_but_not_create_groups() {
        // the shared flags parse
        let cmd = UpdateToolbox::try_parse_from([
            "update",
            "toolbox.json",
            "--overwrite",
            "--dry-run",
            "-t",
            "a",
        ])
        .unwrap();
        assert!(cmd.opts.overwrite && cmd.opts.dry_run);
        assert_eq!(cmd.opts.tools, ["a"]);
        // creating groups makes no sense for a command that creates nothing
        assert!(
            UpdateToolbox::try_parse_from(["update", "toolbox.json", "--create-groups"]).is_err()
        );
        // the manifest is required
        assert!(UpdateToolbox::try_parse_from(["update"]).is_err());
    }
}
