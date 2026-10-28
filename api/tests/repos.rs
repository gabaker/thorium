//! Tests the repos routes in Thorium

use chrono::{DateTime, Duration, Utc};
use std::collections::{HashMap, HashSet};

use thorium::models::{
    BranchRequest, CommitListOpts, CommitRequest, CommitishMapRequest, CommitishRequest,
    GroupUpdate, GroupUsersUpdate, RepoCheckout, RepoDownloadOpts, RepoListLine, RepoListOpts,
    RepoRequest, TarredRepo,
};
use thorium::test_utilities::{self, generators};
use thorium::{Error, contains, fail, is, is_desc};

#[tokio::test]
async fn create() -> Result<(), Error> {
    const REPO_URL: &str = "github.com/servo/rust-url";
    // Add "https://", ".git", and "/", all of which should be removed from the
    // unique URL in the backend on repo creation
    const REPO_URL2: &str = "https://github.com/rust-lang/rust.git/";
    const REPO_URL2_NORMALIZED: &str = "github.com/rust-lang/rust";
    // A nested repo has multiple paths for the project name
    const NESTED_REPO_URL: &str = "https://github.com/user/project/nested.git/";
    const NESTED_REPO_URL_NORMALIZED: &str = "github.com/user/project/nested";
    // Get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // Create a repo
    let req = RepoRequest::new(
        REPO_URL,
        vec![group.clone()],
        Some(RepoCheckout::branch("main")),
    )
    .tag("TestKey", "TestValue")
    .trigger_depth(4);
    let resp = client.repos.create(&req).await?;
    // Ensure the response URL is the one we gave
    is!(resp.url, REPO_URL);
    // Ensure the components of the repo url were assigned properly
    let repo = client.repos.get(REPO_URL).await?;
    let url_components: Vec<&str> = REPO_URL.split('/').collect();
    is!(repo.provider, url_components[0]);
    is!(repo.user, url_components[1]);
    is!(repo.name, url_components[2]);
    // Create a repo with a non-normalized name (includes schema and ".git/")
    let req = RepoRequest::new(REPO_URL2, vec![group.clone()], None).tag("TestKey", "TestValue");
    let resp = client.repos.create(&req).await?;
    // Ensure the response URL is normalized
    is!(resp.url, REPO_URL2_NORMALIZED);
    // Check that a nested repo is parsed correctly
    let req = RepoRequest::new(NESTED_REPO_URL, vec![group.clone()], None);
    let resp = client.repos.create(&req).await?;
    is!(resp.url, NESTED_REPO_URL_NORMALIZED);
    let repo = client.repos.get(NESTED_REPO_URL_NORMALIZED).await?;
    let url_components: Vec<&str> = NESTED_REPO_URL_NORMALIZED.split('/').collect();
    is!(repo.provider, url_components[0]);
    is!(repo.user, url_components[1]);
    is!(repo.name, url_components[2..].join("/"));
    Ok(())
}

