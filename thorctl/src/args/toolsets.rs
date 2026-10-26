//! Arguments for toolset Thorctl commands

use clap::{Parser, ValueEnum};
use std::path::PathBuf;
use thorium::models::ToolSetSelector;
use uuid::Uuid;

use super::helpers::{EditArgs, FormatArgs, ShortFormatArgs, WaitForArgs, WatchArgs};

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
    /// Change a toolset with flags, a spec file, or `--set`, `--unset` and `--patch`
    #[clap(version, author)]
    Update(UpdateToolSet),
    /// Edit a toolset in your editor, or with `--file`, `--set`, `--unset` and `--patch`
    #[clap(version, author)]
    Edit(EditToolSet),
    /// Print a toolset as a spec that `toolsets update --file` reads back
    #[clap(version, author)]
    Spec(SpecToolSet),
    /// Delete a toolset
    #[clap(version, author)]
    Delete(DeleteToolSet),
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
    #[clap(required_unless_present = "spec")]
    pub name: Option<String>,
    /// The groups to create the toolset in
    #[clap(short, long, required_unless_present = "spec")]
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
    /// Create the toolset from a YAML or JSON spec, as `toolsets spec` prints it (- reads
    /// stdin); flags replace what the spec sets
    #[clap(long, value_name = "FILE|-")]
    pub spec: Option<PathBuf>,
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

/// A command to change a toolset without an editor
#[derive(Parser, Debug)]
pub struct UpdateToolSet {
    /// The toolset to update, by ID or name
    pub toolset: String,
    /// Rename the toolset
    #[clap(long)]
    pub rename: Option<String>,
    /// Set the toolset's description
    #[clap(short, long, conflicts_with = "clear_description")]
    pub description: Option<String>,
    /// Remove the toolset's description
    #[clap(long)]
    pub clear_description: bool,
    /// Add the toolset to a group; repeatable
    #[clap(long, conflicts_with = "file")]
    pub add_group: Vec<String>,
    /// Remove the toolset from a group; repeatable
    #[clap(long, conflicts_with = "file")]
    pub remove_group: Vec<String>,
    /// Build new toolset instances automatically when the toolset or its tools change
    #[clap(long, value_enum, value_name = "on|off")]
    pub auto_instance: Option<Switch>,
    /// Add the toolset's groups to the tools that miss some of them
    #[clap(long)]
    pub extend_target_groups: bool,
    /// The shared edit flags
    #[clap(flatten)]
    pub edit: EditArgs,
}

/// A command to edit a toolset
#[derive(Parser, Debug)]
pub struct EditToolSet {
    /// The toolset to edit, by ID or name
    pub toolset: String,
    /// Add the toolset's groups to the tools that miss some of them
    #[clap(long)]
    pub extend_target_groups: bool,
    /// The shared edit flags
    #[clap(flatten)]
    pub edit: EditArgs,
}

/// A command to print a toolset as a spec
#[derive(Parser, Debug)]
pub struct SpecToolSet {
    /// The toolset to print, by ID or name
    pub toolset: String,
    /// Write the spec to this file instead of stdout
    #[clap(short, long)]
    pub output: Option<PathBuf>,
    /// The format to print the spec in: yaml (the default) or json
    #[clap(flatten)]
    pub format: FormatArgs,
}

/// A command to delete a toolset
#[derive(Parser, Debug)]
pub struct DeleteToolSet {
    /// The toolset to delete, by ID or name
    pub toolset: String,
    /// Delete without asking for confirmation
    #[clap(long, visible_alias = "yes")]
    pub skip_confirm: bool,
    /// Print what would be deleted and stop
    #[clap(long)]
    pub dry_run: bool,
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
        assert_eq!(cmd.name.as_deref(), Some("ghidra"));
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

    /// `toolsets create --spec` supplies what the flags don't, and needs no name or group flags
    #[test]
    fn create_spec_args() {
        // a spec alone is enough
        let ToolSets::Create(cmd) = parse(&["create", "--spec", "kit.yaml"]).unwrap() else {
            panic!("expected toolsets create");
        };
        assert_eq!(
            cmd.spec
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
            Some("kit.yaml".to_owned())
        );
        assert!(cmd.name.is_none() && cmd.group.is_empty());
        // a spec on stdin, with flags that override it
        let ToolSets::Create(cmd) = parse(&[
            "create", "kit", "--spec", "-", "-g", "minerva", "-d", "text",
        ])
        .unwrap() else {
            panic!("expected toolsets create");
        };
        assert_eq!(cmd.name.as_deref(), Some("kit"));
        assert_eq!(cmd.group, vec!["minerva"]);
        assert_eq!(cmd.description.as_deref(), Some("text"));
        // without a spec the name and a group are still required
        assert!(parse(&["create", "-g", "minerva"]).is_err());
        assert!(parse(&["create", "kit"]).is_err());
    }

