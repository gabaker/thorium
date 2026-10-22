//! Arguments for instance Thorctl commands

use clap::{Parser, ValueEnum};

use super::helpers::{ShortFormatArgs, WaitForArgs, WatchArgs};

/// The commands to send to the instances task handler
#[derive(Parser, Debug)]
pub enum Instances {
    /// List the instances of a tool, toolset, or workflow
    #[clap(version, author)]
    Get(GetInstances),
    /// Describe a tool, toolset, or workflow instance
    #[clap(version, author)]
    Describe(DescribeInstance),
    /// Show the fields that differ between two instances of the same kind
    #[clap(version, author)]
    Diff(DiffInstances),
}

/// The kind of root whose instances to list
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstanceRoot {
    /// A tool, whose instances are its versions
    Tool,
    /// A toolset
    Toolset,
    /// A workflow
    Workflow,
}

/// A command to list the instances of a root
#[derive(Parser, Debug)]
pub struct GetInstances {
    /// The kind of root
    #[clap(value_enum)]
    pub kind: InstanceRoot,
    /// The root, by ID or name
    pub root: String,
    /// The format to print the instances in (a table by default)
    #[clap(flatten)]
    pub format: ShortFormatArgs,
    /// Re-read and re-print the instances until interrupted
    #[clap(flatten)]
    pub watch: WatchArgs,
    /// Wait until the listing matches a condition, such as `instance-revision=2`
    #[clap(flatten)]
    pub wait: WaitForArgs,
}

/// A command to describe an instance
#[derive(Parser, Debug)]
pub struct DescribeInstance {
    /// The instance, by ID or name
    pub instance: String,
    /// The format to describe the instance in (human readable by default)
    #[clap(flatten)]
    pub format: ShortFormatArgs,
    /// Re-read and re-describe the instance until interrupted
    #[clap(flatten)]
    pub watch: WatchArgs,
    /// Wait until the instance matches a condition
    #[clap(flatten)]
    pub wait: WaitForArgs,
}

/// A command to compare two instances
#[derive(Parser, Debug)]
pub struct DiffInstances {
    /// The first instance, by ID or name
    pub first: String,
    /// The second instance, by ID or name
    pub second: String,
    /// The format to print the differences in (a table by default)
    #[clap(flatten)]
    pub format: ShortFormatArgs,
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{InstanceRoot, Instances};
    use crate::args::{Args, SubCommands};

    /// Parse thorctl arguments into an instances command
    ///
    /// # Arguments
    ///
    /// * `args` - The arguments after `thorctl instances`
    fn parse(args: &[&str]) -> Result<Instances, clap::Error> {
        // prepend the binary and subcommand names
        let mut full = vec!["thorctl", "instances"];
        full.extend_from_slice(args);
        // parse them into an instances command
        match Args::try_parse_from(full)?.cmd {
            SubCommands::Instances(instances) => Ok(instances),
            _ => panic!("expected an instances command"),
        }
    }

    /// `instances get` takes a root kind and a root, `describe` an instance, and `diff` two
    #[test]
    fn instance_args() {
        // get takes the kind of root and the root, with a wait condition
        let Instances::Get(cmd) = parse(&[
            "get",
            "workflow",
            "triage",
            "--wait-for",
            "instance-revision=2",
        ])
        .unwrap() else {
            panic!("expected instances get");
        };
        assert_eq!(cmd.kind, InstanceRoot::Workflow);
        assert_eq!(cmd.root, "triage");
        assert!(cmd.wait.wait_for.is_some());
        assert!(parse(&["get", "agent", "triage"]).is_err());
        // describe takes an instance and diff takes two
        let Instances::Describe(cmd) = parse(&["describe", "triage#r1"]).unwrap() else {
            panic!("expected instances describe");
        };
        assert_eq!(cmd.instance, "triage#r1");
        let Instances::Diff(cmd) = parse(&["diff", "triage#r1", "triage#r2"]).unwrap() else {
            panic!("expected instances diff");
        };
        assert_eq!(
            (cmd.first.as_str(), cmd.second.as_str()),
            ("triage#r1", "triage#r2")
        );
        assert!(parse(&["diff", "triage#r1"]).is_err());
    }
}