// Test for various repo creation failures
#[tokio::test]
async fn create_fail() -> Result<(), Error> {
    // Get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // Create a user
    let user_client = generators::client(&client).await?;
    // Create a repo as a user who is not a member of the given group
    let req = RepoRequest::new(
        "github.com/servo/rust-url",
        vec![group.clone()],
        Some(RepoCheckout::branch("main")),
    );
    let resp = user_client.repos.create(&req).await;
    // Expect a 401
    fail!(resp, 401);
    // Create a repo as a user with no write access to the given group
    let user = user_client.users.info().await?.username;
    let group_update =
        GroupUpdate::default().monitors(GroupUsersUpdate::default().direct_add(&user));
    client.groups.update(&group, &group_update).await?;
    let resp = user_client.repos.create(&req).await;
    // Expect a 401
    fail!(resp, 401);
    // Give the user write access
    let group_update = GroupUpdate::default().users(GroupUsersUpdate::default().direct_add(user));
    client.groups.update(&group, &group_update).await?;
    // Create an invalid request with a URL with no host
    let req = RepoRequest::new("https://", vec![group.clone()], None);
    let resp = user_client.repos.create(&req).await;
    fail!(resp, 400, "EmptyHost");
    // Create an invalid request with no username portion
    let req = RepoRequest::new("github.com", vec![group.clone()], None);
    let resp = user_client.repos.create(&req).await;
    fail!(resp, 400, "does not contain a username");
    // Create an invalid request with no project name
    let req = RepoRequest::new("github.com/test-username", vec![group.clone()], None);
    let resp = user_client.repos.create(&req).await;
    fail!(resp, 400, "does not contain a project name");
    // Create an invalid request with an invalid url scheme
    // This will always succeed because "https://" is always prepended to the URL;
    // uncomment this test if functionality to properly test url schemes is added
    //let req = RepoRequest::new("ftp://github.com/username/project", vec![group.clone()]);
    //let resp = user_client.repos.create(&req).await;
    //fail!(resp, 400, "Invalid url scheme/base");
    // Create an invalid request with empty paths in the URL
    let req = RepoRequest::new(
        "github.com/user//project//nested///repo.git///",
        vec![group.clone()],
        None,
    );
    let resp = user_client.repos.create(&req).await;
    fail!(resp, 400, "contains empty path components");
    Ok(())
}

#[tokio::test]
async fn get() -> Result<(), Error> {
    const REPO_URL: &str = "github.com/chronotope/chrono";
    // Get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // Create a repo
    let req = RepoRequest::new(
        REPO_URL,
        vec![group.clone()],
        Some(RepoCheckout::branch("main")),
    )
    .tag("TestKey", "TestValue")
    .tag("TestKey", "TestValue2")
    .tag("TestKey2", "TestValue");
    client.repos.create(&req).await?;
    let repo = client.repos.get(REPO_URL).await?;
    // This equals check may fail if the method of constructing a repo URL is changed.
    // See implementation of PartialEq<RepoRequest> for Repo which depends on ".git/"
    // and "https://" being trimmed from the url when saved to the backend.
    is!(repo, req);
    Ok(())
}

#[tokio::test]
async fn get_fail() -> Result<(), Error> {
    const REPO_URL: &str = "github.com/serde-rs/serde";
    // Get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // Create a repo
    let req = RepoRequest::new(
        REPO_URL,
        vec![group.clone()],
        Some(RepoCheckout::branch("main")),
    )
    .tag("TestKey", "TestValue")
    .tag("TestKey", "TestValue2")
    .tag("TestKey2", "TestValue");
    client.repos.create(&req).await?;
    // Fail to retrieve the repo as a user with no access
    let user_client = generators::client(&client).await?;
    let resp = user_client.repos.get(REPO_URL).await;
    // Expect a "NOT FOUND" error
    fail!(resp, 404);
    // Fail to retrieve the repo with an extra ".git" extension
    let resp = client.repos.get(&format!("{REPO_URL}.git")).await;
    fail!(resp, 404);
    // Fail to retrieve the repo with an extra "/"
    let resp = client.repos.get(&format!("{REPO_URL}/")).await;
    fail!(resp, 404);
    // Fail to retrieve the repo with an extra ".git/"
    let resp = client.repos.get(&format!("{REPO_URL}.git/")).await;
    // Expect a "NOT FOUND" error
    fail!(resp, 404);
    Ok(())
}

#[tokio::test]
async fn list() -> Result<(), Error> {
    // Get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // Create many random repos with random tags
    let repos: HashSet<String> = generators::repos(&group, 100, &client)
        .await?
        .into_iter()
        .map(|req| req.url)
        .collect();
    // Get all repos in this group, ensuring pagination works
    let opts = RepoListOpts::default()
        .groups(vec![group.clone()])
        .page_size(50);
    let mut cursor = client.repos.list(&opts).await?;
    let mut repo_lines: Vec<RepoListLine> = Vec::new();
    loop {
        // Make sure the cursor is in descending order
        is_desc!(cursor.data);
        repo_lines.append(&mut cursor.data);
        if cursor.exhausted() {
            break;
        }
        // Get the next page
        cursor.refill().await?;
    }
    // Check that every line is one of our requests, tagged or untagged
    for line in repo_lines {
        contains!(repos, &line.url);
    }
    Ok(())
}

