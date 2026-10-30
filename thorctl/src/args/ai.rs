//! The arguments for AI features in Thorctl

use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;

/// How long a headless run waits in the paused state before giving up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseWait {
    /// Do not park: abort a provider pause immediately; the parked requests
    /// surface their original errors
    None,
    /// Wait indefinitely for the operator or a successful probe.
    Forever,
    /// Wait at most this long, then exit 75 with a resumable session.
    For(Duration),
}

/// Parse a `--pause-wait` value: a humantime duration, `forever`, or `none`
///
/// # Arguments
///
/// * `value` - The raw flag value to parse
pub(crate) fn parse_pause_wait(value: &str) -> Result<PauseWait, String> {
    // the fail-fast default can also be named explicitly
    if value.eq_ignore_ascii_case("none") || value.eq_ignore_ascii_case("disabled") {
        return Ok(PauseWait::None);
    }
    // the explicit opt-in for unattended waits
    if value.eq_ignore_ascii_case("forever") || value.eq_ignore_ascii_case("infinite") {
        return Ok(PauseWait::Forever);
    }
    // anything else must be a parseable duration like `15m` or `2h30m`
    match humantime::parse_duration(value) {
        // a zero wait is no wait at all: treat it as the fail-fast mode
        Ok(duration) if duration.is_zero() => Ok(PauseWait::None),
        Ok(duration) => Ok(PauseWait::For(duration)),
        Err(err) => Err(format!(
            "expected a duration like '15m'/'1h', 'forever', or 'none': {err}"
        )),
    }
}

/// The commands to perform AI based commands in Thorctl
#[derive(Parser, Debug)]
pub enum Ai {
    /// Chat with ThorPal about a local binary
    Chat(Chat),
    /// Summarize a local binary with ThorPal
    Summary(Summary),
    /// Verify the configured AI provider responds to a trivial hello prompt
    Check,
    /// Explore global usage and cost stats across every saved ThorPal session
    Usage(Usage),
    /// Interactively set up ThorPal: LLM provider credentials, a Ghidra
    /// install, and the JDK Ghidra needs
    Setup(Setup),
    /// Run pooled headless evaluations and regenerate the evaluation figures
    #[clap(subcommand)]
    Batch(Batch),
    /// Export privacy-safe ThorPal telemetry bundles from saved sessions
    #[clap(subcommand)]
    Telemetry(Telemetry),
}

