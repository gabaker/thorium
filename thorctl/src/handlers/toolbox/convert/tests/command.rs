//! Tests of the `toolbox convert` command: where it writes, what it prints, and when it fails

use clap::Parser;
use serde_json::Value;

use super::{SMALL_LEGACY, Scratch};
use crate::args::toolbox::ConvertToolbox;
use crate::handlers::toolbox::convert::{Execution, execute};

/// The text of the small legacy toolbox converted with the default options
const SMALL_V2: &str = include_str!("../../../../../tests/data/toolbox/small.v2.json");

/// Run `toolbox convert` with arguments
///
/// # Arguments
///
/// * `args` - The arguments after `convert`
async fn run(args: &[&str]) -> Execution {
    // parse the command the way the binary would
    let cmd =
        ConvertToolbox::try_parse_from(std::iter::once("convert").chain(args.iter().copied()))
            .expect("the arguments parse");
    execute(&cmd).await.expect("the command runs")
}

/// Run `toolbox convert` with arguments and expect it to fail with an error
///
/// # Arguments
///
/// * `args` - The arguments after `convert`
async fn run_error(args: &[&str]) -> String {
    // parse the command the way the binary would
    let cmd =
        ConvertToolbox::try_parse_from(std::iter::once("convert").chain(args.iter().copied()))
            .expect("the arguments parse");
    execute(&cmd)
        .await
        .expect_err("the command fails")
        .to_string()
}

/// Write the small legacy toolbox into a scratch directory
///
/// # Arguments
///
/// * `scratch` - The directory to write into
fn small_input(scratch: &Scratch) -> String {
    // the input is the small legacy fixture
    scratch
        .write("toolbox.json", SMALL_LEGACY)
        .display()
        .to_string()
}

/// With no output named the result lands beside the input and the input is untouched
#[tokio::test]
async fn the_default_output_is_beside_the_input() {
    let scratch = Scratch::new();
    let input = small_input(&scratch);
    let execution = run(&[&input]).await;
    assert!(execution.success);
    // the manifest was written and stdout stayed empty
    assert_eq!(
        std::fs::read_to_string(scratch.file("toolbox.v2.json")).unwrap(),
        SMALL_V2
    );
    assert_eq!(execution.stdout, "");
    // the input did not change and the report went to stderr
    assert_eq!(std::fs::read_to_string(&input).unwrap(), SMALL_LEGACY);
    assert!(execution.stderr.contains("toolbox.v2.json"));
    assert!(execution.stderr.contains("missing_field:image"));
    // running it again changes nothing and says so
    let again = run(&[&input]).await;
    assert!(again.success);
    assert!(again.stderr.contains("up to date"), "{}", again.stderr);
}

/// `-o PATH` writes there (creating directories) and `-o -` prints the manifest
#[tokio::test]
async fn output_goes_where_it_is_told() {
    let scratch = Scratch::new();
    let input = small_input(&scratch);
    let target = scratch.file("nested/dir/out.json");
    let execution = run(&[&input, "-o", &target.display().to_string()]).await;
    assert!(execution.success);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), SMALL_V2);
    assert!(!scratch.file("toolbox.v2.json").exists());
    // `-` prints the manifest on stdout and keeps the report on stderr
    let execution = run(&[&input, "-o", "-"]).await;
    assert_eq!(execution.stdout, SMALL_V2);
    assert!(execution.stderr.contains("missing_field:image"));
    assert!(!scratch.file("toolbox.v2.json").exists());
}

/// A directory input finds its legacy `toolbox.json`, even when a converted one is beside it
#[tokio::test]
async fn directories_convert_their_legacy_manifest() {
    let scratch = Scratch::new();
    small_input(&scratch);
    let dir = scratch.path.display().to_string();
    assert!(run(&[&dir]).await.success);
    assert_eq!(
        std::fs::read_to_string(scratch.file("toolbox.v2.json")).unwrap(),
        SMALL_V2
    );
    // with the converted file now present the directory still converts the legacy one
    let execution = run(&[&dir, "-o", "-"]).await;
    assert_eq!(execution.stdout, SMALL_V2);
}

