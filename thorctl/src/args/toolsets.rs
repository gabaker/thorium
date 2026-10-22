//! Arguments for toolset Thorctl commands

use clap::{Parser, ValueEnum};
use thorium::models::ToolSetSelector;
use uuid::Uuid;

use super::helpers::{ShortFormatArgs, WaitForArgs, WatchArgs};

/// The commands to send to the toolsets task handler
#[derive(Parser, Debug)]
pub enum ToolSets {
    /// List the toolsets in your groups
    #[clap(version, author)]
    Get(GetToolSets),
    /// Describe a toolset: its settings, included tools, and latest instance
    #[clap(version, author)]
    Describe(DescribeToolSet),
    /// Create a toolset of agentic tools that AI agents attach as one unit
    #[clap(version, author)]
    Create(CreateToolSet),
    /// Include a tool in a toolset, at its latest version or pinned to one version
    #[clap(version, author)]
    Include(IncludeTool),
    /// Stop including a tool in a toolset
    #[clap(version, author)]
    RemoveTool(RemoveTool),
    /// Set or clear the tag selector that adds tools to a toolset
    #[clap(version, author)]
    Selector(SetSelector),
    /// Turn a toolset's read-only filter on or off
    #[clap(version, author)]
    ReadOnly(SetReadOnly),
}

/// A command to list toolsets
#[derive(Parser, Debug)]
pub struct GetToolSets {
    /// The groups to list toolsets from (all of your groups by default)
    #[clap(short, long)]
    pub group: Vec<String>,
    /// The most toolsets to list
    #[clap(short, long, default_value_t = 50)]
    pub limit: usize,
    /// The format to print the toolsets in (a table by default)
    #[clap(flatten)]
    pub format: ShortFormatArgs,
    /// Re-read and re-print the toolsets until interrupted
    #[clap(flatten)]
    pub watch: WatchArgs,
    /// Wait until every listed toolset matches a condition
    #[clap(flatten)]
    pub wait: WaitForArgs,
}

/// A command to describe a toolset
#[derive(Parser, Debug)]
pub struct DescribeToolSet {
    /// The toolset to describe, by ID or name
    pub toolset: String,
    /// The format to describe the toolset in (human readable by default)
    #[clap(flatten)]
    pub format: ShortFormatArgs,
    /// Re-read and re-describe the toolset until interrupted
    #[clap(flatten)]
    pub watch: WatchArgs,
    /// Wait until the toolset matches a condition
    #[clap(flatten)]
    pub wait: WaitForArgs,
}

/// A command to create a toolset
#[derive(Parser, Debug)]
pub struct CreateToolSet {
    /// The name of the toolset
    pub name: String,
    /// The groups to create the toolset in
    #[clap(short, long, required = true)]
    pub group: Vec<String>,
    /// A tool to include at its latest version, by ID or name
    #[clap(long)]
    pub tool: Vec<String>,
    /// A tool instance to include, pinned to that version
    #[clap(long)]
    pub pin: Vec<Uuid>,
    /// The tag (key=value) that adds every agentic tool carrying it in all of the toolset's
    /// groups
    #[clap(long)]
    pub selector: Option<ToolSetSelector>,
    /// Only include tools whose version declares no effects
    #[clap(long)]
    pub read_only: bool,
    /// Don't build new toolset instances automatically when the toolset or its tools change
    #[clap(long)]
    pub no_auto_instance: bool,
    /// A description of the toolset
    #[clap(short, long)]
    pub description: Option<String>,
    /// Add the toolset's groups to included tools that are missing some of them
    #[clap(long)]
    pub extend_target_groups: bool,
}

/// A command to include a tool in a toolset
#[derive(Parser, Debug)]
pub struct IncludeTool {
    /// The toolset, by ID or name
    pub toolset: String,
    /// The tool to include, by ID or name
    pub tool: String,
    /// Pin the toolset to this version (tool instance) of the tool instead of its latest
    #[clap(long)]
    pub pin: Option<Uuid>,
    /// Add the toolset's groups to the tool when it's missing some of them
    #[clap(long)]
    pub extend_target_groups: bool,
}

/// A command to stop including a tool in a toolset
#[derive(Parser, Debug)]
pub struct RemoveTool {
    /// The toolset, by ID or name
    pub toolset: String,
    /// The tool to stop including, by ID or name
    pub tool: String,
    /// Remove the pin to this version (tool instance) instead of the latest edge
    #[clap(long)]
    pub pin: Option<Uuid>,
}

/// A command to set or clear a toolset's tag selector
#[derive(Parser, Debug)]
pub struct SetSelector {
    /// The toolset, by ID or name
    pub toolset: String,
    /// The tag (key=value) that adds every agentic tool carrying it in all of the toolset's
    /// groups
    #[clap(required_unless_present = "clear", conflicts_with = "clear")]
    pub selector: Option<ToolSetSelector>,
    /// Remove the toolset's selector
    #[clap(long)]
    pub clear: bool,
}

