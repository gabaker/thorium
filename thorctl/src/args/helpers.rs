//! Helper functions and shared arguments for Thorctl commands

use std::str::FromStr;
use std::time::Duration;

use crate::utils::output::OutputFormat;
use crate::utils::watch::WaitFor;

/// Provide a possible number range for an argument, producing an error if a user
/// provides a value not within the range
///
/// Inspired by <https://docs.rs/clap-num/latest/clap_num/fn.number_range.html>
///
/// # Arguments
///
/// * `s` - The raw argument str to parse
/// * `min` - The minimum the argument value can be
/// * `max` - The maximum the argument value can be
pub fn number_range<T>(s: &str, min: T, max: T) -> Result<T, String>
where
    T: FromStr + Copy + Ord + PartialOrd + std::fmt::Display,
    <T as FromStr>::Err: std::fmt::Display,
{
    debug_assert!(min <= max, "minimum of {min} exceeds maximum of {max}");
    let val = s.parse::<T>().map_err(|err| err.to_string())?;
    if val > max {
        Err(format!("exceeds maximum of {max}"))
    } else if val < min {
        Err(format!("less than minimum of {min}"))
    } else {
        Ok(val)
    }
}

/// Parse a duration such as `500ms`, `30s`, `10m`, or `1h`; a bare number is in seconds
///
/// # Arguments
///
/// * `raw` - The raw duration to parse
pub fn parse_duration(raw: &str) -> Result<Duration, String> {
    // ignore any surrounding whitespace
    let trimmed = raw.trim();
    // split our duration into its number and its unit
    let split = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(trimmed.len());
    let (number, unit) = trimmed.split_at(split);
    // parse the number of units
    let count = number
        .parse::<u64>()
        .map_err(|_| format!("'{raw}' is not a duration like 500ms, 30s, 10m, or 1h"))?;
    // convert our count to a duration based on its unit
    match unit {
        "ms" => Ok(Duration::from_millis(count)),
        "" | "s" => Ok(Duration::from_secs(count)),
        "m" => Ok(Duration::from_secs(count.saturating_mul(60))),
        "h" => Ok(Duration::from_secs(count.saturating_mul(3600))),
        _ => Err(format!(
            "'{raw}' has an unknown unit '{unit}'; use ms, s, m, or h"
        )),
    }
}

/// Parse the interval between re-reads, which must be longer than zero
///
/// A zero interval would re-read as fast as the API answers.
///
/// # Arguments
///
/// * `raw` - The raw interval to parse
pub fn parse_interval(raw: &str) -> Result<Duration, String> {
    // parse the interval like any other duration
    let interval = parse_duration(raw)?;
    // refuse an interval that would never wait
    if interval.is_zero() {
        return Err(format!("'{raw}' is not longer than zero"));
    }
    Ok(interval)
}

/// The output format flag for commands that already use `-o` for an output path
#[derive(clap::Args, Debug, Clone, Default)]
pub struct FormatArgs {
    /// The format to print in: table, human, json (the API's response), or yaml
    #[clap(long, value_enum, ignore_case = true)]
    pub format: Option<OutputFormat>,
}

/// The output format flag, with `-o` as its short name, for commands without an output path
#[derive(clap::Args, Debug, Clone, Default)]
pub struct ShortFormatArgs {
    /// The format to print in: table, human, json (the API's response), or yaml
    #[clap(short = 'o', long, value_enum, ignore_case = true)]
    pub format: Option<OutputFormat>,
}

/// The `--watch` flag for re-rendering a view until interrupted
#[derive(clap::Args, Debug, Clone)]
pub struct WatchArgs {
    /// Re-read and re-print until interrupted
    #[clap(long)]
    pub watch: bool,
    /// How long to wait between re-reads with `--watch` or `--wait-for` (e.g. 500ms, 2s, 1m)
    #[clap(long, default_value = "2s", value_parser = parse_interval)]
    pub interval: Duration,
}

/// The `--wait-for` flag for polling a view until a condition holds
#[derive(clap::Args, Debug, Clone)]
pub struct WaitForArgs {
    /// Poll until a condition holds, then print once; exits 3 on timeout
    ///
    /// A condition is `<field>=<value>` (e.g. status=Completed) or a bare `<field>` that must be set
    #[clap(long, value_name = "CONDITION", conflicts_with = "watch")]
    pub wait_for: Option<WaitFor>,
    /// How long `--wait-for` polls before giving up (e.g. 30s, 10m, 1h)
    #[clap(long, default_value = "10m", value_parser = parse_duration)]
    pub timeout: Duration,
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use clap::Parser;

    use super::{
        FormatArgs, ShortFormatArgs, WaitForArgs, WatchArgs, parse_duration, parse_interval,
    };
    use crate::args::reactions::Reactions;
    use crate::args::results::Results;
    use crate::args::tokens::Tokens;
    use crate::args::{Args, SubCommands};
    use crate::utils::output::OutputFormat;

