//! Tests the thorctl `grep` command's corpus and target searches, its output formats, its resume
//! cursor, and its terminal styling against the test API

mod common;

#[cfg(unix)]
use common::editor::Pty;
use common::{TestUser, assert_success, stdout};
use serde_json::Value;
use std::process::Output;
use std::time::Duration;
use thorium::Error;
use thorium::models::{Buffer, CommentRequest, SampleRequest};
use uuid::Uuid;

/// How long a grep on a terminal may take
const WAIT: Duration = Duration::from_secs(120);

/// Build a needle no other test's data contains
fn unique_needle() -> String {
    format!("ThorctlGrepNeedle{}", Uuid::new_v4().simple())
}

/// Upload a sample to the user's group with a needle in a tag and a comment
///
/// # Arguments
///
/// * `user` - The test user
/// * `needle` - The text to plant
async fn seed(user: &TestUser, needle: &str) -> Result<String, Error> {
    // upload a unique sample tagged with the needle
    let req = SampleRequest::new_buffer(
        Buffer::new(format!("thorctl grep body {}", Uuid::new_v4())).name("grep.txt"),
        vec![user.group.clone()],
    )
    .tag("GrepTag", needle);
    let hashes = user.client.files.create(req).await?;
    // comment on the sample with the needle
    let comment = CommentRequest::new(&hashes.sha256, format!("a comment with {needle} in it"));
    user.client.files.comment(comment).await?;
    Ok(hashes.sha256)
}

/// Get a command's stderr as a string
///
/// # Arguments
///
/// * `output` - The command's output
fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Get the item keys of the flat rows a grep printed
///
/// # Arguments
///
/// * `output` - The command's output
fn flat_keys(output: &Output) -> Vec<String> {
    // the key is everything before the first colon of a row
    let mut keys: Vec<String> = stdout(output)
        .lines()
        .filter_map(|row| row.split(':').next())
        .map(str::to_owned)
        .collect();
    keys.dedup();
    keys
}

/// A corpus grep and a single target grep print flat rows when piped, and heading and JSON
/// output when asked for, all without escape codes
#[tokio::test]
async fn grep_prints_each_format_for_a_corpus_and_a_target() -> Result<(), Error> {
    // a user whose group holds one sample with the needle in a tag and a comment
    let user = TestUser::new("grep-formats").await?;
    let needle = unique_needle();
    let sha = seed(&user, &needle).await?;
    // a piped corpus grep prints one self contained flat row per match
    let out = user
        .run("grep-corpus-flat", &["grep", &needle, "-g", &user.group])
        .await;
    assert_success(&out);
    let printed = stdout(&out);
    let rows: Vec<&str> = printed.lines().collect();
    assert_eq!(rows.len(), 2, "{printed}");
    assert!(
        rows.contains(&format!("{sha}:tags:GrepTag:{needle}:1:{needle}").as_str()),
        "{printed}"
    );
    assert!(
        rows.iter()
            .any(|row| row.starts_with(&format!("{sha}:comments:"))
                && row.ends_with(&format!(":1:{needle} in it"))),
        "{printed}"
    );
    assert!(
        !printed.contains('\u{1b}'),
        "a pipe got escape codes: {printed}"
    );
    assert!(
        stderr(&out).contains("grep: 1 matched items"),
        "{}",
        stderr(&out)
    );
    // a single target grep prints the same rows
    let out = user.run("grep-target-flat", &["grep", &needle, &sha]).await;
    assert_success(&out);
    assert_eq!(stdout(&out), printed);
    // heading output groups the matches under the key without escape codes in a pipe
    let args = ["grep", &needle, "-g", &user.group, "--format", "heading"];
    let out = user.run("grep-corpus-heading", &args).await;
    assert_success(&out);
    let heading = stdout(&out);
    assert!(heading.starts_with(&format!("{sha}\n  ")), "{heading}");
    assert!(
        heading.contains(&format!("  tags/GrepTag/{needle}\n    1:{needle}\n")),
        "{heading}"
    );
    assert!(
        !heading.contains('\u{1b}'),
        "a pipe got escape codes: {heading}"
    );
    // JSON output is one object per item with every source's matches
    let args = ["grep", &needle, &sha, "--format", "json"];
    let out = user.run("grep-target-json", &args).await;
    assert_success(&out);
    let items: Vec<Value> = stdout(&out)
        .lines()
        .map(|line| serde_json::from_str(line).expect("a JSON line"))
        .collect();
    assert_eq!(items.len(), 1, "{}", stdout(&out));
    assert_eq!(items[0]["key"], sha.as_str());
    assert_eq!(
        items[0]["tags"]["GrepTag"][&needle]["matches"][0]["snippet"],
        needle.as_str()
    );
    assert_eq!(
        items[0]["comments"].as_object().map(serde_json::Map::len),
        Some(1)
    );
    // a pattern nothing matches exits with a failure
    let out = user
        .run(
            "grep-no-match",
            &["grep", &unique_needle(), "-g", &user.group],
        )
        .await;
    assert!(!out.status.success(), "{}", stdout(&out));
    assert!(stdout(&out).is_empty(), "{}", stdout(&out));
    Ok(())
}