/// `--in-place` saves the input as `.bak` first, refuses to replace a backup, and `--no-backup`
/// skips it
#[tokio::test]
async fn in_place_keeps_a_backup() {
    let scratch = Scratch::new();
    let input = small_input(&scratch);
    let execution = run(&[&input, "--in-place"]).await;
    assert!(execution.success);
    // the input is now the converted manifest and the backup holds the original
    assert_eq!(std::fs::read_to_string(&input).unwrap(), SMALL_V2);
    assert_eq!(
        std::fs::read_to_string(scratch.file("toolbox.json.bak")).unwrap(),
        SMALL_LEGACY
    );
    // another legacy input with a backup already there is refused and left alone
    scratch.write("toolbox.json", SMALL_LEGACY);
    let message = run_error(&[&input, "--in-place"]).await;
    assert!(
        message.contains("toolbox.json.bak") && message.contains("--no-backup"),
        "{message}"
    );
    assert_eq!(std::fs::read_to_string(&input).unwrap(), SMALL_LEGACY);
    // without a backup the conversion goes ahead and the old backup stays as it was
    assert!(run(&[&input, "--in-place", "--no-backup"]).await.success);
    assert_eq!(std::fs::read_to_string(&input).unwrap(), SMALL_V2);
    assert_eq!(
        std::fs::read_to_string(scratch.file("toolbox.json.bak")).unwrap(),
        SMALL_LEGACY
    );
}

/// `--check` writes nothing and fails when the output is missing or differs
#[tokio::test]
async fn check_compares_with_the_output() {
    let scratch = Scratch::new();
    let input = small_input(&scratch);
    // a missing output fails and nothing is written
    let missing = run(&[&input, "--check"]).await;
    assert!(!missing.success);
    assert!(
        missing.stderr.contains("does not exist") || missing.stderr.contains("missing"),
        "{}",
        missing.stderr
    );
    assert!(!scratch.file("toolbox.v2.json").exists());
    // the current output passes
    assert!(run(&[&input]).await.success);
    let current = run(&[&input, "--check"]).await;
    assert!(current.success, "{}", current.stderr);
    assert_eq!(current.stdout, "");
    // a drifted output fails with a diff on stdout and is left as it was
    let drifted = SMALL_V2.replace("antivirus", "antivirus2");
    scratch.write("toolbox.v2.json", &drifted);
    let stale = run(&[&input, "--check"]).await;
    assert!(!stale.success);
    assert!(
        stale.stdout.contains("-    \"antivirus2\"") && stale.stdout.contains("+    \"antivirus\""),
        "{}",
        stale.stdout
    );
    assert_eq!(
        std::fs::read_to_string(scratch.file("toolbox.v2.json")).unwrap(),
        drifted
    );
    // with --in-place the input is what is checked
    let in_place = run(&[&input, "--check", "--in-place"]).await;
    assert!(!in_place.success);
}

/// `--dry-run` writes nothing, prints the report, and diffs against an existing output
#[tokio::test]
async fn dry_run_writes_nothing() {
    let scratch = Scratch::new();
    let input = small_input(&scratch);
    let fresh = run(&[&input, "--dry-run"]).await;
    assert!(fresh.success);
    assert!(!scratch.file("toolbox.v2.json").exists());
    assert!(fresh.stderr.contains("missing_field:image") && fresh.stdout.is_empty());
    // against an existing file it shows what would change and still writes nothing
    let old = SMALL_V2.replace("640800", "1");
    scratch.write("toolbox.v2.json", &old);
    let diffed = run(&[&input, "--dry-run"]).await;
    assert!(diffed.success);
    let changed = |sign: char, text: &str| {
        diffed
            .stdout
            .lines()
            .any(|line| line.starts_with(sign) && line.contains(text))
    };
    assert!(
        changed('-', "\"sla\": 1,") && changed('+', "\"sla\": 640800,"),
        "{}",
        diffed.stdout
    );
    assert_eq!(
        std::fs::read_to_string(scratch.file("toolbox.v2.json")).unwrap(),
        old
    );
    // in-place dry runs leave the input and its backup alone
    assert!(run(&[&input, "--in-place", "--dry-run"]).await.success);
    assert_eq!(std::fs::read_to_string(&input).unwrap(), SMALL_LEGACY);
    assert!(!scratch.file("toolbox.json.bak").exists());
}

/// The report is printed on stderr, as JSON with `--format json`, and to a file or stdout with
/// `--report`
#[tokio::test]
async fn reports_take_their_formats() {
    let scratch = Scratch::new();
    let input = small_input(&scratch);
    // the human report counts what was converted and lists each issue and stage
    let human = run(&[&input, "-o", "-"]).await;
    for expected in [
        "1 tools",
        "1 builds",
        "1 pipelines",
        "missing_field:image",
        "image noimage@latest",
        "invalid_value:stages.*",
        "antivirus",
    ] {
        assert!(
            human.stderr.contains(expected),
            "no {expected:?} in {}",
            human.stderr
        );
    }
    // --format json prints the summary as JSON on stderr
    let json = run(&[&input, "-o", "-", "--format", "json"]).await;
    let summary: Value = serde_json::from_str(&json.stderr).expect("json report");
    assert_eq!(summary["tools"], 1);
    assert_eq!(summary["issues"][0]["code"], "missing_field:image");
    assert_eq!(summary["source"], "legacy");
    // --report FILE writes the JSON there and keeps stderr free of the report
    let file = scratch.file("report.json");
    let reported = run(&[&input, "-o", "-", "--report", &file.display().to_string()]).await;
    let written: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(written["issues"][1]["code"], "invalid_value:stages.*");
    assert!(!reported.stderr.contains("missing_field:image"));
    // --report - prints it on stdout after nothing else, and cannot share stdout with the manifest
    let to_stdout = run(&[&input, "--report", "-"]).await;
    let summary: Value = serde_json::from_str(&to_stdout.stdout).expect("json on stdout");
    assert_eq!(summary["pipelines"], 1);
    let clash = run_error(&[&input, "-o", "-", "--report", "-"]).await;
    assert!(clash.contains("stdout"), "{clash}");
}