    /// A test command using the flags for commands with an output path
    #[derive(Parser, Debug)]
    struct PathCommand {
        /// The output path
        #[clap(short, long)]
        output: Option<String>,
        /// The format flag
        #[clap(flatten)]
        format: FormatArgs,
        /// The watch flag
        #[clap(flatten)]
        watch: WatchArgs,
        /// The wait-for flag
        #[clap(flatten)]
        wait: WaitForArgs,
    }

    /// A test command using the flags for commands without an output path
    #[derive(Parser, Debug)]
    struct NewCommand {
        /// The format flag with its short name
        #[clap(flatten)]
        format: ShortFormatArgs,
    }

    /// A reaction id the view tests target
    const ID: &str = "69ff8820-f688-4968-bd6f-45a4cb0f78a9";

    /// `reactions describe` keeps `-o` as its output path and takes `--format`,
    /// `--wait-for`, and `--timeout`
    #[test]
    fn reactions_describe_flags() {
        // target our shared reaction id
        let id = ID;
        // reactions describe keeps -o as its output path
        let args = Args::try_parse_from([
            "thorctl",
            "reactions",
            "describe",
            id,
            "-o",
            "out.yaml",
            "--format",
            "yaml",
        ])
        .unwrap();
        let SubCommands::Reactions(Reactions::Describe(cmd)) = args.cmd else {
            panic!("expected reactions describe");
        };
        assert_eq!(cmd.output.unwrap().to_string_lossy(), "out.yaml");
        assert_eq!(cmd.format.format, Some(OutputFormat::Yaml));
        // reactions describe takes --wait-for and --timeout
        let args = Args::try_parse_from([
            "thorctl",
            "reactions",
            "describe",
            id,
            "--wait-for",
            "status=Completed",
            "--timeout",
            "5s",
        ])
        .unwrap();
        let SubCommands::Reactions(Reactions::Describe(cmd)) = args.cmd else {
            panic!("expected reactions describe");
        };
        assert!(cmd.wait.wait_for.is_some());
        assert_eq!(cmd.wait.timeout, Duration::from_secs(5));
        // condensed is a JSON modifier so it excludes --format
        assert!(
            Args::try_parse_from([
                "thorctl",
                "reactions",
                "describe",
                id,
                "--condensed",
                "--format",
                "yaml"
            ])
            .is_err()
        );
    }

    /// `reactions logs` keeps `-o` as its output directory and takes `--format` and
    /// `--watch`, and `reactions get` takes every helper flag
    #[test]
    fn reactions_logs_and_get_flags() {
        // target our shared reaction id
        let id = ID;
        // reactions logs keeps -o as its output directory and takes --watch
        let args = Args::try_parse_from([
            "thorctl",
            "reactions",
            "logs",
            id,
            "-o",
            "logs",
            "--format",
            "json",
        ])
        .unwrap();
        let SubCommands::Reactions(Reactions::Logs(cmd)) = args.cmd else {
            panic!("expected reactions logs");
        };
        assert_eq!(cmd.output.unwrap().to_string_lossy(), "logs");
        assert_eq!(cmd.format.format, Some(OutputFormat::Json));
        let args = Args::try_parse_from(["thorctl", "reactions", "logs", id, "--watch"]).unwrap();
        let SubCommands::Reactions(Reactions::Logs(cmd)) = args.cmd else {
            panic!("expected reactions logs");
        };
        assert!(cmd.watch.watch);
        // reactions get takes every helper flag
        let args = Args::try_parse_from([
            "thorctl",
            "reactions",
            "get",
            "-g",
            "corn",
            id,
            "--format",
            "table",
            "--watch",
        ])
        .unwrap();
        let SubCommands::Reactions(Reactions::Get(cmd)) = args.cmd else {
            panic!("expected reactions get");
        };
        assert_eq!(cmd.format.format, Some(OutputFormat::Table));
        assert!(cmd.watch.watch);
    }

    /// `results get` keeps `-o` as its output directory, the token views take `--format`
    /// without `--show-token`, and the existing describe formats are unchanged
    #[test]
    fn results_and_tokens_flags() {
        // results get keeps -o as its output directory
        let args = Args::try_parse_from([
            "thorctl", "results", "get", "--files", "abc", "-o", "dir", "--format", "json",
        ])
        .unwrap();
        let SubCommands::Results(Results::Get(cmd)) = args.cmd else {
            panic!("expected results get");
        };
        assert_eq!(cmd.output, "dir");
        assert_eq!(cmd.format.format, Some(OutputFormat::Json));
        // tokens get and describe take the format flag, and showing tokens excludes it
        let args = Args::try_parse_from(["thorctl", "tokens", "get", "--format", "yaml"]).unwrap();
        let SubCommands::Tokens(Tokens::Get(cmd)) = args.cmd else {
            panic!("expected tokens get");
        };
        assert_eq!(cmd.format.format, Some(OutputFormat::Yaml));
        let args =
            Args::try_parse_from(["thorctl", "tokens", "describe", "--format", "human"]).unwrap();
        let SubCommands::Tokens(Tokens::Describe(cmd)) = args.cmd else {
            panic!("expected tokens describe");
        };
        assert_eq!(cmd.format.format, Some(OutputFormat::Human));
        for flag in ["--format=json", "--watch", "--wait-for=name=x"] {
            assert!(
                Args::try_parse_from(["thorctl", "tokens", "describe", "--show-token", flag])
                    .is_err(),
                "--show-token should exclude {flag}"
            );
        }
        // the existing describe formats are unchanged
        assert!(
            Args::try_parse_from(["thorctl", "images", "describe", "img", "--format", "yaml"])
                .is_err()
        );
    }