/// A corpus grep that stops at its limit prints a cursor, and resuming with it finds the rest
#[tokio::test]
async fn grep_resumes_from_the_printed_cursor() -> Result<(), Error> {
    // a user whose group holds two samples with the needle
    let user = TestUser::new("grep-resume").await?;
    let needle = unique_needle();
    let first = seed(&user, &needle).await?;
    let second = seed(&user, &needle).await?;
    // stop after one item a page at a time
    let args = [
        "grep",
        &needle,
        "-g",
        &user.group,
        "--sources",
        "tags",
        "-l",
        "1",
        "-p",
        "1",
    ];
    let out = user.run("grep-resume-first", &args).await;
    assert_success(&out);
    let seen = flat_keys(&out);
    assert_eq!(seen.len(), 1, "{}", stdout(&out));
    // the summary hands back the cursor to resume with
    let err = stderr(&out);
    let cursor = err
        .lines()
        .find_map(|line| line.strip_prefix("grep: search stopped early; resume with --cursor "))
        .unwrap_or_else(|| panic!("no resume cursor in: {err}"))
        .to_owned();
    Uuid::parse_str(&cursor).expect("the resume cursor is a UUID");
    // resuming finds the other sample and never repeats the first
    let args = [
        "grep",
        &needle,
        "-g",
        &user.group,
        "--sources",
        "tags",
        "--cursor",
        &cursor,
    ];
    let out = user.run("grep-resume-rest", &args).await;
    assert_success(&out);
    let rest = flat_keys(&out);
    assert_eq!(rest.len(), 1, "{}", stdout(&out));
    assert_ne!(seen, rest);
    let mut all = [seen, rest].concat();
    all.sort();
    let mut expected = vec![first, second];
    expected.sort();
    assert_eq!(all, expected);
    assert!(!stderr(&out).contains("resume with"), "{}", stderr(&out));
    Ok(())
}

/// Whether text holds an ANSI color or style sequence
///
/// Only select graphic rendition sequences count, since thorctl shows the cursor on exit on any
/// terminal whether or not output is styled.
///
/// # Arguments
///
/// * `text` - The text to check
#[cfg(unix)]
fn has_styling(text: &str) -> bool {
    // look for an escape and bracket followed by numeric parameters and a closing `m`
    text.split("\u{1b}[").skip(1).any(|rest| {
        let params = rest.trim_start_matches(|c: char| c.is_ascii_digit() || c == ';');
        params.starts_with('m')
    })
}

/// Run a grep on a pseudo-terminal and return what it printed
///
/// # Arguments
///
/// * `user` - The test user whose keys file authenticates the run
/// * `needle` - The pattern to grep for
/// * `envs` - Environment variables to set for the run
#[cfg(unix)]
async fn grep_on_terminal(user: &TestUser, needle: &str, envs: &[(&str, &str)]) -> String {
    // build the arguments and environment the blocking terminal driver owns
    let keys = user.keys.display().to_string();
    let group = user.group.clone();
    let needle = needle.to_owned();
    let mut envs: Vec<(String, String)> = envs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect();
    // keep the loopback API off any configured proxy
    envs.push(("NO_PROXY".to_owned(), "127.0.0.1,localhost,::1".to_owned()));
    envs.push(("no_proxy".to_owned(), "127.0.0.1,localhost,::1".to_owned()));
    // drive the terminal on a blocking thread so the runtime stays free
    tokio::task::spawn_blocking(move || {
        // run quietly so no progress bar shares the terminal with the matches
        let args = [
            "--keys",
            keys.as_str(),
            "--skip-update",
            "--quiet",
            "grep",
            needle.as_str(),
            "-g",
            group.as_str(),
            "--sources",
            "tags",
        ];
        // borrow the environment the way the terminal driver takes it
        let envs: Vec<(&str, &str)> = envs
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect();
        // run thorctl to its end on a new terminal and return what it printed
        let mut pty = Pty::spawn(
            std::path::Path::new(env!("CARGO_BIN_EXE_thorctl")),
            &args,
            &envs,
        )
        .expect("start thorctl on a terminal");
        let status = pty.finish(WAIT).expect("thorctl finished");
        assert!(status.success(), "{status:?}\n{}", pty.output());
        pty.output().replace("\r\n", "\n")
    })
    .await
    .expect("the terminal run panicked")
}

/// On a terminal grep prints styled heading output, and `NO_COLOR` or `TERM=dumb` get plain flat
/// rows
#[cfg(unix)]
#[tokio::test]
async fn grep_honours_no_color_on_a_terminal() -> Result<(), Error> {
    // a user whose group holds one sample with the needle in a tag
    let user = TestUser::new("grep-no-color").await?;
    let needle = unique_needle();
    let sha = seed(&user, &needle).await?;
    // a plain terminal gets styled heading output
    let styled = grep_on_terminal(&user, &needle, &[]).await;
    assert!(has_styling(&styled), "a terminal got no styling: {styled}");
    assert!(!styled.contains(&format!("{sha}:tags:")), "{styled}");
    // NO_COLOR and TERM=dumb each get the same unstyled flat row a pipe gets
    let row = format!("{sha}:tags:GrepTag:{needle}:1:{needle}\n");
    for envs in [[("NO_COLOR", "1")], [("TERM", "dumb")]] {
        let plain = grep_on_terminal(&user, &needle, &envs).await;
        assert!(!has_styling(&plain), "{envs:?} got escape codes: {plain:?}");
        assert!(plain.contains(&row), "{envs:?} printed: {plain}");
    }
    Ok(())
}
