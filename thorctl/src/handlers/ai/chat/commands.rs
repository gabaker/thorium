//! The slash commands usable from the chat prompt.
//!
//! Slash commands are actions (unlike the space menu, which changes what/how
//! the TUI displays things): a prompt starting with `/` is parsed here instead
//! of being sent to the AI. The registry drives both the autocomplete popup and
//! parsing, so adding a command means adding it to [`SLASH_COMMANDS`] and
//! handling its [`ParsedCommand`] variant.

use std::path::PathBuf;

/// A single slash command's registry entry
pub struct SlashCommand {
    /// The command's name, without the leading slash
    pub name: &'static str,
    /// The command's usage string shown in the autocomplete popup
    pub usage: &'static str,
    /// A short description of what this command does
    pub description: &'static str,
}

/// Every slash command the chat prompt supports
pub const SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "report",
        usage: "/report <path> [template]",
        description: "Write an LLM-generated markdown report",
    },
    SlashCommand {
        name: "promote",
        usage: "/promote <script> <description…>",
        description: "Promote a session script into the persistent library",
    },
    SlashCommand {
        name: "btw",
        usage: "/btw <name>",
        description: "Open or switch to a named side conversation",
    },
    SlashCommand {
        name: "done",
        usage: "/done",
        description: "Summarize this side conversation back to the main chat",
    },
    SlashCommand {
        name: "close",
        usage: "/close",
        description: "Close this side conversation without returning anything",
    },
    SlashCommand {
        name: "retry",
        usage: "/retry",
        description: "Probe the provider now while the session is paused",
    },
    SlashCommand {
        name: "abort",
        usage: "/abort",
        description: "Abort the provider pause; parked requests surface errors",
    },
];

/// A successfully parsed slash command, ready to act on
#[derive(Debug)]
pub enum ParsedCommand {
    /// Generate a markdown report and write it to a path
    Report {
        /// Where to write the generated report
        path: PathBuf,
        /// A template path overriding the configured and builtin defaults
        template: Option<PathBuf>,
    },
    /// Promote a session script into the persistent cross-session library
    Promote {
        /// The bare name of the session script to promote
        name: String,
        /// What the script does and when a future run should use it
        description: String,
    },
    /// Open (or switch to) a named side conversation
    Btw {
        /// The side conversation's name
        name: String,
    },
    /// Summarize the current side conversation back to the main chat
    Done,
    /// Close the current side conversation without returning anything
    Close,
    /// Probe the paused provider now instead of waiting out the backoff
    Retry,
    /// Abort the provider pause so parked requests surface their errors
    Abort,
}