    /// `toolsets delete` takes a toolset and the usual confirmation flags
    #[test]
    fn delete_args() {
        // the toolset alone asks for confirmation
        let ToolSets::Delete(cmd) = parse(&["delete", "ghidra"]).unwrap() else {
            panic!("expected toolsets delete");
        };
        assert_eq!(cmd.toolset, "ghidra");
        assert!(!cmd.skip_confirm && !cmd.dry_run);
        // --yes is an alias of --skip-confirm
        let ToolSets::Delete(cmd) = parse(&["delete", "ghidra", "--yes", "--dry-run"]).unwrap()
        else {
            panic!("expected toolsets delete");
        };
        assert!(cmd.skip_confirm && cmd.dry_run);
        assert!(parse(&["delete", "ghidra", "--skip-confirm"]).is_ok());
        // a toolset is required
        assert!(parse(&["delete"]).is_err());
    }

    /// `toolsets update` takes the typed flags and the shared edit flags
    #[test]
    fn update_args() {
        // the typed flags and the edit flags parse together
        let ToolSets::Update(cmd) = parse(&[
            "update",
            "ghidra",
            "--rename",
            "ghidra2",
            "-d",
            "reverse engineering",
            "--add-group",
            "mars",
            "--remove-group",
            "minerva",
            "--auto-instance",
            "off",
            "--set",
            "read_only=true",
            "--dry-run",
            "--yes",
            "-o",
            "json",
            "--extend-target-groups",
        ])
        .unwrap() else {
            panic!("expected toolsets update");
        };
        assert_eq!(cmd.toolset, "ghidra");
        assert_eq!(cmd.rename.as_deref(), Some("ghidra2"));
        assert_eq!(cmd.description.as_deref(), Some("reverse engineering"));
        assert_eq!(cmd.add_group, vec!["mars"]);
        assert_eq!(cmd.remove_group, vec!["minerva"]);
        assert_eq!(cmd.auto_instance, Some(Switch::Off));
        assert_eq!(cmd.edit.ops.len(), 1);
        assert!(cmd.edit.dry_run && cmd.edit.skip_confirm && cmd.extend_target_groups);
        assert_eq!(cmd.edit.format, Some(OutputFormat::Json));
        // the description can be cleared, and contradictions are refused
        let ToolSets::Update(cmd) = parse(&["update", "ghidra", "--clear-description"]).unwrap()
        else {
            panic!("expected toolsets update");
        };
        assert!(cmd.clear_description);
        assert!(parse(&["update", "ghidra", "-d", "x", "--clear-description"]).is_err());
        // group changes are relative to the current groups, which --file replaces
        assert!(parse(&["update", "ghidra", "--add-group", "g", "--file", "f.yaml"]).is_err());
        assert!(
            parse(&[
                "update",
                "ghidra",
                "--remove-group",
                "g",
                "--file",
                "f.yaml"
            ])
            .is_err()
        );
        // auto instance takes on or off
        assert!(parse(&["update", "ghidra", "--auto-instance", "maybe"]).is_err());
    }

    /// `toolsets edit` takes the shared edit flags, and `spec` an output path and a format
    #[test]
    fn edit_and_spec_args() {
        // edit takes the shared edit flags
        let ToolSets::Edit(cmd) = parse(&[
            "edit",
            "ghidra",
            "--editor",
            "vim",
            "--patch",
            "{}",
            "--force",
            "--extend-target-groups",
        ])
        .unwrap() else {
            panic!("expected toolsets edit");
        };
        assert_eq!(cmd.toolset, "ghidra");
        assert_eq!(cmd.edit.editor.as_deref(), Some("vim"));
        assert!(cmd.edit.force && cmd.extend_target_groups);
        assert_eq!(cmd.edit.patch, vec!["{}"]);
        // spec takes an output path and a format
        let ToolSets::Spec(cmd) =
            parse(&["spec", "ghidra", "-o", "t.yaml", "--format", "json"]).unwrap()
        else {
            panic!("expected toolsets spec");
        };
        assert_eq!(cmd.toolset, "ghidra");
        assert_eq!(
            cmd.output.map(|path| path.to_string_lossy().into_owned()),
            Some("t.yaml".to_owned())
        );
        assert_eq!(cmd.format.format, Some(OutputFormat::Json));
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
