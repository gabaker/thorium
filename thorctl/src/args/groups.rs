//! Arguments for group-related Thorctl commands

#![allow(clippy::module_name_repetitions)]

use std::path::PathBuf;

use clap::Parser;

use super::traits::describe::{DescribeCommand, DescribeSealed};
use super::traits::search::{SearchParameterized, SearchParams, SearchSealed};

/// The commands to send to the groups task handler
#[derive(Parser, Debug)]
pub enum Groups {
    /// Get a list of groups that you belong to
    #[clap(version, author)]
    Get(GetGroups),
    /// Describe specific groups, displaying/saving details in JSON format
    #[clap(version, author)]
    Describe(DescribeGroups),
    /// Create a group that you own
    #[clap(version, author)]
    Create(CreateGroup),
}

/// A command to create a group
#[derive(Parser, Debug)]
pub struct CreateGroup {
    /// The name of the group to create (lowercase letters, numbers and dashes)
    pub name: String,
    /// What the group is for
    #[clap(short, long)]
    pub description: Option<String>,
    /// Another user to make an owner of the group (repeatable); you are always an owner
    #[clap(long, value_name = "USER")]
    pub owner: Vec<String>,
}

#[derive(Parser, Debug)]
pub struct GetGroups {
    /// Print the groups in alphabetical order rather than by the date they were joined
    #[clap(short, long)]
    pub alpha: bool,
}

/// A command to describe particular groups in full
#[derive(Parser, Debug)]
pub struct DescribeGroups {
    /// Any specific groups to describe
    pub groups: Vec<String>,
    /// The path to a file containing a list of groups to describe separated by newlines
    #[clap(short = 'L', long = "list")]
    pub list: Option<PathBuf>,
    /// The path to the file to write output to; if not provided, details will be output to stdout
    #[clap(short, long)]
    pub output: Option<PathBuf>,
    /// Output details in a condensed format (no formatting/whitespace)
    #[clap(long)]
    pub condensed: bool,
    /// Describe all groups to which you have access (still within the limit given in `--limit`)
    #[clap(long)]
    pub all: bool,
    /// The maximum number of groups to retrieve
    #[clap(short, long, default_value_t = 50)]
    pub limit: usize,
    /// Describe groups with no limit
    #[clap(long)]
    pub no_limit: bool,
    /// The number of groups to retrieve per request
    #[clap(short, long, default_value_t = 50)]
    pub page_size: usize,
}

impl SearchSealed for DescribeGroups {
    fn get_search_params(&self) -> SearchParams<'_> {
        SearchParams {
            groups: &[],
            tags: &[],
            tags_case_insensitive: false,
            delimiter: '=',
            start: &None,
            end: &None,
            date_fmt: "",
            cursor: None,
            limit: self.limit,
            no_limit: self.no_limit,
            page_size: self.page_size,
        }
    }
}

impl SearchParameterized for DescribeGroups {
    fn has_targets(&self) -> bool {
        !self.groups.is_empty() || self.list.is_some()
    }

    fn apply_to_all(&self) -> bool {
        self.all
    }
}

impl DescribeSealed for DescribeGroups {
    type Data = thorium::models::Group;

    type Target<'a> = &'a str;

    type Cursor = thorium::client::Cursor<Self::Data>;

    fn raw_targets(&self) -> &[String] {
        &self.groups
    }

    fn condensed(&self) -> bool {
        self.condensed
    }

    fn out_path(&self) -> Option<&std::path::PathBuf> {
        self.output.as_ref()
    }

    fn target_list(&self) -> Option<&std::path::PathBuf> {
        self.list.as_ref()
    }

    fn parse_target<'a>(&self, raw: &'a str) -> Result<Self::Target<'a>, thorium::Error> {
        // no parsing is required so just return the raw target
        Ok(raw)
    }

    async fn retrieve_data(
        &self,
        target: Self::Target<'_>,
        thorium: &thorium::Thorium,
    ) -> Result<Self::Data, thorium::Error> {
        thorium.groups.get(target).await
    }

    /// List all groups out in a cursor; this shouldn't be called unless
    /// `all` is set
    async fn retrieve_data_search(
        &self,
        thorium: &thorium::Thorium,
    ) -> Result<Vec<Self::Cursor>, thorium::Error> {
        let params = self.get_search_params();
        let mut cursor = thorium
            .groups
            .list()
            .details()
            .page_size(params.page_size as u64);
        if !params.no_limit {
            cursor = cursor.limit(params.limit as u64);
        }
        Ok(vec![cursor])
    }
}

impl DescribeCommand for DescribeGroups {}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::Groups;
    use crate::args::{Args, SubCommands};

    /// `groups create` takes a name, a description and any number of owners
    #[test]
    fn create_args() {
        // parse a create that uses every flag
        let args = Args::try_parse_from([
            "thorctl",
            "groups",
            "create",
            "corn",
            "-d",
            "Corn things",
            "--owner",
            "bob",
            "--owner",
            "sarah",
        ])
        .expect("groups create parses");
        let SubCommands::Groups(Groups::Create(cmd)) = &args.cmd else {
            panic!("expected groups create");
        };
        assert_eq!(cmd.name, "corn");
        assert_eq!(cmd.description.as_deref(), Some("Corn things"));
        assert_eq!(cmd.owner, ["bob", "sarah"]);
        // only the name is required
        let args = Args::try_parse_from(["thorctl", "groups", "create", "corn"])
            .expect("a bare create parses");
        let SubCommands::Groups(Groups::Create(cmd)) = &args.cmd else {
            panic!("expected groups create");
        };
        assert!(cmd.description.is_none() && cmd.owner.is_empty());
        // a create without a name is refused
        assert!(Args::try_parse_from(["thorctl", "groups", "create"]).is_err());
        // the singular alias reaches the same command
        assert!(Args::try_parse_from(["thorctl", "group", "create", "corn"]).is_ok());
    }
}