/// Whether a setting is on or off
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Switch {
    /// Turn the setting on
    On,
    /// Turn the setting off
    Off,
}

/// A command to turn a toolset's read-only filter on or off
#[derive(Parser, Debug)]
pub struct SetReadOnly {
    /// The toolset, by ID or name
    pub toolset: String,
    /// Whether only tools whose version declares no effects are included
    pub state: Switch,
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use thorium::models::ToolSetSelector;

    use super::{Switch, ToolSets};
    use crate::args::{Args, SubCommands};
    use crate::utils::output::OutputFormat;

    /// Parse thorctl arguments into a toolsets command
    ///
    /// # Arguments
    ///
    /// * `args` - The arguments after `thorctl toolsets`
    fn parse(args: &[&str]) -> Result<ToolSets, clap::Error> {
        // prepend the binary and subcommand names
        let mut full = vec!["thorctl", "toolsets"];
        full.extend_from_slice(args);
        // parse them into a toolsets command
        match Args::try_parse_from(full)?.cmd {
            SubCommands::ToolSets(tool_sets) => Ok(tool_sets),
            _ => panic!("expected a toolsets command"),
        }
    }

    /// `toolsets create` takes a name, groups, tools, pins, a selector, and the read-only flag
    #[test]
    fn create_args() {
        // every create flag parses
        let pin = "69ff8820-f688-4968-bd6f-45a4cb0f78a9";
        let ToolSets::Create(cmd) = parse(&[
            "create",
            "ghidra",
            "-g",
            "minerva",
            "--tool",
            "decompile",
            "--pin",
            pin,
            "--selector",
            "toolset=ghidra",
            "--read-only",
        ])
        .unwrap() else {
            panic!("expected toolsets create");
        };
        assert_eq!(cmd.name, "ghidra");
        assert_eq!(cmd.group, vec!["minerva"]);
        assert_eq!(cmd.tool, vec!["decompile"]);
        assert_eq!(cmd.pin.len(), 1);
        assert_eq!(
            cmd.selector,
            Some(ToolSetSelector::new("toolset", "ghidra"))
        );
        assert!(cmd.read_only && !cmd.no_auto_instance);
        // a group is required and a selector needs an equals sign
        assert!(parse(&["create", "x"]).is_err());
        assert!(parse(&["create", "x", "-g", "a", "--selector", "toolset"]).is_err());
    }

    /// `toolsets get` and `describe` take groups, a toolset, and the output flags
    #[test]
    fn get_and_describe_args() {
        // get takes groups and a format
        let ToolSets::Get(cmd) = parse(&["get", "-g", "minerva", "-o", "json"]).unwrap() else {
            panic!("expected toolsets get");
        };
        assert_eq!(cmd.group, vec!["minerva"]);
        assert_eq!(cmd.format.format, Some(OutputFormat::Json));
        // describe takes a toolset
        let ToolSets::Describe(cmd) = parse(&["describe", "ghidra"]).unwrap() else {
            panic!("expected toolsets describe");
        };
        assert_eq!(cmd.toolset, "ghidra");
    }

    /// The edit commands take exactly what they change
    #[test]
    fn edit_args() {
        // include with and without a pin
        let ToolSets::Include(cmd) = parse(&["include", "ghidra", "decompile"]).unwrap() else {
            panic!("expected toolsets include");
        };
        assert!(cmd.pin.is_none());
        assert!(parse(&["include", "ghidra", "decompile", "--pin", "nope"]).is_err());
        // remove-tool names a toolset and a tool
        let ToolSets::RemoveTool(cmd) = parse(&["remove-tool", "ghidra", "decompile"]).unwrap()
        else {
            panic!("expected toolsets remove-tool");
        };
        assert_eq!(cmd.tool, "decompile");
        // selector takes a pair or --clear, but not both and not neither
        let ToolSets::Selector(cmd) = parse(&["selector", "ghidra", "suite=re"]).unwrap() else {
            panic!("expected toolsets selector");
        };
        assert_eq!(cmd.selector, Some(ToolSetSelector::new("suite", "re")));
        let ToolSets::Selector(cmd) = parse(&["selector", "ghidra", "--clear"]).unwrap() else {
            panic!("expected toolsets selector");
        };
        assert!(cmd.clear && cmd.selector.is_none());
        assert!(parse(&["selector", "ghidra"]).is_err());
        assert!(parse(&["selector", "ghidra", "a=b", "--clear"]).is_err());
        assert!(parse(&["selector", "ghidra", "suite"]).is_err());
        // read-only takes on or off
        let ToolSets::ReadOnly(cmd) = parse(&["read-only", "ghidra", "on"]).unwrap() else {
            panic!("expected toolsets read-only");
        };
        assert_eq!(cmd.state, Switch::On);
        assert!(parse(&["read-only", "ghidra", "maybe"]).is_err());
    }
}