/// Chat with ThorPal about a local binary
#[derive(Parser, Debug)]
pub struct Chat {
    /// The local binaries to chat about: the first is the primary sample the
    /// Ghidra project is created for, and any further paths are sibling
    /// samples (a dropper's payload, a bot's plugins) imported into the same
    /// project as additional programs (omit all when resuming a session)
    #[clap(required_unless_present = "resume", conflicts_with = "resume", num_args = 1..)]
    pub binary: Vec<PathBuf>,
    /// The name to save this session under in ~/.thorium/sessions (defaults to
    /// an auto-generated name the model may rename descriptively at the end)
    #[clap(long, conflicts_with = "resume")]
    pub name: Option<String>,
    /// Resume a saved session from ~/.thorium/sessions/<NAME> (the binary,
    /// Ghidra settings, and workflow all come from the saved session)
    #[clap(long, value_name = "NAME")]
    pub resume: Option<String>,
    /// The Ghidra installation directory to analyze with, overriding the `ai`
    /// config (ignored when resuming)
    #[clap(long, env = "GHIDRA_INSTALL")]
    pub ghidra_install: Option<PathBuf>,
    /// The ThorPal workflow to use, overriding the `ai` config and builtin
    /// workflow (ignored when resuming)
    #[clap(long)]
    pub workflow: Option<PathBuf>,
    /// Always create a new Ghidra project for this binary instead of reusing
    /// the most recent one (a resume always reopens its saved project)
    #[clap(long, conflicts_with = "resume")]
    pub fresh_project: bool,
    /// Analyze an anonymized copy of the binary, staged under the Ghidra
    /// projects root and named by its SHA-256, so the model never sees the
    /// original file name or path (the copy is kept for resumes, which always
    /// keep the paths they were built with)
    #[clap(long, conflicts_with = "resume")]
    pub anonymize: bool,
    /// A file of deny tokens (one case-insensitive token per line, `#`
    /// comments allowed) scrubbed out of every model-visible script-library
    /// surface -- names, descriptions, docs, bodies, search hits, run output.
    /// The model never sees the list itself, and it is never persisted into
    /// the session store, so pass it again on every resume of a blinded run
    #[clap(long, value_name = "FILE")]
    pub redact_tokens: Option<PathBuf>,
    /// The family under analysis's name (repeat the flag for each alias):
    /// cross-run `family-intel` memories matching any of these are hidden
    /// entirely from the model, and the names also scrub out of memory text
    /// like deny tokens. The list is eval ground truth: the model never sees
    /// it and it is never persisted into the session store, so pass it again
    /// on every resume of a family-blinded run
    #[clap(long = "blind-family", value_name = "NAME")]
    pub blind_family: Vec<String>,
    /// Disable the static-detector forum pre-seed pass before the agent loop
    #[clap(long)]
    pub disable_detectors: bool,
    /// Disable the analyst-interaction tools (ask_analyst / flag_for_review)
    /// for this session, keeping every agent's tool list and preamble
    /// byte-identical to pre-feature runs (the knob evaluation owners pin for
    /// strict A/B comparability); the gate is saved with the session, so a
    /// resume keeps the same tool surface
    #[clap(long)]
    pub no_analyst_asks: bool,
    /// Write an LLM-generated markdown report to this path once the agent
    /// finishes its analysis (overwritten silently; a copy is always saved in
    /// the session store as report.md)
    #[clap(long)]
    pub report: Option<PathBuf>,
    /// A markdown template describing the report's structure, overriding the
    /// `ai` config's report_template and the builtin default template
    #[clap(long)]
    pub report_template: Option<PathBuf>,
    /// Defang IOCs (hxxp[://]-style schemes, bracketed dots in IPv4s and
    /// URL/domain hostnames) in reports written to disk, for safe transit and
    /// sharing; the session store's report.md copy always stays pristine
    #[clap(long)]
    pub defang: bool,
    /// Run non-interactively (headless): seed the forum, drive the agent loop
    /// to completion with this opening prompt, print plain-text progress to
    /// stderr and the final response to stdout, then exit (0 on success, 1 on
    /// failure, 75 when a provider pause outlives an opted-in --pause-wait,
    /// 130 on an interrupting signal); with --resume, continue the saved
    /// session's conversation headless with this prompt
    #[clap(long, conflicts_with = "prompt_file")]
    pub prompt: Option<String>,
    /// Like --prompt but reads the opening prompt from a file (headless)
    #[clap(long, value_name = "PATH")]
    pub prompt_file: Option<PathBuf>,
    /// Headless: continue a resumed session's agent loop with no new prompt --
    /// the loop reopens on the forum's still-open questions (recovers a run
    /// that crashed or was interrupted partway)
    #[clap(long = "continue", requires = "resume", conflicts_with_all = ["prompt", "prompt_file"])]
    pub continue_run: bool,
    /// Headless: resume a finished session, regenerate its report, write it to
    /// --report, and exit without running the agent loop (recovers a report
    /// lost to a provider fault at the end of a run)
    #[clap(long, requires = "resume", requires = "report")]
    pub report_only: bool,
    /// Headless: how long a paused run (expired credentials, provider outage)
    /// may park waiting for recovery. The default `none` fails fast: the
    /// provider error surfaces within the retry budget and the run exits 1. A
    /// duration like `15m`/`1h` (or `forever`) opts in to parking: while
    /// paused the run probes on a backoff and re-reads the config file when it
    /// changes (so refreshing a token in place resumes the run automatically),
    /// and when the wait expires the run exits 75 with a resumable session
    // release note: the `none` default restores c8257bf's fail-loud behavior
    // (surfaced error within the retry budget, exit 1) -- NOT cb69be0's, which
    // swallowed every iteration error and could exit 0 with an empty response
    #[clap(long, default_value = "none", value_parser = parse_pause_wait)]
    pub pause_wait: PauseWait,
    /// Override the main agent loop's turn cap (default 100)
    #[clap(long)]
    pub max_turns: Option<usize>,
}

