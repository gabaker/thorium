//! Runs the thorpal-harness binary on runs whose inputs include a per-run prompt (a sample
//! uploaded as `prompt.txt` by the run's creator) and checks that the prompt becomes the run's
//! question, that the other sample is the one analyzed, that another user's `prompt.txt` is only
//! a file, and that the prompt's text never reaches the logs or `status.json`

use chrono::Utc;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::process::Command;
use std::sync::Arc;
use thorium::models::{Origin, Sample, ScrubbedUser, SubmissionChunk, UserRole, UserSettings};
use uuid::Uuid;

#[allow(dead_code)]
mod common {
    pub mod fixtures;
    pub mod mock;
}

use common::fixtures::RunFixture;
use common::mock::{self, MockServer, Recorded, Reply};

/// The run question the prompt samples hold, distinctive so a leak is easy to find
const QUESTION: &str = "Which C2 domain does it beacon to? amalthea-q7z";

/// The SHA256 of the run's binary sample
fn binary_sha() -> String {
    // any 64 hex digits name a sample
    "b".repeat(64)
}

/// The SHA256 of the run's prompt sample
fn prompt_sha() -> String {
    // any 64 hex digits name a sample
    "c".repeat(64)
}

/// The user who created the fixture run, and so owns its token
const CREATOR: &str = "io";

/// Another user who shares the run's group
const OTHER_USER: &str = "callisto";

/// Build the sample Thorium returns for a SHA256, with one submission under a name
///
/// # Arguments
///
/// * `sha256` - The sample's SHA256
/// * `name` - The submission's file name
/// * `submitter` - The user who uploaded the submission
fn sample(sha256: &str, name: &str, submitter: &str) -> Value {
    // one submission in the fixture's group
    let sample = Sample {
        sha256: sha256.to_owned(),
        sha1: "1".repeat(40),
        md5: "5".repeat(32),
        tags: std::collections::HashMap::new(),
        submissions: vec![SubmissionChunk {
            id: Uuid::new_v4(),
            name: Some(name.to_owned()),
            description: None,
            groups: vec!["jupiter-k3f9".to_owned()],
            submitter: submitter.to_owned(),
            uploaded: Utc::now(),
            origin: Origin::None,
        }],
        comments: Vec::new(),
    };
    serde_json::to_value(sample).unwrap()
}

/// Build the user `whoami` returns for the run's token
///
/// # Arguments
///
/// * `username` - The token's owner
fn user(username: &str) -> Value {
    // a plain user in the fixture's group
    let user = ScrubbedUser {
        username: username.to_owned(),
        role: UserRole::User,
        email: format!("{username}@example.com"),
        groups: vec!["jupiter-k3f9".to_owned()],
        actual_groups: Vec::new(),
        token: "token".to_owned(),
        token_expiration: Utc::now(),
        unix: None,
        settings: UserSettings::default(),
        local: true,
        verified: true,
        has_image: false,
    };
    serde_json::to_value(user).unwrap()
}

/// Start a server that plays Thorium's file and whoami routes, naming each sample's submission
/// from a map, and a model that answers at once
///
/// # Arguments
///
/// * `submissions` - The submission file name and submitter of each sample, by SHA256; any other
///   sample is unknown
/// * `creator` - The user `whoami` names for the run's token, or `None` to fail the lookup
async fn server(
    submissions: BTreeMap<String, (String, String)>,
    creator: Option<&'static str>,
) -> MockServer {
    // one server plays both the model endpoint and Thorium
    MockServer::start(Arc::new(move |request: &Recorded, _| {
        // whoami names the run's creator, or fails
        if request.path == "/api/users/whoami" {
            return match creator {
                Some(creator) => Reply::json(200, &user(creator)),
                None => Reply::json(500, &json!({"error": "whoami is down"})),
            };
        }
        // a sample lookup gets the sample, or a 404 for a sample Thorium doesn't know
        if let Some(sha256) = request.path.strip_prefix("/api/files/sample/") {
            return match submissions.get(sha256) {
                Some((name, submitter)) => Reply::json(200, &sample(sha256, name, submitter)),
                None => Reply::json(404, &json!({"error": "sample not found"})),
            };
        }
        // the model answers without calling a tool
        mock::anthropic_text(request, "the answer")
    }))
    .await
}

/// Run the harness binary in a clean environment, returning its exit code and its logs
///
/// # Arguments
///
/// * `args` - The harness arguments
fn harness(args: &[String]) -> (i32, String) {
    // keep only PATH from the test's environment
    let mut command = Command::new(env!("CARGO_BIN_EXE_thorpal-harness"));
    command.env_clear().args(args);
    if let Ok(path) = std::env::var("PATH") {
        command.env("PATH", path);
    }
    // run it, showing its logs with the test's output
    let output = command.output().unwrap();
    let logs = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    eprintln!("{logs}");
    (output.status.code().unwrap_or(-1), logs)
}

