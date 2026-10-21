//! Tests the repos routes in Thorium

use std::collections::HashSet;

use thorium::models::{
    GroupUpdate, GroupUsersUpdate, RepoCheckout, RepoListLine, RepoListOpts, RepoRequest,
};
use thorium::test_utilities::{self, generators};
use thorium::{contains, fail, is, is_desc, Error};

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