impl Chat {
    /// Whether this invocation runs headless (an opening prompt was given, a
    /// resumed loop continuation was requested, or a report-only regeneration)
    pub fn is_headless(&self) -> bool {
        self.prompt.is_some() || self.prompt_file.is_some() || self.continue_run || self.report_only
    }

    /// Validates the flag combinations clap cannot express
    ///
    /// Clap silently drops a `requires` whose target conflicts with a present
    /// positional (here: `--continue` requiring `--resume`, which the binary
    /// positional excludes), so the rule is enforced here and checked by the
    /// dispatcher before any session work starts.
    pub fn validate(&self) -> Result<(), String> {
        // continuing an agent loop only makes sense over a saved session
        if self.continue_run && self.resume.is_none() {
            return Err(
                "--continue needs --resume <NAME> to name the session to continue".to_owned(),
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An opening prompt flips the chat into headless mode
    #[test]
    fn chat_prompt_is_headless() {
        // parse a chat with an inline opening prompt
        let chat = Chat::try_parse_from(["chat", "bin", "--prompt", "hi"]).unwrap();
        assert!(chat.is_headless());
        // parse a plain interactive chat
        let chat = Chat::try_parse_from(["chat", "bin"]).unwrap();
        assert!(!chat.is_headless());
    }

    /// Anonymization only applies when a session is first built, never on resume
    #[test]
    fn chat_anonymize_conflicts_with_resume() {
        // a resume keeps the paths it was built with, so --anonymize is rejected
        assert!(Chat::try_parse_from(["chat", "--resume", "x", "--anonymize"]).is_err());
        // a fresh session accepts the flag
        let chat = Chat::try_parse_from(["chat", "bin", "--anonymize"]).unwrap();
        assert!(chat.anonymize);
    }

    /// The deny-token list rides fresh runs AND resumes (it is never persisted)
    #[test]
    fn chat_redact_tokens_allowed_with_resume() {
        // a fresh blinded run names its token file
        let chat = Chat::try_parse_from(["chat", "bin", "--redact-tokens", "tokens.txt"]).unwrap();
        assert_eq!(chat.redact_tokens, Some(PathBuf::from("tokens.txt")));
        // a resume must be able to re-supply it, unlike --anonymize
        let chat = Chat::try_parse_from(["chat", "--resume", "x", "--redact-tokens", "tokens.txt"])
            .unwrap();
        assert_eq!(chat.redact_tokens, Some(PathBuf::from("tokens.txt")));
        // and it stays optional
        let chat = Chat::try_parse_from(["chat", "bin"]).unwrap();
        assert!(chat.redact_tokens.is_none());
    }

    /// Telemetry export defaults to the content-free level over every session
    #[test]
    fn telemetry_export_defaults_are_level_zero_everything() {
        // a bare export is level 0, every session, interactive, default path
        let Telemetry::Export(export) = Telemetry::try_parse_from(["telemetry", "export"]).unwrap()
        else {
            panic!("expected the export verb");
        };
        assert_eq!(export.level, 0);
        assert!(export.session.is_empty());
        assert!(export.sessions_root.is_none());
        assert!(export.redact_tokens.is_none());
        assert!(export.out.is_none());
        assert!(!export.yes);
    }

    /// Telemetry export accepts only the two defined bundle levels
    #[test]
    fn telemetry_export_levels_are_bounded() {
        // level 1 opts into normalized failure signatures
        let Telemetry::Export(export) =
            Telemetry::try_parse_from(["telemetry", "export", "--level", "1"]).unwrap()
        else {
            panic!("expected the export verb");
        };
        assert_eq!(export.level, 1);
        // anything past the defined tiers is rejected at parse time
        assert!(Telemetry::try_parse_from(["telemetry", "export", "--level", "2"]).is_err());
    }

    /// Telemetry explore takes bundle files, or a sessions root with none
    #[test]
    fn telemetry_explore_takes_bundles_or_a_root() {
        // bare explore walks the local store
        let Telemetry::Explore(explore) =
            Telemetry::try_parse_from(["telemetry", "explore"]).unwrap()
        else {
            panic!("expected the explore verb");
        };
        assert!(explore.bundles.is_empty());
        assert!(explore.sessions_root.is_none());
        // bundle files merge; a copied store swaps the root instead
        let Telemetry::Explore(explore) =
            Telemetry::try_parse_from(["telemetry", "explore", "a.json", "b.json.gz"]).unwrap()
        else {
            panic!("expected the explore verb");
        };
        assert_eq!(explore.bundles.len(), 2);
        // bundles and a sessions root are different sources; both is an error
        assert!(
            Telemetry::try_parse_from([
                "telemetry",
                "explore",
                "a.json",
                "--sessions-root",
                "./copy"
            ])
            .is_err()
        );
    }

    /// Telemetry export sessions repeat per name and headless flags combine
    #[test]
    fn telemetry_export_sessions_repeat_and_combine_headless() {
        // each exported session is its own flag occurrence, like blind-family
        let Telemetry::Export(export) = Telemetry::try_parse_from([
            "telemetry",
            "export",
            "--session",
            "alpha",
            "--session",
            "beta",
            "--sessions-root",
            "./copy",
            "--redact-tokens",
            "deny.txt",
            "--out",
            "bundle.json.gz",
            "--yes",
        ])
        .unwrap() else {
            panic!("expected the export verb");
        };
        assert_eq!(export.session, ["alpha", "beta"]);
        assert_eq!(export.sessions_root, Some(PathBuf::from("./copy")));
        assert_eq!(export.redact_tokens, Some(PathBuf::from("deny.txt")));
        assert_eq!(export.out, Some(PathBuf::from("bundle.json.gz")));
        assert!(export.yes);
    }

    /// The blind-family list repeats per alias and rides resumes too
    #[test]
    fn chat_blind_family_repeats_and_rides_resumes() {
        // each alias of the analyzed family is its own flag occurrence
        let chat = Chat::try_parse_from([
            "chat",
            "bin",
            "--blind-family",
            "SmokeLoader",
            "--blind-family",
            "Dofoil",
        ])
        .unwrap();
        assert_eq!(chat.blind_family, ["SmokeLoader", "Dofoil"]);
        // a resume must be able to re-supply it (it is never persisted)
        let chat = Chat::try_parse_from(["chat", "--resume", "x", "--blind-family", "SmokeLoader"])
            .unwrap();
        assert_eq!(chat.blind_family, ["SmokeLoader"]);
        // and it stays optional
        let chat = Chat::try_parse_from(["chat", "bin"]).unwrap();
        assert!(chat.blind_family.is_empty());
    }

    /// A chat accepts several binaries: a primary plus sibling samples
    #[test]
    fn chat_accepts_multiple_binaries() {
        // one binary stays the common case
        let chat = Chat::try_parse_from(["chat", "a.bin"]).unwrap();
        assert_eq!(chat.binary.len(), 1);
        // extra positionals become sibling samples in order
        let chat = Chat::try_parse_from(["chat", "a.bin", "b.bin", "c.bin"]).unwrap();
        assert_eq!(
            chat.binary,
            vec![
                PathBuf::from("a.bin"),
                PathBuf::from("b.bin"),
                PathBuf::from("c.bin")
            ]
        );
        // binaries still conflict with resuming, however many are given
        assert!(Chat::try_parse_from(["chat", "a.bin", "b.bin", "--resume", "x"]).is_err());
    }

    /// Defanging is opt-in and composes with the report flags
    #[test]
    fn chat_defang_parses() {
        // the flag is off unless asked for
        let chat = Chat::try_parse_from(["chat", "bin"]).unwrap();
        assert!(!chat.defang);
        // it parses alongside --report and alone (the TUI writes reports too)
        let chat = Chat::try_parse_from(["chat", "bin", "--report", "r.md", "--defang"]).unwrap();
        assert!(chat.defang);
        let chat = Chat::try_parse_from(["chat", "bin", "--defang"]).unwrap();
        assert!(chat.defang);
    }

    /// The headless prompt flags compose with resume and conflict with each other
    #[test]
    fn chat_prompt_conflicts() {
        // a resume with an inline prompt is a headless continuation turn
        let chat = Chat::try_parse_from(["chat", "--resume", "x", "--prompt", "hi"]).unwrap();
        assert!(chat.is_headless());
        // a prompt file composes with resume the same way
        let chat =
            Chat::try_parse_from(["chat", "--resume", "x", "--prompt-file", "p.md"]).unwrap();
        assert!(chat.is_headless());
        // only one source of the opening prompt is allowed
        let both = ["chat", "bin", "--prompt", "hi", "--prompt-file", "p.md"];
        assert!(Chat::try_parse_from(both).is_err());
    }

    /// --pause-wait parses durations, the forever sentinel, and fail-fast
    #[test]
    fn chat_pause_wait_parses() {
        // the default is fail-fast: no parking at all
        let chat = Chat::try_parse_from(["chat", "bin"]).unwrap();
        assert_eq!(chat.pause_wait, PauseWait::None);
        // the fail-fast mode can be named explicitly, case-insensitively
        let chat = Chat::try_parse_from(["chat", "bin", "--pause-wait", "none"]).unwrap();
        assert_eq!(chat.pause_wait, PauseWait::None);
        let chat = Chat::try_parse_from(["chat", "bin", "--pause-wait", "Disabled"]).unwrap();
        assert_eq!(chat.pause_wait, PauseWait::None);
        // a zero duration is no wait at all, so it maps to fail-fast too
        let chat = Chat::try_parse_from(["chat", "bin", "--pause-wait", "0s"]).unwrap();
        assert_eq!(chat.pause_wait, PauseWait::None);
        // an explicit duration parses through humantime and opts in to parking
        let chat = Chat::try_parse_from(["chat", "bin", "--pause-wait", "15m"]).unwrap();
        assert_eq!(chat.pause_wait, PauseWait::For(Duration::from_secs(900)));
        let chat = Chat::try_parse_from(["chat", "bin", "--pause-wait", "1h"]).unwrap();
        assert_eq!(chat.pause_wait, PauseWait::For(Duration::from_secs(3600)));
        // the unattended opt-in waits forever
        let chat = Chat::try_parse_from(["chat", "bin", "--pause-wait", "forever"]).unwrap();
        assert_eq!(chat.pause_wait, PauseWait::Forever);
        // garbage is rejected with a usage hint
        assert!(Chat::try_parse_from(["chat", "bin", "--pause-wait", "soon"]).is_err());
    }

    /// Summary carries the same --pause-wait flag with the same fail-fast default
    #[test]
    fn summary_pause_wait_parses() {
        // the default is fail-fast, exactly like chat's
        let summary = Summary::try_parse_from(["summary", "bin"]).unwrap();
        assert_eq!(summary.pause_wait, PauseWait::None);
        // durations opt in to parking through the shared parser
        let summary = Summary::try_parse_from(["summary", "bin", "--pause-wait", "15m"]).unwrap();
        assert_eq!(summary.pause_wait, PauseWait::For(Duration::from_secs(900)));
    }

    /// --continue resumes the agent loop headless with no new prompt
    #[test]
    fn chat_continue_rules() {
        // a bare continuation of a saved session is headless and validates
        let chat = Chat::try_parse_from(["chat", "--resume", "x", "--continue"]).unwrap();
        assert!(chat.continue_run);
        assert!(chat.is_headless());
        assert!(chat.validate().is_ok());
        // continuing needs a session to continue; clap suppresses the
        // `requires` here (requiring an arg the present positional conflicts
        // with), so the rule is enforced by validate() instead
        let chat = Chat::try_parse_from(["chat", "bin", "--continue"]).unwrap();
        assert!(chat.validate().is_err());
        // a continuation never also takes a new prompt
        let both = ["chat", "--resume", "x", "--continue", "--prompt", "hi"];
        assert!(Chat::try_parse_from(both).is_err());
    }

    /// `batch run` defaults to continuing the latest run in the current dir
    #[test]
    fn batch_run_defaults() {
        // a bare run carries the documented defaults
        let batch = Batch::try_parse_from(["batch", "run"]).unwrap();
        let Batch::Run(run) = batch else {
            panic!("expected a run command")
        };
        assert_eq!(run.dir, PathBuf::from("."));
        assert_eq!(run.run, None);
        assert!(!run.new_run);
        assert!(run.units.is_empty());
        assert!(!run.dry_run);
        assert!(!run.retry_exhausted);
        assert_eq!(run.max_concurrency, None);
    }

    /// A run is either an explicit number or brand new, never both
    #[test]
    fn batch_run_number_conflicts() {
        // an explicit run number parses
        let batch = Batch::try_parse_from(["batch", "run", "--run", "8"]).unwrap();
        let Batch::Run(run) = batch else {
            panic!("expected a run command")
        };
        assert_eq!(run.run, Some(8));
        // starting a new run parses
        let batch = Batch::try_parse_from(["batch", "run", "--new-run"]).unwrap();
        let Batch::Run(run) = batch else {
            panic!("expected a run command")
        };
        assert!(run.new_run);
        // naming a run and asking for a new one at once is rejected
        let both = ["batch", "run", "--run", "8", "--new-run"];
        assert!(Batch::try_parse_from(both).is_err());
    }

    /// `batch run --units` narrows an invocation to the named units
    #[test]
    fn batch_run_units_narrow() {
        // several unit names ride one flag
        let batch = Batch::try_parse_from(["batch", "run", "--units", "s01", "s02"]).unwrap();
        let Batch::Run(run) = batch else {
            panic!("expected a run command")
        };
        assert_eq!(run.units, vec!["s01".to_owned(), "s02".to_owned()]);
    }

    /// `batch figures` needs the evaluation.md path and takes a check mode
    #[test]
    fn batch_figures_rules() {
        // the evaluation.md path is required
        assert!(Batch::try_parse_from(["batch", "figures"]).is_err());
        // a plain regeneration writes; --check only verifies
        let batch = Batch::try_parse_from(["batch", "figures", "eval.md"]).unwrap();
        let Batch::Figures(figures) = batch else {
            panic!("expected a figures command")
        };
        assert_eq!(figures.evaluation, PathBuf::from("eval.md"));
        assert!(!figures.check);
        let batch = Batch::try_parse_from(["batch", "figures", "eval.md", "--check"]).unwrap();
        let Batch::Figures(figures) = batch else {
            panic!("expected a figures command")
        };
        assert!(figures.check);
    }

    /// The setup wizard parses bare and with its Ghidra override flags
    #[test]
    fn setup_parses_bare_and_with_ghidra_flags() {
        // a bare setup leaves every override unset
        let setup = Setup::try_parse_from(["setup"]).unwrap();
        assert!(setup.ghidra_version.is_none());
        assert!(setup.ghidra_dir.is_none());
        assert!(setup.ghidra_zip.is_none());
        // a pinned release and an install root both parse
        let setup = Setup::try_parse_from([
            "setup",
            "--ghidra-version",
            "12.1.4",
            "--ghidra-dir",
            "/opt/installs",
        ])
        .unwrap();
        assert_eq!(setup.ghidra_version.as_deref(), Some("12.1.4"));
        assert_eq!(setup.ghidra_dir, Some(PathBuf::from("/opt/installs")));
    }

    /// An offline zip install conflicts with pinning a release to download
    #[test]
    fn setup_zip_conflicts_with_version() {
        // the zip path alone parses for offline installs
        let setup = Setup::try_parse_from(["setup", "--ghidra-zip", "g.zip"]).unwrap();
        assert_eq!(setup.ghidra_zip, Some(PathBuf::from("g.zip")));
        // but a local zip and a pinned download are mutually exclusive
        assert!(
            Setup::try_parse_from([
                "setup",
                "--ghidra-zip",
                "g.zip",
                "--ghidra-version",
                "12.1.4"
            ])
            .is_err()
        );
    }
}

/// Explore global usage and cost stats across every saved ThorPal session
///
/// Opens a read-only TUI over the sessions root: nothing is written or
/// locked, so it is always safe to run alongside live chats.
#[derive(Parser, Debug)]
pub struct Usage {
    /// The sessions root to scan instead of ~/.thorium/sessions (for fixtures
    /// and inspecting copied stores)
    #[clap(long, value_name = "DIR")]
    pub sessions_root: Option<PathBuf>,
}

/// Interactively set up ThorPal's dependencies and provider credentials
///
/// Opens a wizard TUI that walks through logging into Thorium (when no config
/// exists yet), configuring an LLM provider with an optional live connection
/// test, detecting or downloading a Ghidra install, checking for the full JDK
/// Ghidra needs, and choosing the Ghidra projects directory. Nothing is
/// written until the final review step is confirmed.
#[derive(Parser, Debug)]
pub struct Setup {
    /// Install this specific Ghidra release tag (e.g. "12.1.4") instead of the
    /// latest release when the wizard downloads Ghidra
    #[clap(long, value_name = "VERSION")]
    pub ghidra_version: Option<String>,
    /// The root directory downloaded Ghidra releases are installed under
    /// (defaults to the local data dir, e.g. ~/.local/share/thorium/ghidra)
    #[clap(long, value_name = "DIR")]
    pub ghidra_dir: Option<PathBuf>,
    /// Install Ghidra from a local release zip instead of downloading one
    /// (for offline boxes)
    #[clap(long, value_name = "FILE", conflicts_with = "ghidra_version")]
    pub ghidra_zip: Option<PathBuf>,
}

/// Summarize a local binary with ThorPal
#[derive(Parser, Debug)]
pub struct Summary {
    /// The path to the local binary to summarize
    pub binary: PathBuf,
    /// The Ghidra installation directory to analyze with, overriding the `ai` config
    #[clap(long, env = "GHIDRA_INSTALL")]
    pub ghidra_install: Option<PathBuf>,
    /// The ThorPal workflow to use, overriding the `ai` config and builtin workflow
    #[clap(long)]
    pub workflow: Option<PathBuf>,
    /// Always create a new Ghidra project for this binary instead of reusing
    /// the most recent one
    #[clap(long)]
    pub fresh_project: bool,
    /// Analyze an anonymized copy of the binary, staged under the Ghidra
    /// projects root and named by its SHA-256, so the model never sees the
    /// original file name or path
    #[clap(long)]
    pub anonymize: bool,
    /// How long a paused run (expired credentials, provider outage) may park
    /// waiting for recovery. The default `none` fails fast: the provider error
    /// surfaces within the retry budget and the run exits 1. A duration like
    /// `15m`/`1h` (or `forever`) opts in to parking: while paused the run
    /// probes on a backoff and re-reads the config file when it changes (so
    /// refreshing a token in place resumes the run automatically), and when
    /// the wait expires the run exits 75 with a resumable session
    #[clap(long, default_value = "none", value_parser = parse_pause_wait)]
    pub pause_wait: PauseWait,
}

/// The batched-evaluation commands: pooled headless runs over a manifest of
/// sample units, plus regeneration of the docs' evaluation figures
#[derive(Parser, Debug)]
pub enum Batch {
    /// Launch or continue a pooled evaluation run from a batch manifest
    Run(BatchRun),
    /// Summarize a run's unit statuses, attempts, usage, and cost
    Status(BatchStatus),
    /// Regenerate the evaluation figures and the spliced aggregate table from
    /// the docs' Scoreboard table
    Figures(BatchFigures),
}

/// Launch or continue a pooled evaluation run from a batch manifest
#[derive(Parser, Debug)]
pub struct BatchRun {
    /// The evaluation directory holding batch.yml and the runs/ state tree
    #[clap(long, default_value = ".")]
    pub dir: PathBuf,
    /// The run number to create or continue (defaults to the latest run
    /// directory, or run 1 when none exist yet)
    #[clap(long)]
    pub run: Option<u32>,
    /// Start a brand-new run numbered one past the latest instead of
    /// continuing the latest run
    #[clap(long, conflicts_with = "run")]
    pub new_run: bool,
    /// Act only on these manifest unit names (defaults to every unit)
    #[clap(long, num_args = 1..)]
    pub units: Vec<String>,
    /// Print the reconciled staging and launch plan without staging samples or
    /// spawning any sessions
    #[clap(long)]
    pub dry_run: bool,
    /// Re-arm the retry budgets of units that already exhausted theirs
    #[clap(long)]
    pub retry_exhausted: bool,
    /// Cap live concurrency below the manifest's maximum for this invocation
    #[clap(long)]
    pub max_concurrency: Option<usize>,
}

/// Summarize a run's unit statuses, attempts, usage, and cost
#[derive(Parser, Debug)]
pub struct BatchStatus {
    /// The evaluation directory holding batch.yml and the runs/ state tree
    #[clap(long, default_value = ".")]
    pub dir: PathBuf,
    /// The run number to summarize (defaults to the latest run)
    #[clap(long)]
    pub run: Option<u32>,
}

/// The offline-telemetry commands: export privacy-safe bundles compiled from
/// saved ThorPal sessions, and explore bundles or a local store
#[derive(Parser, Debug)]
pub enum Telemetry {
    /// Export a privacy-safe telemetry bundle from saved ThorPal sessions
    Export(TelemetryExport),
    /// Explore telemetry bundles, or the local session store with no args
    Explore(TelemetryExplore),
}

/// Export a privacy-safe telemetry bundle from saved ThorPal sessions
///
/// Folds each session's transcript and forum into counts, token totals,
/// histograms, and enum tallies -- never message text, tool arguments or
/// results, forum prose, file names, or paths -- then renders the exact bytes
/// that would leave the machine and writes them only after confirmation
/// (`--yes` skips the TUI for scripted exports). Strictly read-only over the
/// sessions root, like `thorctl analyze usage`.
#[derive(Parser, Debug)]
pub struct TelemetryExport {
    /// The bundle level: 0 is strictly content-free, 1 adds normalized
    /// failure signatures (bounded multi-line, defanged, number-scrubbed
    /// prefixes of failing tool results)
    #[clap(long, default_value_t = 0, value_parser = clap::value_parser!(u8).range(0..=1))]
    pub level: u8,
    /// Export only these sessions by name (repeat the flag for each session;
    /// defaults to every session under the root)
    #[clap(long, value_name = "NAME")]
    pub session: Vec<String>,
    /// The sessions root to scan instead of ~/.thorium/sessions (for fixtures
    /// and inspecting copied stores)
    #[clap(long, value_name = "DIR")]
    pub sessions_root: Option<PathBuf>,
    /// A file of deny tokens (one case-insensitive token per line, `#`
    /// comments allowed) scrubbed out of every exported string before the
    /// preview renders; the list itself never leaves the machine and is never
    /// persisted
    #[clap(long, value_name = "FILE")]
    pub redact_tokens: Option<PathBuf>,
    /// Write the bundle to this path (a `.gz` suffix gzips the same bytes);
    /// defaults to thorpal-telemetry-<UTC timestamp>.json in the working
    /// directory
    #[clap(long, value_name = "FILE")]
    pub out: Option<PathBuf>,
    /// Skip the interactive flow: apply redaction, write the bundle, and
    /// print one stable machine-parseable summary line on stderr
    #[clap(long)]
    pub yes: bool,
}

/// Explore telemetry bundles, or the local session store with no args
///
/// Opens a read-only TUI cycling six screens -- fleet overview, tool league
/// table, failure signatures, stuck points, detector outcomes, and token
/// economics -- over the merged bundles, or over the local sessions root
/// (computing the same aggregates on the fly) when no bundle is given.
/// Session drill-down is available only over a local store: bundles carry no
/// messages to replay, by design.
#[derive(Parser, Debug)]
pub struct TelemetryExplore {
    /// The bundle files to merge and explore (plain or gzipped); omit to
    /// explore the local sessions root instead
    pub bundles: Vec<PathBuf>,
    /// The sessions root to scan instead of ~/.thorium/sessions when no
    /// bundle is given (for fixtures and inspecting copied stores)
    #[clap(long, value_name = "DIR", conflicts_with = "bundles")]
    pub sessions_root: Option<PathBuf>,
}

/// Regenerate the evaluation figures and the spliced aggregate table from the
/// docs' Scoreboard table
#[derive(Parser, Debug)]
pub struct BatchFigures {
    /// The evaluation.md whose Scoreboard drives the figures (the SVGs are
    /// written to its sibling figures/ directory)
    pub evaluation: PathBuf,
    /// Verify instead of write: report what would change and exit nonzero if
    /// any artifact is stale, writing nothing
    #[clap(long)]
    pub check: bool,
    /// Recompute every row's Novel and CISA rate cells from this directory's
    /// run<run>-<slug>.json scoring files, failing on any mismatch (rows
    /// without a scoring JSON are skipped and counted on stderr)
    #[clap(long)]
    pub scoring: Option<PathBuf>,
}