#[tokio::test]
async fn list_tagged() -> Result<(), Error> {
    // Get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // Create many random repos with the same tag
    let (key, value, repo_reqs_tagged) = generators::repos_tagged(&group, 50, &client).await?;
    // Create many random repos with random tags
    let _repo_reqs = generators::repos(&group, 50, &client).await?;
    // Only get repos that were tagged
    let opts = RepoListOpts::default()
        .groups(vec![group.clone()])
        .tag(key, value)
        .page_size(100);
    let cursor = client.repos.list(&opts).await?;
    // Ensure that only repos with the given tag were retrieved
    let repos_tagged: HashSet<String> = repo_reqs_tagged.into_iter().map(|req| req.url).collect();
    for line in &cursor.data {
        contains!(repos_tagged, &line.url);
    }
    Ok(())
}

/// Opens a Scylla session to the test cluster, to read rows the API's routes never return
async fn scylla_session() -> scylla::client::session::Session {
    // connect to every node the test config lists, with its credentials when it has them
    let conf = &*test_utilities::CONF;
    let mut builder =
        scylla::client::session_builder::SessionBuilder::new().known_nodes(&conf.scylla.nodes);
    if let Some(auth) = &conf.scylla.auth {
        builder = builder.user(&auth.username, &auth.password);
    }
    builder.build().await.expect("failed to connect to Scylla")
}

/// Changing the default checkout of a repo submitted to several groups by several submissions
/// writes no repo rows that lack the repo's own columns
#[tokio::test]
async fn resubmitting_writes_no_partial_repo_rows() -> Result<(), Error> {
    // Get admin client and two groups
    let client = test_utilities::admin_client().await?;
    let mut groups = generators::groups(2, &client).await?;
    let (first, second) = (groups.remove(0).name, groups.remove(0).name);
    let url = format!("github.com/thorium-tests/{}", generators::gen_string(12));
    // Submit the repo to one group, then to both, then to both again with another checkout
    for (groups, branch) in [
        (vec![first.clone()], "main"),
        (vec![first.clone(), second.clone()], "main"),
        (vec![first.clone(), second.clone()], "dev"),
    ] {
        let req = RepoRequest::new(&url, groups, Some(RepoCheckout::branch(branch)));
        client.repos.create(&req).await?;
    }
    // The repo takes the latest default checkout
    let repo = client.repos.get(&url).await?;
    is!(repo.default_checkout, Some(RepoCheckout::branch("dev")));
    // Every row of the repo list for these groups holds the repo's url
    let session = scylla_session().await;
    let query = format!(
        "SELECT group, url FROM {}.repos_list",
        test_utilities::CONF.thorium.namespace
    );
    let rows = session
        .query_unpaged(query, ())
        .await
        .expect("failed to read the repo list")
        .into_rows_result()
        .expect("the repo list returned no rows");
    let partial: Vec<String> = rows
        .rows::<(String, Option<String>)>()
        .expect("unexpected repo list columns")
        .filter_map(Result::ok)
        .filter(|(group, url)| url.is_none() && (*group == first || *group == second))
        .map(|(group, _)| group)
        .collect();
    assert!(partial.is_empty(), "repo rows without a url in {partial:?}");
    Ok(())
}