/// A finished or refused run: its exit code, logs, status, and the model requests it made
struct Outcome {
    /// The exit code
    code: i32,
    /// What the harness wrote to stdout and stderr
    logs: String,
    /// The parsed `status.json`
    status: Value,
    /// The raw `status.json`
    status_text: String,
    /// The model requests, in order
    calls: Vec<Recorded>,
    /// The samples directory the run read
    samples: std::path::PathBuf,
    /// Keeps the run's directory alive until the outcome is dropped
    _dir: tempfile::TempDir,
}

/// Run the harness on a fixture run with some samples, each uploaded by the run's creator and
/// written to the samples directory
///
/// # Arguments
///
/// * `samples` - The run's samples in input order, each with the bytes downloaded for it
/// * `names` - The submission file name of each sample Thorium knows, by SHA256
async fn run(samples: &[(String, Vec<u8>)], names: BTreeMap<String, String>) -> Outcome {
    // every submission is the creator's own
    let submissions = names
        .into_iter()
        .map(|(sha, name)| (sha, (name, CREATOR.to_owned())))
        .collect();
    run_with(samples, submissions, Some(CREATOR)).await
}

/// Run the harness on a fixture run with some samples, each written to the samples directory,
/// with Thorium naming each sample's submitter and the run's creator
///
/// # Arguments
///
/// * `samples` - The run's samples in input order, each with the bytes downloaded for it
/// * `submissions` - The submission file name and submitter of each sample Thorium knows
/// * `creator` - The user `whoami` names for the run's token, or `None` to fail the lookup
async fn run_with(
    samples: &[(String, Vec<u8>)],
    submissions: BTreeMap<String, (String, String)>,
    creator: Option<&'static str>,
) -> Outcome {
    // a fixture run on the given samples, pointed at the server for the model and Thorium
    let server = server(submissions, creator).await;
    let mut fixture = RunFixture::new(&server.url, &server.url);
    fixture.run.samples = samples.iter().map(|(sha, _)| sha.clone()).collect();
    let dir = tempfile::tempdir().unwrap();
    let args = fixture.write(dir.path());
    // the thorium-agent downloads each sample into the samples directory under its SHA256
    let samples_dir = dir.path().join("samples");
    std::fs::create_dir_all(&samples_dir).unwrap();
    for (sha, bytes) in samples {
        std::fs::write(samples_dir.join(sha), bytes).unwrap();
    }
    // run the harness off the test's runtime
    let (code, logs) = tokio::task::spawn_blocking(move || harness(&args))
        .await
        .unwrap();
    let status_text = std::fs::read_to_string(dir.path().join("result-files/status.json")).unwrap();
    let status = serde_json::from_str(&status_text).unwrap();
    Outcome {
        code,
        logs,
        status,
        status_text,
        calls: server.on("/v1/messages"),
        samples: samples_dir,
        _dir: dir,
    }
}

/// Get the text of the first user message a model request carries
///
/// # Arguments
///
/// * `request` - The model request
fn first_user_text(request: &Recorded) -> String {
    // the first message is the user's prompt, as a string or text blocks
    let content = &request.body["messages"][0]["content"];
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block["text"].as_str())
            .collect::<Vec<&str>>()
            .join(""),
        other => other.to_string(),
    }
}

/// Check that the question's text never reached the logs or `status.json`
///
/// # Arguments
///
/// * `outcome` - The run
fn assert_text_private(outcome: &Outcome) {
    // neither the whole question nor its distinctive word may show
    for (name, text) in [
        ("logs", &outcome.logs),
        ("status.json", &outcome.status_text),
    ] {
        assert!(
            !text.contains("amalthea-q7z"),
            "the run prompt leaked into {name}: {text}"
        );
    }
}