/// Parses a submitted prompt as a slash command
///
/// The input must start with `/`; the command name and its arguments are split
/// on whitespace. An unknown command or a wrong argument count returns a
/// user-facing error message carrying the command's usage.
///
/// # Arguments
///
/// * `input` - The submitted prompt text, starting with `/`
pub fn parse(input: &str) -> Result<ParsedCommand, String> {
    // split the input into the command name and its arguments
    let mut parts = input.trim_start_matches('/').split_whitespace();
    // an empty command name cannot match anything
    let name = parts.next().unwrap_or_default();
    // collect the remaining whitespace-separated arguments
    let args: Vec<&str> = parts.collect();
    // dispatch on the command name
    match name {
        // /report <path> [template]
        "report" => match args.as_slice() {
            // a path alone uses the configured or builtin template
            [path] => Ok(ParsedCommand::Report {
                path: PathBuf::from(path),
                template: None,
            }),
            // a second argument overrides the template
            [path, template] => Ok(ParsedCommand::Report {
                path: PathBuf::from(path),
                template: Some(PathBuf::from(template)),
            }),
            // anything else is a usage error
            _ => Err("usage: /report <path> [template]".to_string()),
        },
        // /promote <script> <description…>
        "promote" => match args.as_slice() {
            // the script name plus a non-empty free-text description
            [name, description @ ..] if !description.is_empty() => Ok(ParsedCommand::Promote {
                name: (*name).to_string(),
                // the description is everything after the name, re-joined
                description: description.join(" "),
            }),
            // a missing name or description is a usage error
            _ => Err("usage: /promote <script> <description…>".to_string()),
        },
        // /btw <name>
        "btw" => match args.as_slice() {
            // exactly one name, validated with the same rules the store applies
            [name] => match thorpal::validate_btw_name(name) {
                Ok(()) => Ok(ParsedCommand::Btw {
                    name: (*name).to_string(),
                }),
                // an invalid name surfaces the store's own explanation
                Err(msg) => Err(msg),
            },
            // a missing or multi-word name is a usage error
            _ => Err("usage: /btw <name>".to_string()),
        },
        // /done takes no arguments
        "done" => match args.as_slice() {
            [] => Ok(ParsedCommand::Done),
            _ => Err("usage: /done".to_string()),
        },
        // /close takes no arguments
        "close" => match args.as_slice() {
            [] => Ok(ParsedCommand::Close),
            _ => Err("usage: /close".to_string()),
        },
        // /retry takes no arguments
        "retry" => match args.as_slice() {
            [] => Ok(ParsedCommand::Retry),
            _ => Err("usage: /retry".to_string()),
        },
        // /abort takes no arguments
        "abort" => match args.as_slice() {
            [] => Ok(ParsedCommand::Abort),
            _ => Err("usage: /abort".to_string()),
        },
        // an unrecognized command lists what is available
        unknown => {
            // render the available command names for the error message
            let available = SLASH_COMMANDS
                .iter()
                .map(|cmd| format!("/{}", cmd.name))
                .collect::<Vec<_>>()
                .join(", ");
            Err(format!(
                "unknown command '/{unknown}'; available: {available}"
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A report command with just a path parses with no template
    #[test]
    fn parse_report_path_only() {
        // parse a report command carrying only a target path
        match parse("/report out.md") {
            Ok(ParsedCommand::Report { path, template }) => {
                assert_eq!(path, PathBuf::from("out.md"));
                assert!(template.is_none());
            }
            other => panic!("expected a parsed report command: {other:?}"),
        }
    }

    /// A report command with a template parses both paths
    #[test]
    fn parse_report_with_template() {
        // parse a report command carrying a path and a template
        match parse("/report out.md custom.md") {
            Ok(ParsedCommand::Report { path, template }) => {
                assert_eq!(path, PathBuf::from("out.md"));
                assert_eq!(template, Some(PathBuf::from("custom.md")));
            }
            other => panic!("expected a parsed report command: {other:?}"),
        }
    }

    /// A report command without a path is a usage error
    #[test]
    fn parse_report_missing_path() {
        // a bare /report should surface its usage string
        let err = parse("/report").unwrap_err();
        assert!(err.contains("usage: /report"));
    }

    /// A report command with too many arguments is a usage error
    #[test]
    fn parse_report_too_many_args() {
        // extra arguments should surface the usage string
        let err = parse("/report a b c").unwrap_err();
        assert!(err.contains("usage: /report"));
    }

    /// An unknown command lists the available commands
    #[test]
    fn parse_unknown_command() {
        // an unknown name should name itself and list what exists
        let err = parse("/bogus").unwrap_err();
        assert!(err.contains("'/bogus'"));
        assert!(err.contains("/report"));
        assert!(err.contains("/promote"));
    }

    /// A btw command parses its validated name
    #[test]
    fn parse_btw_with_name() {
        // a simple lowercase name parses into its variant
        match parse("/btw crypto") {
            Ok(ParsedCommand::Btw { name }) => assert_eq!(name, "crypto"),
            other => panic!("expected a parsed btw command: {other:?}"),
        }
    }

    /// A btw command without a name, with extras, or with a bad name errors
    #[test]
    fn parse_btw_rejects_bad_names() {
        // a missing name or extra arguments surface the usage string
        assert!(parse("/btw").unwrap_err().contains("usage: /btw"));
        assert!(parse("/btw a b").unwrap_err().contains("usage: /btw"));
        // the reserved main name and invalid characters surface the store's rules
        assert!(parse("/btw main").unwrap_err().contains("reserved"));
        assert!(parse("/btw Crypto").unwrap_err().contains("lowercase"));
    }

    /// The done and close commands take no arguments
    #[test]
    fn parse_done_and_close() {
        // the bare forms parse into their variants
        assert!(matches!(parse("/done"), Ok(ParsedCommand::Done)));
        assert!(matches!(parse("/close"), Ok(ParsedCommand::Close)));
        // any argument is a usage error
        assert!(parse("/done now").unwrap_err().contains("usage: /done"));
        assert!(parse("/close now").unwrap_err().contains("usage: /close"));
    }

    /// A promote command parses its name and re-joined description
    #[test]
    fn parse_promote_with_description() {
        // the description is everything after the script name
        match parse("/promote find-xor finds single-byte XOR stubs") {
            Ok(ParsedCommand::Promote { name, description }) => {
                assert_eq!(name, "find-xor");
                assert_eq!(description, "finds single-byte XOR stubs");
            }
            other => panic!("expected a parsed promote command: {other:?}"),
        }
    }

    /// A promote command without a description is a usage error
    #[test]
    fn parse_promote_missing_description() {
        // the description is required so the library stays searchable
        let err = parse("/promote find-xor").unwrap_err();
        assert!(err.contains("usage: /promote"));
        // a bare /promote surfaces the same usage string
        let err = parse("/promote").unwrap_err();
        assert!(err.contains("usage: /promote"));
    }
}