/// Upload a repo data blob holding some content and return its sha256
///
/// # Arguments
///
/// * `client` - The client to upload with
/// * `repo` - The repo to upload data for
/// * `group` - The group to share this data with
/// * `content` - The content of this data blob
async fn upload_repo_data(
    client: &thorium::Thorium,
    repo: &str,
    group: &str,
    content: &str,
) -> Result<String, Error> {
    // write our content to a file of its own
    let path =
        std::env::temp_dir().join(format!("thorium-repo-data-{}", generators::gen_string(16)));
    tokio::fs::write(&path, content).await?;
    // upload this file as the repo's data
    let tar = TarredRepo {
        name: "data".to_owned(),
        repo: path.clone(),
    };
    let resp = client.repos.upload(repo, tar, vec![group.to_owned()]).await;
    // remove our file before checking the upload
    tokio::fs::remove_file(&path).await?;
    Ok(resp?.sha256)
}

/// Build a commit batch for one commit at a timestamp
///
/// # Arguments
///
/// * `group` - The group to save this commit to
/// * `hash` - The hash of this commit
/// * `timestamp` - When this commit was made
/// * `end` - Whether this is the final batch of commits
/// * `earliest` - The earliest commit in this repo, if known
fn commit_batch(
    group: &str,
    hash: &str,
    timestamp: DateTime<Utc>,
    end: bool,
    earliest: Option<DateTime<Utc>>,
) -> CommitishMapRequest {
    // build our single commit
    let commit = CommitishRequest::Commit(CommitRequest {
        author: "io".to_owned(),
        timestamp,
        topic: None,
        description: None,
        truncated: false,
    });
    CommitishMapRequest {
        groups: vec![group.to_owned()],
        earliest,
        end,
        commitishes: HashMap::from([(hash.to_owned(), commit)]),
    }
}

/// A repo data blob whose commits are a few hours old survives the prune at the end of a
/// later batch of commits for another blob
#[tokio::test]
async fn prune_keeps_data_with_recent_commits() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // create a group and a repo in it
    let group = generators::groups(1, &client).await?.remove(0).name;
    let repo = generators::repos(&group, 1, &client).await?.remove(0).url;
    // date our commits relative to now
    let now = Utc::now();
    // upload a first blob with a commit from two hours ago as the final batch
    let first = upload_repo_data(&client, &repo, &group, "first blob").await?;
    let two_hours = now - Duration::hours(2);
    let batch = commit_batch(&group, "c1", two_hours, true, Some(two_hours));
    let resp = client.repos.add_commits(&repo, &first, &batch).await?;
    is!(resp.status().as_u16(), 204);
    // upload a second blob with a commit from an hour ago, whose final batch prunes old blobs
    let second = upload_repo_data(&client, &repo, &group, "second blob").await?;
    let batch = commit_batch(&group, "c2", now - Duration::hours(1), true, None);
    let resp = client.repos.add_commits(&repo, &second, &batch).await?;
    is!(resp.status().as_u16(), 204);
    // the first blob is still referenced by c1, so commits can still be added to it
    let batch = commit_batch(&group, "c3", now - Duration::minutes(30), false, None);
    let resp = client.repos.add_commits(&repo, &first, &batch).await;
    assert!(
        resp.is_ok(),
        "the first blob was pruned while c1 still references it: {resp:?}"
    );
    Ok(())
}

/// Creating a repo again as the same user and groups, with or without a scheme, reuses the
/// existing submission for that scheme instead of adding another
#[tokio::test]
async fn create_reuses_matching_submissions() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // create a group to add repos to
    let group = generators::groups(1, &client).await?.remove(0).name;
    let path = format!("github.com/corn/{}", generators::gen_string(12));
    // create the repo three times with the same credentials, with and without a scheme
    for url in [
        format!("https://alice:CornSilkPw7Q9z@{path}"),
        format!("alice:CornSilkPw7Q9z@{path}"),
        format!("https://alice:CornSilkPw7Q9z@{path}"),
    ] {
        let req = RepoRequest::new(&url, vec![group.as_str()], None);
        client.repos.create(&req).await?;
    }
    // they share one submission
    let repo = client.repos.get(&path).await?;
    assert_eq!(repo.submissions.len(), 1, "authenticated creates");
    // create the repo three times without credentials, with and without a scheme
    for url in [
        format!("https://{path}"),
        path.clone(),
        format!("https://{path}"),
    ] {
        let req = RepoRequest::new(&url, vec![group.as_str()], None);
        client.repos.create(&req).await?;
    }
    // they share a second submission, since they pull the repo without credentials
    let repo = client.repos.get(&path).await?;
    assert_eq!(repo.submissions.len(), 2, "plain creates");
    Ok(())
}