/// A `prompt.txt` sample in either input order becomes the run's question, appended after the
/// agent's prompt, while the other sample is the one analyzed; the logs and status say a run
/// prompt was used and its size without its text
#[tokio::test(flavor = "multi_thread")]
async fn a_prompt_sample_becomes_the_question_in_either_order() {
    // the binary and the prompt in both orders
    let binary = (binary_sha(), b"MZ\x90\x00binary".to_vec());
    let prompt = (prompt_sha(), QUESTION.as_bytes().to_vec());
    let names = BTreeMap::from([
        (binary_sha(), "io-sample.exe".to_owned()),
        (prompt_sha(), "prompt.txt".to_owned()),
    ]);
    for order in [
        vec![prompt.clone(), binary.clone()],
        vec![binary.clone(), prompt.clone()],
    ] {
        let outcome = run(&order, names.clone()).await;
        assert_eq!(outcome.code, 0, "{}", outcome.logs);
        // the model got the agent's prompt, then the binary, then the question
        assert!(!outcome.calls.is_empty());
        let text = first_user_text(&outcome.calls[0]);
        let binary_path = outcome.samples.join(binary_sha());
        let expected = format!(
            "Describe the input file.\n\nThe input file has SHA256 {} and is at {}.\n\nRun question:\n{QUESTION}",
            binary_sha(),
            binary_path.display()
        );
        assert_eq!(text, expected);
        // the prompt sample isn't offered as the file to analyze
        assert!(!text.contains(&prompt_sha()), "{text}");
        // the status records the run prompt by sample and size only
        assert_eq!(
            outcome.status["run_prompt"],
            json!({"sample": prompt_sha(), "bytes": QUESTION.len()}),
            "{}",
            outcome.status
        );
        // the logs say a run prompt was used and how big it is, and nothing is skipped
        let line = format!(
            "thorpal-harness: the run's question comes from its run prompt (sample {}, {} bytes)",
            prompt_sha(),
            QUESTION.len()
        );
        assert!(outcome.logs.contains(&line), "{}", outcome.logs);
        assert!(!outcome.logs.contains("is skipped"), "{}", outcome.logs);
        assert_text_private(&outcome);
    }
}

/// Without a `prompt.txt` sample the first sample is analyzed with the agent's prompt alone and
/// the status has no run prompt, even when Thorium can't describe the sample
#[tokio::test(flavor = "multi_thread")]
async fn runs_without_a_prompt_sample_are_unchanged() {
    // one run whose sample has another name, and one Thorium doesn't know
    let binary = (binary_sha(), b"MZ\x90\x00binary".to_vec());
    let named = BTreeMap::from([(binary_sha(), "io-sample.exe".to_owned())]);
    for names in [named, BTreeMap::new()] {
        let outcome = run(std::slice::from_ref(&binary), names.clone()).await;
        assert_eq!(outcome.code, 0, "{}", outcome.logs);
        // the prompt is exactly the agent's prompt and the sample
        let text = first_user_text(&outcome.calls[0]);
        let expected = format!(
            "Describe the input file.\n\nThe input file has SHA256 {} and is at {}.",
            binary_sha(),
            outcome.samples.join(binary_sha()).display()
        );
        assert_eq!(text, expected);
        // the status has no run prompt key at all
        assert!(
            outcome.status.get("run_prompt").is_none(),
            "{}",
            outcome.status
        );
        // no run prompt is used, and a sample Thorium can't describe is named in a warning
        assert!(
            !outcome.logs.contains("comes from its run prompt"),
            "{}",
            outcome.logs
        );
        let warned = outcome.logs.contains("isn't checked for a run prompt");
        assert_eq!(warned, names.is_empty(), "{}", outcome.logs);
    }
}

/// Check that a run was refused before any model call with a reason naming the problem, keeping
/// the prompt's text private
///
/// # Arguments
///
/// * `outcome` - The run
/// * `named` - Words the reason must contain
fn assert_refused(outcome: &Outcome, named: &[&str]) {
    // the harness refuses with exit 2 and makes no model call
    assert_eq!(outcome.code, 2, "{}", outcome.logs);
    assert!(outcome.calls.is_empty(), "{:?}", outcome.calls.len());
    assert_eq!(outcome.status["success"], Value::Bool(false));
    // the reason names the problem
    let reason = outcome.status["reason"].as_str().unwrap_or_default();
    for word in named {
        assert!(reason.contains(word), "{word}: {reason}");
    }
    assert_text_private(outcome);
}