/// `--strict` fails on any issue before writing, and passes a conversion with none
#[tokio::test]
async fn strict_fails_on_issues_without_writing() {
    let scratch = Scratch::new();
    let input = small_input(&scratch);
    let strict = run(&[&input, "--strict"]).await;
    assert!(!strict.success);
    assert!(!scratch.file("toolbox.v2.json").exists());
    assert!(strict.stderr.contains("missing_field:image"));
    // a clean toolbox converts under --strict
    let mut document: Value = serde_json::from_str(SMALL_LEGACY).unwrap();
    document["images"]
        .as_object_mut()
        .unwrap()
        .remove("noimage");
    document["pipelines"]
        .as_object_mut()
        .unwrap()
        .remove("scan-all");
    let clean = scratch.write("clean.json", &document.to_string());
    let strict = run(&[&clean.display().to_string(), "--strict"]).await;
    assert!(strict.success, "{}", strict.stderr);
    assert!(scratch.file("clean.v2.json").exists());
}

/// Names defined twice stop the conversion before anything is written
#[tokio::test]
async fn collisions_stop_the_conversion() {
    let scratch = Scratch::new();
    let mut document: Value = serde_json::from_str(SMALL_LEGACY).unwrap();
    let clamav = document["images"]["clamav"]["latest"].clone();
    let mut other = clamav.clone();
    other["config"]["group"] = "dynamic".into();
    document["images"]["clamav"]["other"] = other;
    let input = scratch
        .write("toolbox.json", &document.to_string())
        .display()
        .to_string();
    let execution = run(&[&input]).await;
    assert!(!execution.success);
    assert!(
        execution.stderr.contains("collision:clamav")
            && execution.stderr.contains("--group-override"),
        "{}",
        execution.stderr
    );
    assert!(!scratch.file("toolbox.v2.json").exists());
    // the override resolves it
    assert!(run(&[&input, "--group-override", "static"]).await.success);
}

/// A manifest already in the current format is a no-op with a clear message unless an output is
/// named
#[tokio::test]
async fn current_manifests_are_a_no_op() {
    let scratch = Scratch::new();
    let current = scratch
        .write("toolbox.v2.json", SMALL_V2)
        .display()
        .to_string();
    let execution = run(&[&current]).await;
    assert!(execution.success);
    assert!(
        execution.stderr.contains("already in the v2 format"),
        "{}",
        execution.stderr
    );
    assert_eq!(std::fs::read_dir(&scratch.path).unwrap().count(), 1);
    // --check passes against itself
    assert!(run(&[&current, "--check"]).await.success);
    // a copy is written when asked for, byte for byte
    let copy = scratch.file("copy.json");
    assert!(
        run(&[&current, "-o", &copy.display().to_string()])
            .await
            .success
    );
    assert_eq!(std::fs::read_to_string(&copy).unwrap(), SMALL_V2);
    // a v2 file that is not normalized fails --check and --in-place normalizes it
    let loose = scratch
        .write(
            "loose.json",
            &serde_json::to_string(&serde_json::from_str::<Value>(SMALL_V2).unwrap()).unwrap(),
        )
        .display()
        .to_string();
    assert!(!run(&[&loose, "--check"]).await.success);
    assert!(run(&[&loose, "--in-place", "--no-backup"]).await.success);
    assert_eq!(std::fs::read_to_string(&loose).unwrap(), SMALL_V2);
}

/// A URL needs an output path, and cannot be replaced in place; both fail before any request
#[tokio::test]
async fn urls_need_an_output() {
    let message = run_error(&["https://example.invalid/toolbox.json"]).await;
    assert!(message.contains("-o"), "{message}");
    let message = run_error(&["https://example.invalid/toolbox.json", "--in-place"]).await;
    assert!(message.contains("--in-place"), "{message}");
}
