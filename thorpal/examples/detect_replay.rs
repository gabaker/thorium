//! Offline detector replay: run the preseed detector pass over one binary and
//! dump the resulting forum as JSON, with no LLM and no session store.
//!
//! Exists to A/B the detectors against recorded telemetry: replaying the same
//! sample through the current code and diffing the seeded leads against what a
//! stored session's `forum.json` holds shows exactly which leads a change
//! removed, demoted, or kept. The dumped shape is the session store's
//! `ForumSnapshot`, so existing telemetry tooling parses it unchanged.
//!
//! Usage:
//!   cargo run -p thorpal --example detect_replay -- <binary> \
//!     [--projects <dir>] [--project-name <name>] [--ghidra <install>] \
//!     [--yara <rules>] [--out <file>] [--no-questions]
//!
//! The Ghidra install falls back to `THORPAL_LIVE_GHIDRA`; the projects dir
//! defaults to a replay-local folder beside the binary's existing project when
//! one is passed, so pre-analyzed projects are reused instead of paying a
//! fresh import + auto-analysis.

use std::path::PathBuf;
use std::process::ExitCode;

use thorpal::{ForumState, GhidraSession, GhidraSettings, seed_forum_from_detectors};

/// The parsed command line for one replay run.
struct Args {
    /// The sample to run the detectors over.
    binary: PathBuf,
    /// The Ghidra projects directory to open or create the project in.
    projects: Option<PathBuf>,
    /// The Ghidra project name (defaults to the binary file stem).
    project_name: Option<String>,
    /// The Ghidra installation directory.
    ghidra: Option<PathBuf>,
    /// Operator YARA rules to include in the pass, when given.
    yara: Option<PathBuf>,
    /// Where to write the forum snapshot JSON (stdout when unset).
    out: Option<PathBuf>,
    /// Whether to skip deriving seeded questions.
    no_questions: bool,
}

/// Parse the command line, returning `None` (after printing usage) on error.
///
/// # Arguments
///
/// * `argv` - The raw process arguments, program name included
fn parse_args(mut argv: std::env::Args) -> Option<Args> {
    // skip the program name and start with an empty arg set
    argv.next();
    let mut args = Args {
        binary: PathBuf::new(),
        projects: None,
        project_name: None,
        ghidra: None,
        yara: None,
        out: None,
        no_questions: false,
    };
    // walk the remaining arguments, pairing flags with their values
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--projects" => args.projects = argv.next().map(PathBuf::from),
            "--project-name" => args.project_name = argv.next(),
            "--ghidra" => args.ghidra = argv.next().map(PathBuf::from),
            "--yara" => args.yara = argv.next().map(PathBuf::from),
            "--out" => args.out = argv.next().map(PathBuf::from),
            "--no-questions" => args.no_questions = true,
            other if args.binary.as_os_str().is_empty() && !other.starts_with('-') => {
                args.binary = PathBuf::from(other);
            }
            other => {
                eprintln!("unknown argument: {other}");
                return None;
            }
        }
    }
    // the binary is the one required argument
    if args.binary.as_os_str().is_empty() {
        eprintln!(
            "usage: detect_replay <binary> [--projects <dir>] [--project-name <name>] \
             [--ghidra <install>] [--yara <rules>] [--out <file>] [--no-questions]"
        );
        return None;
    }
    Some(args)
}

/// Run the replay: bootstrap Ghidra, seed a throwaway forum, dump the snapshot.
#[tokio::main]
async fn main() -> ExitCode {
    // parse the command line, bailing with usage on malformed input
    let Some(args) = parse_args(std::env::args()) else {
        return ExitCode::from(2);
    };
    // resolve the Ghidra install from the flag or the live-install env fallback
    let ghidra_install = args
        .ghidra
        .or_else(|| std::env::var_os("THORPAL_LIVE_GHIDRA").map(PathBuf::from));
    if ghidra_install.is_none() {
        eprintln!("no Ghidra install: pass --ghidra or set THORPAL_LIVE_GHIDRA");
        return ExitCode::from(2);
    }
    // default the project name to the binary file stem, matching session naming
    let project_name = args.project_name.unwrap_or_else(|| {
        args.binary
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| "replay".to_string())
    });
    // default the projects dir to a replay-local folder so runs never collide
    // with real session projects unless the caller points at one deliberately
    let projects = args
        .projects
        .unwrap_or_else(|| PathBuf::from("/tmp/detect-replay-projects"));
    // build the lazily-connecting Ghidra session over the chosen project
    let session = GhidraSession::new(GhidraSettings {
        binary: args.binary.clone(),
        ghidra_install,
        projects_dir: projects,
        project_name,
        extra_binaries: Vec::new(),
    });
    // connect (or bootstrap) the project; this is the expensive step
    let started = std::time::Instant::now();
    let ghidra = match session.get().await {
        Ok(ghidra) => ghidra,
        Err(error) => {
            eprintln!("ghidra bootstrap failed: {error:#}");
            return ExitCode::from(1);
        }
    };
    eprintln!("ghidra ready in {:.1}s", started.elapsed().as_secs_f64());
    // seed a throwaway in-memory forum exactly as a real session would
    let forum = ForumState::new();
    let seeded = std::time::Instant::now();
    let report = match seed_forum_from_detectors(
        &ghidra,
        &forum,
        &args.binary,
        !args.no_questions,
        Vec::new(),
        None,
        args.yara.as_deref(),
        None,
    )
    .await
    {
        Ok(report) => report,
        Err(error) => {
            eprintln!("seeding failed: {error:#}");
            let _ = session.shutdown().await;
            return ExitCode::from(1);
        }
    };
    eprintln!("seeded in {:.1}s", seeded.elapsed().as_secs_f64());
    // print the roster so a driver can log per-detector hit counts and failures
    for run in &report.roster {
        eprintln!(
            "detector {} ({}) hits={} failed={}",
            run.name, run.technique, run.hits, run.failed
        );
    }
    // snapshot the forum into the session store's serialized shape
    let snapshot = forum.snapshot().await;
    let json = match serde_json::to_string_pretty(&snapshot) {
        Ok(json) => json,
        Err(error) => {
            eprintln!("snapshot serialization failed: {error:#}");
            let _ = session.shutdown().await;
            return ExitCode::from(1);
        }
    };
    // write the snapshot to the requested sink
    let write_result = match &args.out {
        Some(path) => std::fs::write(path, &json),
        None => {
            println!("{json}");
            Ok(())
        }
    };
    if let Err(error) = write_result {
        eprintln!("writing the snapshot failed: {error:#}");
        let _ = session.shutdown().await;
        return ExitCode::from(1);
    }
    // close the bridge so no Java process lingers after the replay
    if let Err(error) = session.shutdown().await {
        eprintln!("ghidra shutdown failed (ignored): {error:#}");
    }
    ExitCode::SUCCESS
}