    /// Durations parse with and without units
    #[test]
    fn durations() {
        assert_eq!(parse_duration("500ms"), Ok(Duration::from_millis(500)));
        assert_eq!(parse_duration("30"), Ok(Duration::from_secs(30)));
        assert_eq!(parse_duration("30s"), Ok(Duration::from_secs(30)));
        assert_eq!(parse_duration("10m"), Ok(Duration::from_secs(600)));
        assert_eq!(parse_duration("1h"), Ok(Duration::from_secs(3600)));
        assert!(parse_duration("ten").is_err());
        assert!(parse_duration("10d").is_err());
        // an interval must be longer than zero
        assert_eq!(parse_interval("200ms"), Ok(Duration::from_millis(200)));
        assert!(parse_interval("0").is_err());
        assert!(parse_interval("0ms").is_err());
        // the flag refuses a zero interval and sets the period of --wait-for too
        assert!(PathCommand::try_parse_from(["test", "--watch", "--interval", "0"]).is_err());
        let cmd = PathCommand::try_parse_from(["test", "--wait-for", "status", "--interval", "1s"])
            .unwrap();
        assert_eq!(cmd.watch.interval, Duration::from_secs(1));
    }

    /// The flags default to no format, no watch, a 2 second interval, and a 10 minute
    /// timeout
    #[test]
    fn defaults() {
        let cmd = PathCommand::try_parse_from(["test"]).unwrap();
        assert_eq!(cmd.format.format, None);
        assert!(!cmd.watch.watch);
        assert_eq!(cmd.watch.interval, Duration::from_secs(2));
        assert!(cmd.wait.wait_for.is_none());
        assert_eq!(cmd.wait.timeout, Duration::from_secs(600));
    }

    /// On a command with a path flag, `-o` is the path and `--format` is the format
    #[test]
    fn output_path_and_format() {
        let cmd =
            PathCommand::try_parse_from(["test", "-o", "out.json", "--format", "yaml"]).unwrap();
        assert_eq!(cmd.output.as_deref(), Some("out.json"));
        assert_eq!(cmd.format.format, Some(OutputFormat::Yaml));
        // every format parses
        for (raw, format) in [
            ("table", OutputFormat::Table),
            ("human", OutputFormat::Human),
            ("json", OutputFormat::Json),
            ("YAML", OutputFormat::Yaml),
        ] {
            let cmd = PathCommand::try_parse_from(["test", "--format", raw]).unwrap();
            assert_eq!(cmd.format.format, Some(format));
        }
        // unknown formats are rejected
        assert!(PathCommand::try_parse_from(["test", "--format", "xml"]).is_err());
    }

    /// On a new command, `-o json` equals `--format json`
    #[test]
    fn short_format() {
        let short = NewCommand::try_parse_from(["test", "-o", "json"]).unwrap();
        let long = NewCommand::try_parse_from(["test", "--format", "json"]).unwrap();
        assert_eq!(short.format.format, Some(OutputFormat::Json));
        assert_eq!(short.format.format, long.format.format);
    }

    /// `--watch`, `--wait-for`, `--interval`, and `--timeout` parse, and watch excludes
    /// wait-for
    #[test]
    fn watch_and_wait_for() {
        let cmd = PathCommand::try_parse_from(["test", "--watch", "--interval", "250ms"]).unwrap();
        assert!(cmd.watch.watch);
        assert_eq!(cmd.watch.interval, Duration::from_millis(250));
        let cmd = PathCommand::try_parse_from([
            "test",
            "--wait-for",
            "status=Completed",
            "--timeout",
            "30s",
        ])
        .unwrap();
        let cond = cmd.wait.wait_for.unwrap();
        assert_eq!(cond.path, vec!["status"]);
        assert_eq!(cond.expected.as_deref(), Some("Completed"));
        assert_eq!(cmd.wait.timeout, Duration::from_secs(30));
        // a malformed condition is rejected
        assert!(PathCommand::try_parse_from(["test", "--wait-for", "=x"]).is_err());
        // watching and waiting are exclusive
        assert!(
            PathCommand::try_parse_from(["test", "--watch", "--wait-for", "status=Completed"])
                .is_err()
        );
    }
}