/// Create a repo without a default checkout in some groups and upload a data blob to all of them
///
/// Returns the repo's url and the sha256 of its data blob.
///
/// # Arguments
///
/// * `client` - The client to create the repo with
/// * `groups` - The groups to create the repo and its data in
async fn repo_with_data(
    client: &thorium::Thorium,
    groups: &[String],
) -> Result<(String, String), Error> {
    // create the repo with no default checkout, so downloads fall back to its latest commit
    let url = format!("github.com/thorium-tests/{}", generators::gen_string(12));
    let req = RepoRequest::new(&url, groups.to_vec(), None);
    client.repos.create(&req).await?;
    // write a data blob to a file of its own
    let path =
        std::env::temp_dir().join(format!("thorium-repo-data-{}", generators::gen_string(16)));
    tokio::fs::write(&path, "repo data").await?;
    // upload this file as the repo's data for every group
    let tar = TarredRepo {
        name: "data".to_owned(),
        repo: path.clone(),
    };
    let resp = client.repos.upload(&url, tar, groups.to_vec()).await;
    // remove our file before checking the upload
    tokio::fs::remove_file(&path).await?;
    Ok((url, resp?.sha256))
}

/// Build a commit request at a timestamp
///
/// # Arguments
///
/// * `timestamp` - When this commit was made
fn commit_at(timestamp: DateTime<Utc>) -> CommitishRequest {
    CommitishRequest::Commit(CommitRequest {
        author: "io".to_owned(),
        timestamp,
        topic: None,
        description: None,
        truncated: false,
    })
}

/// List the keys of every commitish of a repo in some groups with default options
///
/// # Arguments
///
/// * `client` - The client to list with
/// * `repo` - The repo to list commitishes for
/// * `groups` - The groups to list commitishes from
async fn list_commitish_keys(
    client: &thorium::Thorium,
    repo: &str,
    groups: &[String],
) -> Result<HashSet<String>, Error> {
    // list with default options in just these groups
    let opts = CommitListOpts {
        groups: groups.to_vec(),
        ..CommitListOpts::default()
    };
    let mut cursor = client.repos.list_commitishes(repo, &opts).await?;
    // collect the keys on every page until the cursor is exhausted
    let mut keys: HashSet<String> = cursor.data.drain(..).map(|c| c.key().clone()).collect();
    while !cursor.exhausted() {
        cursor.refill().await?;
        keys.extend(cursor.data.drain(..).map(|c| c.key().clone()));
    }
    Ok(keys)
}

/// Get a timestamp a few hours ago in whole seconds, so it is stored exactly
fn whole_seconds_ago() -> DateTime<Utc> {
    // drop the sub-second part of a timestamp from two hours ago
    let then = Utc::now() - Duration::hours(2);
    DateTime::from_timestamp(then.timestamp(), 0).expect("a valid timestamp")
}

/// A default commitish listing includes the commit made at the repo's earliest timestamp
#[tokio::test]
async fn commitishes_list_the_earliest_commit() -> Result<(), Error> {
    // get admin client, a group, and a repo with data in it
    let client = test_utilities::admin_client().await?;
    let groups = vec![generators::groups(1, &client).await?.remove(0).name];
    let (repo, data) = repo_with_data(&client, &groups).await?;
    // add a first commit at the repo's earliest timestamp and a second an hour later
    let earliest = whole_seconds_ago();
    let map = CommitishMapRequest {
        groups: groups.clone(),
        earliest: Some(earliest),
        end: true,
        commitishes: HashMap::from([
            ("first".to_owned(), commit_at(earliest)),
            (
                "second".to_owned(),
                commit_at(earliest + Duration::hours(1)),
            ),
        ]),
    };
    client.repos.add_commits(&repo, &data, &map).await?;
    // a default listing returns both commits
    let keys = list_commitish_keys(&client, &repo, &groups).await?;
    let expected: HashSet<String> = ["first", "second"].map(str::to_owned).into();
    is!(keys, expected);
    Ok(())
}