/// A run prompt that is too big, isn't UTF-8, or is the run's only sample is refused before
/// any model call with a reason naming the problem, as are two run prompts
#[tokio::test(flavor = "multi_thread")]
async fn bad_run_prompts_are_refused() {
    // Thorium names one sample a binary and the other prompt.txt
    let binary = (binary_sha(), b"MZ\x90\x00binary".to_vec());
    let names = BTreeMap::from([
        (binary_sha(), "io-sample.exe".to_owned()),
        (prompt_sha(), "prompt.txt".to_owned()),
    ]);
    // a prompt one byte over the cap
    let mut big = QUESTION.as_bytes().to_vec();
    big.resize(16 * 1024 + 1, b'x');
    let outcome = run(&[binary.clone(), (prompt_sha(), big)], names.clone()).await;
    assert_refused(&outcome, &["prompt.txt", "16384 bytes", "16385"]);
    // a prompt that isn't UTF-8
    let mut invalid = QUESTION.as_bytes().to_vec();
    invalid.extend_from_slice(&[0xff, 0xfe, 0x00]);
    let outcome = run(&[(prompt_sha(), invalid), binary.clone()], names.clone()).await;
    assert_refused(&outcome, &["prompt.txt", "UTF-8"]);
    // a prompt with no file to analyze
    let prompt = (prompt_sha(), QUESTION.as_bytes().to_vec());
    let outcome = run(std::slice::from_ref(&prompt), names.clone()).await;
    assert_refused(&outcome, &["prompt.txt", "no file to analyze"]);
    // two prompts in one run
    let second = "d".repeat(64);
    let mut two = names.clone();
    two.insert(second.clone(), "/tmp/questions/prompt.txt".to_owned());
    let outcome = run(
        &[prompt, (second, b"another question".to_vec()), binary],
        two,
    )
    .await;
    assert_refused(&outcome, &["prompt.txt", "2"]);
}

/// Check that a run analyzed its first sample with the agent's prompt alone: no run question, no
/// run prompt in the status or logs, and the text of any `prompt.txt` kept out of the logs
///
/// # Arguments
///
/// * `outcome` - The run
/// * `first` - The SHA256 of the run's first sample
fn assert_analyzed_as_file(outcome: &Outcome, first: &str) {
    // the run finished and the model got only the agent's prompt and the first sample
    assert_eq!(outcome.code, 0, "{}", outcome.logs);
    let text = first_user_text(&outcome.calls[0]);
    let expected = format!(
        "Describe the input file.\n\nThe input file has SHA256 {first} and is at {}.",
        outcome.samples.join(first).display()
    );
    assert_eq!(text, expected);
    assert!(!text.contains("Run question:"), "{text}");
    // no run prompt is recorded or logged
    assert!(
        outcome.status.get("run_prompt").is_none(),
        "{}",
        outcome.status
    );
    assert!(
        !outcome.logs.contains("comes from its run prompt"),
        "{}",
        outcome.logs
    );
    assert_text_private(outcome);
}

/// A `prompt.txt` another user uploaded is an ordinary file: a binary one doesn't refuse the run,
/// and a text one never becomes the run's question
#[tokio::test(flavor = "multi_thread")]
async fn another_users_prompt_txt_is_a_file() {
    // another user re-uploaded a binary as prompt.txt, and it is the run's only input
    let binary = (binary_sha(), b"MZ\x90\x00binary".to_vec());
    let theirs = |sha: String| (sha, ("prompt.txt".to_owned(), OTHER_USER.to_owned()));
    let submissions = BTreeMap::from([theirs(binary_sha())]);
    let outcome = run_with(std::slice::from_ref(&binary), submissions, Some(CREATOR)).await;
    assert_analyzed_as_file(&outcome, &binary_sha());
    // another user's small text prompt.txt first, then the creator's binary
    let prompt = (prompt_sha(), QUESTION.as_bytes().to_vec());
    let submissions = BTreeMap::from([
        theirs(prompt_sha()),
        (
            binary_sha(),
            ("io-sample.exe".to_owned(), CREATOR.to_owned()),
        ),
    ]);
    let outcome = run_with(&[prompt, binary], submissions, Some(CREATOR)).await;
    assert_analyzed_as_file(&outcome, &prompt_sha());
    // the binary is skipped as the run's second sample
    assert!(outcome.logs.contains("is skipped"), "{}", outcome.logs);
}

/// When Thorium can't say who created the run, a `prompt.txt` is analyzed as a file, with a
/// warning, rather than read as the question
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_creator_lookup_treats_prompt_txt_as_a_file() {
    // the creator's own prompt.txt and binary, but whoami fails
    let prompt = (prompt_sha(), QUESTION.as_bytes().to_vec());
    let binary = (binary_sha(), b"MZ\x90\x00binary".to_vec());
    let submissions = BTreeMap::from([
        (prompt_sha(), ("prompt.txt".to_owned(), CREATOR.to_owned())),
        (
            binary_sha(),
            ("io-sample.exe".to_owned(), CREATOR.to_owned()),
        ),
    ]);
    let outcome = run_with(&[binary, prompt], submissions, None).await;
    assert_analyzed_as_file(&outcome, &binary_sha());
    // the warning says why no run prompt was looked for
    assert!(
        outcome.logs.contains("unable to learn who created the run"),
        "{}",
        outcome.logs
    );
}
