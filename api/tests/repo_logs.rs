//! Tests that creating repos never logs the credentials in their URLs

use std::process::Command;

use thorium::Error;
use thorium::models::RepoRequest;
use thorium::test_utilities::{self, generators};

/// The password embedded in the repo URLs these tests submit
const PASSWORD: &str = "CornSilkPw7Q9z";

/// The environment variable that marks the child process doing the requests
const CHILD_MARKER: &str = "THORIUM_REPO_LOGS_CHILD";

/// Create repos whose URLs carry a password while the API logs everything
///
/// This only runs as the child of [`repo_create_logs_no_credentials`], which captures the
/// in-process API's log output and checks it.
#[tokio::test]
#[ignore = "run by repo_create_logs_no_credentials with the API logging at trace"]
async fn repo_create_child() -> Result<(), Error> {
    // only do the requests when started by the parent test
    if std::env::var(CHILD_MARKER).is_err() {
        return Ok(());
    }
    // get admin client
    let client = test_utilities::admin_client().await?;
    // create a group to add repos to
    let group = generators::groups(1, &client).await?.remove(0).name;
    // create a repo whose URL authenticates with a username and password
    let url = format!("https://alice:{PASSWORD}@github.com/corn/harvest");
    let req = RepoRequest::new(&url, vec![group.as_str()], None);
    client.repos.create(&req).await?;
    // send creates with authenticated URLs that can't be parsed into a repo
    for bad_url in [
        format!("https://alice:{PASSWORD}@github.com"),
        format!("https://alice:{PASSWORD}@github.com/corn"),
        format!("https://alice:{PASSWORD}@github.com/corn/harvest//main"),
        format!("https://ftp//alice:{PASSWORD}@github.com/corn/harvest"),
    ] {
        let req = RepoRequest::new(&bad_url, vec![group.as_str()], None);
        // the create must fail without quoting the password back
        match client.repos.create(&req).await {
            Ok(_) => return Err(Error::new(format!("a create with {bad_url} succeeded"))),
            Err(err) => assert!(
                !err.to_string().contains(PASSWORD),
                "a failed create returned the password in its error"
            ),
        }
    }
    Ok(())
}

/// Creating repos with authenticated URLs, including ones that fail to parse, logs no
/// credentials in any log line, span field, or error at any level
#[test]
fn repo_create_logs_no_credentials() {
    // run the child test in a new process whose in-process API logs everything to stdout
    let output = Command::new(std::env::current_exe().expect("no test binary path"))
        .args(["--exact", "repo_create_child", "--ignored", "--nocapture"])
        .env(CHILD_MARKER, "1")
        .env("THORIUM__THORIUM__TRACING__LOCAL__LEVEL", "Trace")
        .env("NO_COLOR", "1")
        .output()
        .expect("failed to run the child test");
    // combine everything the child wrote
    let mut logs = String::from_utf8_lossy(&output.stdout).into_owned();
    logs.push_str(&String::from_utf8_lossy(&output.stderr));
    // the requests must have run, or nothing was checked
    assert!(
        output.status.success(),
        "the child test failed: {}",
        logs.replace(PASSWORD, "<password>")
    );
    // make sure the create was logged at all so the capture is known to work
    assert!(
        logs.contains("db::repos::create") && logs.contains("github.com/corn/harvest"),
        "the API's create logs were not captured"
    );
    // find every line that carries the password without printing the password itself,
    // including the partition keys the Scylla driver's trace spans print
    let leaks = logs
        .lines()
        .filter(|line| line.contains(PASSWORD))
        .map(|line| line.replace(PASSWORD, "<password>"))
        .collect::<Vec<String>>();
    assert!(
        leaks.is_empty(),
        "{} log lines carry the password:\n{}",
        leaks.len(),
        leaks.join("\n")
    );
}