/// A repo whose commits were all made at its earliest timestamp lists them all and downloads
/// its latest commit when no commitish is given
#[tokio::test]
async fn commits_at_the_earliest_timestamp_are_found() -> Result<(), Error> {
    // get admin client, a group, and a repo with data in it
    let client = test_utilities::admin_client().await?;
    let groups = vec![generators::groups(1, &client).await?.remove(0).name];
    let (repo, data) = repo_with_data(&client, &groups).await?;
    // add two commits, both at the repo's earliest timestamp
    let earliest = whole_seconds_ago();
    let map = CommitishMapRequest {
        groups: groups.clone(),
        earliest: Some(earliest),
        end: true,
        commitishes: HashMap::from([
            ("only-a".to_owned(), commit_at(earliest)),
            ("only-b".to_owned(), commit_at(earliest)),
        ]),
    };
    client.repos.add_commits(&repo, &data, &map).await?;
    // a default listing returns both commits
    let keys = list_commitish_keys(&client, &repo, &groups).await?;
    let expected: HashSet<String> = ["only-a", "only-b"].map(str::to_owned).into();
    is!(keys, expected);
    // downloading without a commitish resolves the latest commit
    let dir = std::env::temp_dir().join(format!("thorium-repo-dl-{}", generators::gen_string(12)));
    tokio::fs::create_dir_all(&dir).await?;
    let download = client
        .repos
        .download(&repo, &RepoDownloadOpts::default(), &dir)
        .await;
    tokio::fs::remove_dir_all(&dir).await?;
    assert!(
        download.is_ok(),
        "download without a commitish: {download:?}"
    );
    Ok(())
}

/// A commit visible to only one of a repo's 40 groups still downloads, and a key that is a
/// branch in one group and a commit in another is still ambiguous
#[tokio::test]
async fn download_finds_commits_in_some_of_many_groups() -> Result<(), Error> {
    // get admin client, 40 groups, and a repo with data in all of them
    let client = test_utilities::admin_client().await?;
    let groups: Vec<String> = generators::groups(40, &client)
        .await?
        .into_iter()
        .map(|group| group.name)
        .collect();
    let (repo, data) = repo_with_data(&client, &groups).await?;
    let timestamp = whole_seconds_ago();
    // add c_all to every group as the final batch
    let map = CommitishMapRequest {
        groups: groups.clone(),
        earliest: Some(timestamp - Duration::hours(1)),
        end: true,
        commitishes: HashMap::from([("c_all".to_owned(), commit_at(timestamp))]),
    };
    client.repos.add_commits(&repo, &data, &map).await?;
    // add c_one and a branch named amb to only the first group
    let branch = CommitishRequest::Branch(BranchRequest {
        commit: "c_all".to_owned(),
        timestamp,
    });
    let map = CommitishMapRequest {
        groups: vec![groups[0].clone()],
        earliest: None,
        end: false,
        commitishes: HashMap::from([
            ("c_one".to_owned(), commit_at(timestamp)),
            ("amb".to_owned(), branch),
        ]),
    };
    client.repos.add_commits(&repo, &data, &map).await?;
    // add a commit named amb to only the last group
    let map = CommitishMapRequest {
        groups: vec![groups[39].clone()],
        earliest: None,
        end: false,
        commitishes: HashMap::from([("amb".to_owned(), commit_at(timestamp))]),
    };
    client.repos.add_commits(&repo, &data, &map).await?;
    // download into a directory of our own
    let dir = std::env::temp_dir().join(format!("thorium-repo-dl-{}", generators::gen_string(12)));
    tokio::fs::create_dir_all(&dir).await?;
    let one = client
        .repos
        .download(&repo, &RepoDownloadOpts::default().commitish("c_one"), &dir)
        .await;
    let all = client
        .repos
        .download(&repo, &RepoDownloadOpts::default().commitish("c_all"), &dir)
        .await;
    let amb = client
        .repos
        .download(&repo, &RepoDownloadOpts::default().commitish("amb"), &dir)
        .await;
    tokio::fs::remove_dir_all(&dir).await?;
    // commits in one group or in every group download
    assert!(one.is_ok(), "download of c_one: {one:?}");
    assert!(all.is_ok(), "download of c_all: {all:?}");
    // a key that is a branch and a commit in different groups is ambiguous
    match amb {
        Ok(_) => panic!("download of the ambiguous key amb succeeded"),
        Err(error) => is!(error.status().map(|code| code.as_u16()), Some(400)),
    }
    Ok(())
}

/// Commitish details list a commit saved to all of a repo's 101 groups
#[tokio::test]
async fn commitish_details_in_more_than_100_groups() -> Result<(), Error> {
    // get admin client, 101 groups, and a repo with data in all of them
    let client = test_utilities::admin_client().await?;
    let groups: Vec<String> = generators::groups(101, &client)
        .await?
        .into_iter()
        .map(|group| group.name)
        .collect();
    let (repo, data) = repo_with_data(&client, &groups).await?;
    // add a commit to every group
    let timestamp = whole_seconds_ago();
    let map = CommitishMapRequest {
        groups: groups.clone(),
        earliest: Some(timestamp - Duration::hours(1)),
        end: true,
        commitishes: HashMap::from([("wide".to_owned(), commit_at(timestamp))]),
    };
    client.repos.add_commits(&repo, &data, &map).await?;
    // list the repo's commitish details in all of those groups
    let opts = CommitListOpts {
        groups: groups.clone(),
        ..CommitListOpts::default()
    };
    let cursor = client.repos.list_commitish_details(&repo, &opts).await?;
    // the commit is listed with its details
    let keys: Vec<&String> = cursor
        .data
        .iter()
        .map(thorium::models::CommitishDetails::key)
        .collect();
    is!(keys, vec![&"wide".to_owned()]);
    Ok(())
}

/// A non-admin's 100 or more groups that can't see a repo are refused, like fewer groups are
#[tokio::test]
async fn authorize_refuses_repos_hidden_from_many_groups() -> Result<(), Error> {
    // get admin client, a group, and a repo in it
    let client = test_utilities::admin_client().await?;
    let owner = generators::groups(1, &client).await?.remove(0).name;
    let repo = generators::repos(&owner, 1, &client).await?.remove(0).url;
    // build the shared objects the backend authorizes with
    let shared = Box::pin(thorium::utils::Shared::new(test_utilities::CONF.clone())).await;
    // build groups that don't hold the repo
    let others = |count: usize| -> Vec<String> {
        (0..count)
            .map(|_| format!("vesta-{}", generators::gen_string(10).to_lowercase()))
            .collect()
    };
    let authorize = |groups: Vec<String>, repos: Vec<String>| {
        let shared = &shared;
        async move { thorium::models::backends::db::repos::authorize(&groups, &repos, shared).await }
    };
    // 99 and 100 groups without the repo are both refused
    for count in [99, 100] {
        let res = authorize(others(count), vec![repo.clone()]).await;
        let code = res.err().map(|error| error.status().as_u16());
        is!(code, Some(401));
    }
    // 100 groups including the repo's group are allowed
    let mut groups = others(99);
    groups.push(owner.clone());
    assert!(authorize(groups, vec![repo.clone()]).await.is_ok());
    // a repo listed twice with 50 groups including its own is allowed
    let mut groups = others(49);
    groups.push(owner.clone());
    let res = authorize(groups, vec![repo.clone(), repo.clone()]).await;
    assert!(res.is_ok(), "a repeated repo was refused: {res:?}");
    Ok(())
}
