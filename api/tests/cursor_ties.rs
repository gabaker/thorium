//! Tests that listings page through rows sharing a timestamp without skipping or repeating any

use base64::Engine;
use chrono::{Duration, TimeZone, Utc};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

use thorium::models::{
    AssociationKind, AssociationRequest, AssociationTarget, BranchRequest, Buffer, CommitRequest,
    CommitishMapRequest, CommitishRequest, EntityRequest, GitTagRequest, GroupUpdate,
    GroupUsersUpdate, RepoRequest, Resources, SampleRequest, ScopedTokenRequest, TarredRepo,
};
use thorium::test_utilities::{self, generators};
use thorium::{Error, Thorium};

/// The most pages a listing may take before it is treated as never ending
const MAX_PAGES: usize = 50;

/// Send a GET to the API with this client's credentials and return its JSON body
///
/// # Arguments
///
/// * `client` - The client whose credentials to send
/// * `path` - The path and query to request, starting with `/api/`
async fn get_json(client: &Thorium, path: &str) -> Result<Value, Error> {
    // get the token for this client
    let token = client.users.info().await?.token;
    // build the auth header the client would send
    let encoded = base64::engine::general_purpose::STANDARD.encode(token.as_bytes());
    // send our raw request so the cursor and limit reach the API unchanged
    let resp = reqwest::Client::new()
        .get(format!("{}{path}", client.host))
        .header("authorization", format!("token {encoded}"))
        .send()
        .await?;
    // fail on any error status
    let status = resp.status();
    let body = resp.text().await?;
    assert!(status.is_success(), "{path} returned {status}: {body}");
    Ok(serde_json::from_str(&body)?)
}

/// Send a GET to the API with this client's credentials and return its status code
///
/// # Arguments
///
/// * `client` - The client whose credentials to send
/// * `path` - The path and query to request, starting with `/api/`
async fn status_of(client: &Thorium, path: &str) -> Result<u16, Error> {
    // build the auth header the client would send
    let token = client.users.info().await?.token;
    let encoded = base64::engine::general_purpose::STANDARD.encode(token.as_bytes());
    // send our raw request so the cursor reaches the API unchanged
    let resp = reqwest::Client::new()
        .get(format!("{}{path}", client.host))
        .header("authorization", format!("token {encoded}"))
        .send()
        .await?;
    Ok(resp.status().as_u16())
}

/// Write a value under a key in Redis
///
/// # Arguments
///
/// * `key` - The key
/// * `value` - The value
async fn redis_set(key: &str, value: &str) -> Result<(), Error> {
    let redis = test_utilities::redis_pool().await;
    let mut conn = redis
        .get()
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    let (): () = bb8_redis::redis::cmd("SET")
        .arg(key)
        .arg(value)
        .query_async(&mut *conn)
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    Ok(())
}

/// Page a listing until its cursor is exhausted and return every row in order
///
/// Panics if the listing takes more than [`MAX_PAGES`] pages.
///
/// # Arguments
///
/// * `client` - The client to list with
/// * `path` - The listing's path and query, without its limit or cursor
/// * `limit` - The page size to request
async fn page_all(client: &Thorium, path: &str, limit: usize) -> Result<Vec<Value>, Error> {
    // page with the limit parameter most listings use
    page_all_by(client, path, "limit", limit).await
}

/// Page a listing whose page size has a given parameter name until its cursor is exhausted
///
/// Panics if the listing takes more than [`MAX_PAGES`] pages.
///
/// # Arguments
///
/// * `client` - The client to list with
/// * `path` - The listing's path and query, without its page size or cursor
/// * `param` - The name of the page size parameter
/// * `limit` - The page size to request
async fn page_all_by(
    client: &Thorium,
    path: &str,
    param: &str,
    limit: usize,
) -> Result<Vec<Value>, Error> {
    // pick the separator between the existing query and our params
    let sep = if path.contains('?') { '&' } else { '?' };
    // collect the rows of every page
    let mut rows = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_PAGES {
        // request the next page, continuing from our cursor if we have one
        let page_path = match &cursor {
            Some(id) => format!("{path}{sep}{param}={limit}&cursor={id}"),
            None => format!("{path}{sep}{param}={limit}"),
        };
        let page = get_json(client, &page_path).await?;
        // a page never holds more rows than its limit
        let data = page["data"].as_array().cloned().unwrap_or_default();
        assert!(
            data.len() <= limit,
            "{path} returned {} rows for a limit of {limit}",
            data.len()
        );
        rows.extend(data);
        // stop once the cursor is exhausted
        match page["cursor"].as_str() {
            Some(id) => cursor = Some(id.to_owned()),
            None => return Ok(rows),
        }
    }
    panic!(
        "{path} with limit {limit} never ended after {MAX_PAGES} pages ({} rows)",
        rows.len()
    );
}

/// Check that a listing returned each expected key exactly once
///
/// # Arguments
///
/// * `rows` - The rows the listing returned
/// * `key` - The field that identifies a row
/// * `expected` - The keys the listing should return
fn assert_each_once(rows: &[Value], key: &str, expected: &HashSet<String>) {
    // count how often each key was returned
    let mut seen: HashMap<String, usize> = HashMap::new();
    for row in rows {
        let value = match &row[key] {
            Value::String(value) => value.clone(),
            other => other.to_string(),
        };
        *seen.entry(value).or_default() += 1;
    }
    // every expected key was returned once
    let missing: Vec<&String> = expected
        .iter()
        .filter(|key| !seen.contains_key(*key))
        .collect();
    let repeated: Vec<(&String, &usize)> = seen.iter().filter(|(_, count)| **count > 1).collect();
    let unexpected: Vec<&String> = seen.keys().filter(|key| !expected.contains(*key)).collect();
    assert!(
        missing.is_empty() && repeated.is_empty() && unexpected.is_empty(),
        "missing {missing:?}, repeated {repeated:?}, unexpected {unexpected:?} in {} rows",
        rows.len()
    );
}

/// Copy an entity's row in the entities table into new rows that share its timestamp
///
/// The copies keep the template row's partition and created timestamp, so the census already
/// covers them. Returns the ids of the new rows.
///
/// # Arguments
///
/// * `template` - The id of the entity whose row to copy
/// * `at` - The id of the entity whose year, bucket, and timestamp to use
/// * `count` - The number of copies to make
async fn copy_entity_rows(template: Uuid, at: Uuid, count: usize) -> Result<Vec<Uuid>, Error> {
    // connect to scylla
    let session = test_utilities::scylla_session().await?;
    let ns = &test_utilities::CONF.thorium.namespace;
    // read a row of an entity as JSON
    let read = format!("SELECT JSON * FROM {ns}.entities WHERE id = ? ALLOW FILTERING");
    let row_of = |id: Uuid| {
        let session = &session;
        let read = read.clone();
        async move {
            let rows = session
                .query_unpaged(read, (id,))
                .await
                .map_err(|err| Error::new(err.to_string()))?
                .into_rows_result()
                .map_err(|err| Error::new(err.to_string()))?;
            let (json,) = rows
                .first_row::<(String,)>()
                .map_err(|err| Error::new(err.to_string()))?;
            Ok::<Value, Error>(serde_json::from_str(&json)?)
        }
    };
    let mut row = row_of(template).await?;
    let target = row_of(at).await?;
    // move the copies into the target's year, bucket, and timestamp
    for field in ["year", "bucket", "created"] {
        row[field] = target[field].clone();
    }
    // insert each copy with a new id
    let insert = format!("INSERT INTO {ns}.entities JSON ?");
    let mut ids = Vec::with_capacity(count);
    for index in 0..count {
        let id = Uuid::new_v4();
        row["id"] = Value::String(id.to_string());
        row["name"] = Value::String(format!("tie-{index}-{id}"));
        session
            .query_unpaged(insert.clone(), (row.to_string(),))
            .await
            .map_err(|err| Error::new(err.to_string()))?;
        ids.push(id);
    }
    Ok(ids)
}

/// One association request with more targets than a page shares one timestamp, and paging it
/// returns every target once and ends
#[tokio::test]
async fn association_ties_page_through() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // build a group and a file to associate from
    let group = generators::groups(1, &client).await?.remove(0).name;
    let sha256 = client
        .files
        .create(generators::gen_sample(&group))
        .await?
        .sha256;
    // associate the file with 60 repos in one request, so every row shares one timestamp
    let targets: Vec<String> = (0..60)
        .map(|i| format!("provider.tld/ties/repo{i:02}"))
        .collect();
    let mut req = AssociationRequest::new(
        AssociationKind::AssociatedWith,
        AssociationTarget::File(sha256.clone()),
    )
    .groups(vec![group.clone()]);
    for target in &targets {
        req = req.target(AssociationTarget::Repo(target.clone()));
    }
    client.associations.create(&req).await?;
    // page the file's associations 50 at a time
    let rows = page_all(&client, &format!("/api/files/associations/{sha256}"), 50).await?;
    // every target comes back once
    let others: Vec<Value> = rows
        .iter()
        .map(|row| serde_json::json!({"other": row["other"]["Repo"]}))
        .collect();
    assert_each_once(&others, "other", &targets.into_iter().collect());
    Ok(())
}

/// Association rows in two groups that share the page-boundary timestamp each come back once
#[tokio::test]
async fn association_ties_across_groups() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // build two groups and a file in both to associate from
    let groups: Vec<String> = generators::groups(2, &client)
        .await?
        .into_iter()
        .map(|group| group.name)
        .collect();
    let mut sample = generators::gen_sample(&groups[0]);
    sample.groups.push(groups[1].clone());
    let sha256 = client.files.create(sample).await?.sha256;
    // associate the file with 9 repos in both groups in one request
    let targets: Vec<String> = (0..9)
        .map(|i| format!("provider.tld/groups/repo{i}"))
        .collect();
    let mut req = AssociationRequest::new(
        AssociationKind::AssociatedWith,
        AssociationTarget::File(sha256.clone()),
    )
    .groups(groups.clone());
    for target in &targets {
        req = req.target(AssociationTarget::Repo(target.clone()));
    }
    client.associations.create(&req).await?;
    // page the associations with boundaries inside the tied rows
    for limit in [2, 4] {
        let rows = page_all(&client, &format!("/api/files/associations/{sha256}"), limit).await?;
        let others: Vec<Value> = rows
            .iter()
            .map(|row| serde_json::json!({"other": row["other"]["Repo"]}))
            .collect();
        assert_each_once(&others, "other", &targets.iter().cloned().collect());
    }
    Ok(())
}

/// More entities than a page that share one created timestamp page through with each entity
/// returned once and the cursor ending
#[tokio::test]
async fn entity_ties_page_through() -> Result<(), Error> {
    // get admin client and a group to list in
    let client = test_utilities::admin_client().await?;
    let group = generators::groups(1, &client).await?.remove(0).name;
    // create an entity and copy its row into 11 more at the same timestamp
    let req = generators::gen_entity(&group, generators::gen_other_meta());
    let template = client.entities.create(req).await?.id;
    let mut expected: HashSet<String> = copy_entity_rows(template, template, 11)
        .await?
        .into_iter()
        .map(|id| id.to_string())
        .collect();
    expected.insert(template.to_string());
    // page the group's entities 5 at a time
    let rows = page_all(&client, &format!("/api/entities/?groups[]={group}"), 5).await?;
    assert_each_once(&rows, "id", &expected);
    Ok(())
}

/// Entities of two kinds that share a group and the page-boundary timestamp each come back once
#[tokio::test]
async fn entity_ties_across_kinds() -> Result<(), Error> {
    // get admin client and a group to list in
    let client = test_utilities::admin_client().await?;
    let group = generators::groups(1, &client).await?.remove(0).name;
    // create an entity of each kind
    let other = client
        .entities
        .create(generators::gen_entity(&group, generators::gen_other_meta()))
        .await?
        .id;
    let vendor = generators::vendor_entity(&group, &client).await?.id;
    // copy each kind's row into more rows that all share the newest entity's timestamp
    let mut expected: HashSet<String> = HashSet::from([other.to_string(), vendor.to_string()]);
    for id in copy_entity_rows(other, vendor, 5).await? {
        expected.insert(id.to_string());
    }
    for id in copy_entity_rows(vendor, vendor, 6).await? {
        expected.insert(id.to_string());
    }
    // page both kinds with boundaries inside the tied rows
    for limit in [2, 5] {
        let path = format!("/api/entities/?groups[]={group}&kinds[]=Other&kinds[]=Vendor");
        let rows = page_all(&client, &path, limit).await?;
        assert_each_once(&rows, "id", &expected);
    }
    Ok(())
}

/// Create a repo whose commits c9 and c1 and branches main and a1 all share one timestamp
///
/// Returns the repo's url.
///
/// # Arguments
///
/// * `client` - The client to create the repo with
async fn repo_with_tied_commitishes(client: &Thorium) -> Result<String, Error> {
    // create a group and a repo in it
    let group = generators::groups(1, client).await?.remove(0).name;
    let repo = generators::repos(&group, 1, client).await?.remove(0).url;
    // upload a data blob for our commitishes
    let path = std::env::temp_dir().join(format!("thorium-tie-data-{}", Uuid::new_v4()));
    tokio::fs::write(&path, "tied commitishes").await?;
    let tar = TarredRepo {
        name: "data".to_owned(),
        repo: path.clone(),
    };
    let upload = client.repos.upload(&repo, tar, vec![group.clone()]).await;
    tokio::fs::remove_file(&path).await?;
    let data = upload?.sha256;
    // build commits c9 and c1 and branches main and a1 all at one timestamp
    let timestamp = Utc::now() - Duration::minutes(5);
    let commit = || {
        CommitishRequest::Commit(CommitRequest {
            author: "io".to_owned(),
            timestamp,
            topic: None,
            description: None,
            truncated: false,
        })
    };
    let branch = || {
        CommitishRequest::Branch(BranchRequest {
            commit: "c9".to_owned(),
            timestamp,
        })
    };
    // save them with an earliest commit before them, since listings stop at the earliest
    let map = CommitishMapRequest {
        groups: vec![group],
        earliest: Some(timestamp - Duration::hours(1)),
        end: true,
        commitishes: HashMap::from([
            ("c9".to_owned(), commit()),
            ("c1".to_owned(), commit()),
            ("main".to_owned(), branch()),
            ("a1".to_owned(), branch()),
        ]),
    };
    client.repos.add_commits(&repo, &data, &map).await?;
    Ok(repo)
}

/// Commits and branches that share one timestamp page through with each returned once
///
/// The branch keys sort on both sides of the commit keys, so whichever kind's rows arrive
/// first, a page boundary falls between keys of different kinds.
#[tokio::test]
async fn commitish_ties_page_through() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    let expected: HashSet<String> = ["c9", "c1", "main", "a1"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    for _ in 0..3 {
        // build a repo whose commitishes share one timestamp
        let repo = repo_with_tied_commitishes(&client).await?;
        // page the commitishes with boundaries inside the tied rows
        for limit in [1, 2] {
            let rows = page_all(&client, &format!("/api/repos/commitishes/{repo}"), limit).await?;
            // pull the key out of each commitish, whatever its kind
            let keys: Vec<Value> = rows
                .iter()
                .map(|row| {
                    let inner = row
                        .as_object()
                        .and_then(|map| map.values().next())
                        .unwrap_or(row);
                    let key = inner
                        .get("hash")
                        .or_else(|| inner.get("name"))
                        .cloned()
                        .unwrap_or(Value::Null);
                    serde_json::json!({ "key": key })
                })
                .collect();
            assert_each_once(&keys, "key", &expected);
        }
    }
    Ok(())
}

/// Pull the kind and key out of each commitish a listing returned
///
/// # Arguments
///
/// * `rows` - The commitish rows the listing returned
fn commitish_keys(rows: &[Value]) -> Vec<Value> {
    rows.iter()
        .map(|row| {
            // a commitish is serialized as a map from its kind to its data
            let (kind, inner) = row
                .as_object()
                .and_then(|map| map.iter().next())
                .map_or(("", row), |(kind, inner)| (kind.as_str(), inner));
            // commits are keyed by hash and branches and tags by name
            let key = inner
                .get("hash")
                .or_else(|| inner.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            serde_json::json!({ "key": format!("{kind}:{key}") })
        })
        .collect()
}

/// A branch and a tag with the same name and timestamp page through with each returned once
#[tokio::test]
async fn commitish_ties_share_a_name() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // create a group and a repo in it
    let group = generators::groups(1, &client).await?.remove(0).name;
    let repo = generators::repos(&group, 1, &client).await?.remove(0).url;
    // upload a data blob for our commitishes
    let path = std::env::temp_dir().join(format!("thorium-tie-data-{}", Uuid::new_v4()));
    tokio::fs::write(&path, "commitishes sharing a name").await?;
    let tar = TarredRepo {
        name: "data".to_owned(),
        repo: path.clone(),
    };
    let upload = client.repos.upload(&repo, tar, vec![group.clone()]).await;
    tokio::fs::remove_file(&path).await?;
    let data = upload?.sha256;
    // save commit c9 and branch v1, then tag v1, all at one timestamp
    let timestamp = Utc::now() - Duration::minutes(5);
    let first = HashMap::from([
        (
            "c9".to_owned(),
            CommitishRequest::Commit(CommitRequest {
                author: "io".to_owned(),
                timestamp,
                topic: None,
                description: None,
                truncated: false,
            }),
        ),
        (
            "v1".to_owned(),
            CommitishRequest::Branch(BranchRequest {
                commit: "c9".to_owned(),
                timestamp,
            }),
        ),
    ]);
    let second = HashMap::from([(
        "v1".to_owned(),
        CommitishRequest::Tag(GitTagRequest {
            commit: "c9".to_owned(),
            author: "io".to_owned(),
            timestamp,
        }),
    )]);
    for commitishes in [first, second] {
        // listings stop at the earliest commit, so give one before our commitishes
        let map = CommitishMapRequest {
            groups: vec![group.clone()],
            earliest: Some(timestamp - Duration::hours(1)),
            end: true,
            commitishes,
        };
        client.repos.add_commits(&repo, &data, &map).await?;
    }
    // page with boundaries between the branch and the tag, with and without details
    let expected: HashSet<String> = ["Branch:v1", "Tag:v1", "Commit:c9"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    for listing in ["commitishes", "commitish-details"] {
        for limit in [1, 2] {
            let rows = page_all(&client, &format!("/api/repos/{listing}/{repo}"), limit).await?;
            assert_each_once(&commitish_keys(&rows), "key", &expected);
        }
    }
    Ok(())
}

/// Commits and branches in buckets the other kind has no rows in are all listed, with and
/// without their details
#[tokio::test]
async fn commitishes_list_every_kinds_buckets() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // create a group and a repo in it
    let group = generators::groups(1, &client).await?.remove(0).name;
    let repo = generators::repos(&group, 1, &client).await?.remove(0).url;
    // upload a data blob for our commitishes
    let path = std::env::temp_dir().join(format!("thorium-kind-data-{}", Uuid::new_v4()));
    tokio::fs::write(&path, "commitishes in separate buckets").await?;
    let tar = TarredRepo {
        name: "data".to_owned(),
        repo: path.clone(),
    };
    let upload = client.repos.upload(&repo, tar, vec![group.clone()]).await;
    tokio::fs::remove_file(&path).await?;
    let data = upload?.sha256;
    // a commit and a branch share the newest bucket, and each kind also has an older bucket
    // of its own
    let now = Utc::now();
    let commit = |minutes: i64| {
        CommitishRequest::Commit(CommitRequest {
            author: "io".to_owned(),
            timestamp: now - Duration::minutes(minutes),
            topic: None,
            description: None,
            truncated: false,
        })
    };
    let branch = |minutes: i64| {
        CommitishRequest::Branch(BranchRequest {
            commit: "c2".to_owned(),
            timestamp: now - Duration::minutes(minutes),
        })
    };
    // listings stop at the earliest commit, so give one before our commitishes
    let map = CommitishMapRequest {
        groups: vec![group],
        earliest: Some(now - Duration::hours(2)),
        end: true,
        commitishes: HashMap::from([
            ("c1".to_owned(), commit(30)),
            ("c2".to_owned(), commit(1)),
            ("main".to_owned(), branch(1)),
            ("old".to_owned(), branch(60)),
        ]),
    };
    client.repos.add_commits(&repo, &data, &map).await?;
    // every commitish is listed once, in pages of every size, with and without details
    let expected: HashSet<String> = ["Commit:c1", "Commit:c2", "Branch:main", "Branch:old"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    for listing in ["commitishes", "commitish-details"] {
        for limit in [1, 2, 10] {
            let rows = page_all(&client, &format!("/api/repos/{listing}/{repo}"), limit).await?;
            assert_each_once(&commitish_keys(&rows), "key", &expected);
        }
    }
    Ok(())
}

/// Copy the tag rows of an item into new items that share its timestamp, a set per group
///
/// Each copy keeps the original row's partition, so the tag census already covers it, and is
/// written to only one group. Returns the new items.
///
/// # Arguments
///
/// * `key` - The tag key the item was tagged with
/// * `item` - The item whose tag rows to copy
/// * `per_group` - The number of copies to make in each group
/// * `new_item` - Builds the name of each copy from its group's index and its index
async fn copy_tag_rows(
    key: &str,
    item: &str,
    per_group: usize,
    new_item: impl Fn(usize, usize) -> String,
) -> Result<Vec<String>, Error> {
    // connect to scylla
    let session = test_utilities::scylla_session().await?;
    let ns = &test_utilities::CONF.thorium.namespace;
    // read the item's tag rows for this key, one per group, as JSON
    let read = format!("SELECT JSON * FROM {ns}.tags WHERE key = ? AND item = ? ALLOW FILTERING");
    let rows = session
        .query_unpaged(read, (key, item))
        .await
        .map_err(|err| Error::new(err.to_string()))?
        .into_rows_result()
        .map_err(|err| Error::new(err.to_string()))?
        .rows::<(String,)>()
        .map_err(|err| Error::new(err.to_string()))?
        .map(|row| row.map(|(json,)| json))
        .collect::<Result<Vec<String>, _>>()
        .map_err(|err| Error::new(err.to_string()))?;
    assert!(
        rows.len() > 1,
        "{item} has {} tag rows for {key}",
        rows.len()
    );
    // insert the copies for each group
    let insert = format!("INSERT INTO {ns}.tags JSON ?");
    let mut items = Vec::with_capacity(rows.len() * per_group);
    for (group_index, json) in rows.iter().enumerate() {
        let mut row: Value = serde_json::from_str(json)?;
        for index in 0..per_group {
            let copy = new_item(group_index, index);
            row["item"] = Value::String(copy.clone());
            session
                .query_unpaged(insert.clone(), (row.to_string(),))
                .await
                .map_err(|err| Error::new(err.to_string()))?;
            items.push(copy);
        }
    }
    Ok(items)
}

/// Tagged file, repo, and entity listings page through rows sharing a timestamp across many
/// groups with each returned once
#[tokio::test]
async fn tag_ties_page_through() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // build six groups to tag items in
    let groups: Vec<String> = generators::groups(6, &client)
        .await?
        .into_iter()
        .map(|group| group.name)
        .collect();
    let group_refs: Vec<&str> = groups.iter().map(String::as_str).collect();
    let group_query = groups
        .iter()
        .map(|group| format!("groups[]={group}"))
        .collect::<Vec<String>>()
        .join("&");
    // tag one file, repo, and entity in every group with a key only this run uses
    let key = format!("harvest{}", &Uuid::new_v4().simple().to_string()[..8]);
    let sample =
        SampleRequest::new_buffer(Buffer::new(generators::gen_string(512)), group_refs.clone())
            .tag(&key, "corn");
    let sha256 = client.files.create(sample).await?.sha256;
    let url = format!("provider.tld/ties/{key}");
    let repo = RepoRequest::new(&url, group_refs.clone(), None).tag(&key, "corn");
    client.repos.create(&repo).await?;
    let entity = EntityRequest::new(
        format!("entity-{key}"),
        generators::gen_other_meta(),
        groups.clone(),
    )
    .tag(&key, "corn");
    let id = client.entities.create(entity).await?.id.to_string();
    // for each kind, copy its tag rows into ten new items per group at the same timestamp
    let listings = [
        (
            "/api/files/",
            "sha256",
            sha256.clone(),
            copy_tag_rows(&key, &sha256, 10, |group, index| {
                format!("{:0>64}", format!("{group}{index:02}"))
            })
            .await?,
        ),
        (
            "/api/repos/",
            "url",
            url.clone(),
            copy_tag_rows(&key, &url, 10, |group, index| {
                format!("provider.tld/ties/{key}-{group}-{index:02}")
            })
            .await?,
        ),
        (
            "/api/entities/",
            "id",
            id.clone(),
            copy_tag_rows(&key, &id, 10, |_, _| Uuid::new_v4().to_string()).await?,
        ),
    ];
    for (path, field, original, copies) in listings {
        // every tagged item comes back once whatever the page size
        let mut expected: HashSet<String> = copies.into_iter().collect();
        expected.insert(original);
        let listing = format!("{path}?{group_query}&tags[{key}][]=corn");
        for limit in [7, 50] {
            let rows = page_all(&client, &listing, limit).await?;
            assert_each_once(&rows, field, &expected);
        }
    }
    Ok(())
}

/// The grouped network policy cursor clears a tie once it has been consumed, so ties at two
/// successive page boundaries return every policy once
#[tokio::test]
async fn network_policy_ties_are_cleared() -> Result<(), Error> {
    // get admin client and three groups
    let client = test_utilities::admin_client().await?;
    let groups: Vec<String> = generators::groups(3, &client)
        .await?
        .into_iter()
        .map(|group| group.name)
        .collect();
    // write three different policies that share one name, one per group
    let session = test_utilities::scylla_session().await?;
    let ns = &test_utilities::CONF.thorium.namespace;
    let name = format!("tied-{}", Uuid::new_v4());
    let insert = format!("INSERT INTO {ns}.network_policies (group, name, id) VALUES (?, ?, ?)");
    let mut expected = HashSet::with_capacity(3);
    for group in &groups {
        let id = Uuid::new_v4();
        session
            .query_unpaged(insert.clone(), (group, &name, id))
            .await
            .map_err(|err| Error::new(err.to_string()))?;
        expected.insert(id.to_string());
    }
    // page one policy at a time so each page boundary falls inside the tie
    let path = format!(
        "/api/network-policies/?groups[]={}&groups[]={}&groups[]={}",
        groups[0], groups[1], groups[2]
    );
    let rows = page_all(&client, &path, 1).await?;
    assert_each_once(&rows, "id", &expected);
    Ok(())
}

/// Check that listed entities are in newest-first order
///
/// # Arguments
///
/// * `rows` - The listed entity rows
fn assert_newest_first(rows: &[Value]) {
    // get each row's created timestamp
    let created: Vec<chrono::DateTime<Utc>> = rows
        .iter()
        .map(|row| serde_json::from_value(row["created"].clone()).unwrap())
        .collect();
    // every row is no newer than the one before it
    assert!(
        created.windows(2).all(|pair| pair[0] >= pair[1]),
        "rows are not newest first: {created:?}"
    );
}

/// Entities of one kind created in one group within one bucket page newest first with each
/// entity returned once
#[tokio::test]
async fn entities_page_newest_first() -> Result<(), Error> {
    // get admin client and a group to list in
    let client = test_utilities::admin_client().await?;
    let group = generators::groups(1, &client).await?.remove(0).name;
    // create 12 entities one after another
    let mut expected = HashSet::with_capacity(12);
    for _ in 0..12 {
        let req = generators::gen_entity(&group, generators::gen_other_meta());
        expected.insert(client.entities.create(req).await?.id.to_string());
    }
    // page the group's entities 5 at a time
    let rows = page_all(&client, &format!("/api/entities/?groups[]={group}"), 5).await?;
    assert_each_once(&rows, "id", &expected);
    assert_newest_first(&rows);
    Ok(())
}

/// The entities listing statements sort by `created DESC, id DESC` across partitions when both
/// partition-key columns use `IN` and they run unpaged
#[tokio::test]
async fn entities_order_across_kinds() -> Result<(), Error> {
    // get admin client and a group to list in
    let client = test_utilities::admin_client().await?;
    let group = generators::groups(1, &client).await?.remove(0).name;
    // create entities of two kinds, alternating so their timestamps interleave
    let mut expected = HashSet::with_capacity(12);
    for _ in 0..6 {
        let req = generators::gen_entity(&group, generators::gen_other_meta());
        expected.insert(client.entities.create(req).await?.id.to_string());
        expected.insert(
            generators::vendor_entity(&group, &client)
                .await?
                .id
                .to_string(),
        );
    }
    // both kinds page newest first across their partitions
    let path = format!("/api/entities/?groups[]={group}&kinds[]=Other&kinds[]=Vendor");
    for limit in [3, 8] {
        let rows = page_all(&client, &path, limit).await?;
        assert_each_once(&rows, "id", &expected);
        assert_newest_first(&rows);
    }
    // Scylla accepts the ordered statement shape with IN on both partition-key columns unpaged
    let session = test_utilities::scylla_session().await?;
    let ns = &test_utilities::CONF.thorium.namespace;
    let read = format!(
        "SELECT kind, created, id FROM {ns}.entities \
            WHERE kind in ? AND group = ? AND year = ? AND bucket in ? \
            ORDER BY created DESC, id DESC PER PARTITION LIMIT ?"
    );
    let prepared = session
        .prepare(read)
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    // read the buckets of the last few minutes, where our entities live, with kinds stored as
    // their JSON strings
    let now = Utc::now();
    let year = chrono::Datelike::year(&now);
    let size = test_utilities::CONF.thorium.entities.partition_size;
    let current = thorium::utils::helpers::partition(now, year, size);
    let buckets: Vec<i32> = ((current - 3).max(0)..=current).collect();
    let result = session
        .execute_unpaged(
            &prepared,
            (
                vec![r#""Other""#, r#""Vendor""#],
                &group,
                year,
                buckets,
                10_i32,
            ),
        )
        .await
        .map_err(|err| Error::new(err.to_string()))?
        .into_rows_result()
        .map_err(|err| Error::new(err.to_string()))?;
    // rows from both kinds' partitions come back newest first
    let created: Vec<chrono::DateTime<Utc>> = result
        .rows::<(String, chrono::DateTime<Utc>, Uuid)>()
        .map_err(|err| Error::new(err.to_string()))?
        .map(|row| row.map(|(_, created, _)| created))
        .collect::<Result<_, _>>()
        .map_err(|err| Error::new(err.to_string()))?;
    assert_eq!(
        created.len(),
        12,
        "the ordered statement returned {created:?}"
    );
    assert!(
        created.windows(2).all(|pair| pair[0] >= pair[1]),
        "the ordered statement isn't newest first: {created:?}"
    );
    Ok(())
}

/// The listing families checked for page shapes and unchanged ordering
#[derive(Debug, Clone, Copy)]
enum Family {
    /// Untagged entity listings
    Entities,
    /// File listings
    Files,
    /// Repo listings
    Repos,
    /// A file's association listings
    Associations,
    /// A repo's commitish listings
    Commitishes,
    /// Network policy listings, a grouped cursor
    NetworkPolicies,
    /// Node listings, a simple cursor
    Nodes,
}

/// Every listing family
const FAMILIES: [Family; 7] = [
    Family::Entities,
    Family::Files,
    Family::Repos,
    Family::Associations,
    Family::Commitishes,
    Family::NetworkPolicies,
    Family::Nodes,
];

/// A listing built for a test along with the rows it should return
struct Listing {
    /// The listing's path and query, without its page size or cursor
    path: String,
    /// The name of the listing's page size parameter
    param: &'static str,
    /// The JSON pointer to each row's identifying value
    pointer: &'static str,
    /// The identifying values of the rows in the order the listing returns them
    expected: Vec<String>,
}

/// Get the identifying value of each row as a string, in order
///
/// # Arguments
///
/// * `rows` - The rows to get values from
/// * `pointer` - The JSON pointer to each row's identifying value
fn keys_at(rows: &[Value], pointer: &str) -> Vec<String> {
    // read each row's value, keeping strings as they are and printing anything else
    rows.iter()
        .map(|row| match row.pointer(pointer) {
            Some(Value::String(value)) => value.clone(),
            other => format!("{other:?}"),
        })
        .collect()
}

/// Create rows with distinct timestamps for a listing family
///
/// # Arguments
///
/// * `client` - The client to create rows with
/// * `family` - The family to create rows for
/// * `count` - The number of rows to create
#[allow(clippy::too_many_lines)]
async fn build_listing(client: &Thorium, family: Family, count: usize) -> Result<Listing, Error> {
    // every listing gets a group of its own
    let group = generators::groups(1, client).await?.remove(0).name;
    let mut created = Vec::with_capacity(count);
    let listing = match family {
        Family::Entities => {
            // create entities one after another
            for _ in 0..count {
                let req = generators::gen_entity(&group, generators::gen_other_meta());
                created.push(client.entities.create(req).await?.id.to_string());
            }
            // they list newest first
            created.reverse();
            Listing {
                path: format!("/api/entities/?groups[]={group}"),
                param: "limit",
                pointer: "/id",
                expected: created,
            }
        }
        Family::Files => {
            // upload files one after another
            for _ in 0..count {
                let sample = generators::gen_sample(&group);
                created.push(client.files.create(sample).await?.sha256);
            }
            // they list newest first
            created.reverse();
            Listing {
                path: format!("/api/files/?groups[]={group}"),
                param: "limit",
                pointer: "/sha256",
                expected: created,
            }
        }
        Family::Repos => {
            // create repos one after another
            for _ in 0..count {
                created.push(generators::repos(&group, 1, client).await?.remove(0).url);
            }
            // they list newest first
            created.reverse();
            Listing {
                path: format!("/api/repos/?groups[]={group}"),
                param: "limit",
                pointer: "/url",
                expected: created,
            }
        }
        Family::Associations => {
            // associate a file with one repo per request so each row has its own timestamp
            let sha256 = client
                .files
                .create(generators::gen_sample(&group))
                .await?
                .sha256;
            for index in 0..count {
                let target = format!("provider.tld/shapes/repo{index:02}");
                let source = AssociationTarget::File(sha256.clone());
                let req = AssociationRequest::new(AssociationKind::AssociatedWith, source)
                    .groups(vec![group.clone()])
                    .target(AssociationTarget::Repo(target.clone()));
                client.associations.create(&req).await?;
                created.push(target);
            }
            // they list newest first
            created.reverse();
            Listing {
                path: format!("/api/files/associations/{sha256}"),
                param: "limit",
                pointer: "/other/Repo",
                expected: created,
            }
        }
        Family::Commitishes => {
            // create a repo with a data blob
            let repo = generators::repos(&group, 1, client).await?.remove(0).url;
            let path = std::env::temp_dir().join(format!("thorium-shape-data-{}", Uuid::new_v4()));
            tokio::fs::write(&path, "shaped commitishes").await?;
            let tar = TarredRepo {
                name: "data".to_owned(),
                repo: path.clone(),
            };
            let upload = client.repos.upload(&repo, tar, vec![group.clone()]).await;
            tokio::fs::remove_file(&path).await?;
            let data = upload?.sha256;
            // add commits a minute apart, newest first
            let now = Utc::now();
            let mut commitishes = HashMap::with_capacity(count);
            for index in 0..count {
                let commit = CommitishRequest::Commit(CommitRequest {
                    author: "io".to_owned(),
                    timestamp: now - Duration::minutes(i64::try_from(index).unwrap() + 1),
                    topic: None,
                    description: None,
                    truncated: false,
                });
                commitishes.insert(format!("commit{index:02}"), commit);
                created.push(format!("commit{index:02}"));
            }
            let map = CommitishMapRequest {
                groups: vec![group.clone()],
                earliest: Some(now - Duration::days(1)),
                end: true,
                commitishes,
            };
            client.repos.add_commits(&repo, &data, &map).await?;
            Listing {
                path: format!("/api/repos/commitishes/{repo}"),
                param: "limit",
                pointer: "/Commit/hash",
                expected: created,
            }
        }
        Family::NetworkPolicies => {
            // create network policies in our group
            let reqs =
                generators::network_policies(std::slice::from_ref(&group), count, client).await?;
            created.extend(reqs.into_iter().map(|req| req.name));
            // they list by name
            created.sort();
            Listing {
                path: format!("/api/network-policies/?groups[]={group}"),
                param: "limit",
                pointer: "/name",
                expected: created,
            }
        }
        Family::Nodes => {
            // register nodes in a cluster of their own, named after our group
            for index in 0..count {
                let node = format!("node{index:02}");
                generators::node(&group, &node, Resources::default(), client).await?;
                created.push(node);
            }
            Listing {
                path: format!("/api/system/nodes/?clusters[]={group}"),
                param: "page_size",
                pointer: "/node",
                expected: created,
            }
        }
    };
    Ok(listing)
}

/// For every listing family, fewer rows than a page, exactly one page, and many pages each
/// return every row once and end
#[tokio::test]
async fn listings_page_shapes() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    for family in FAMILIES {
        for count in [3, 5, 12] {
            // build a listing of this size and page it 5 rows at a time
            let listing = build_listing(&client, family, count).await?;
            let rows = page_all_by(&client, &listing.path, listing.param, 5).await?;
            // every row comes back once
            let keys: Vec<Value> = keys_at(&rows, listing.pointer)
                .into_iter()
                .map(|key| serde_json::json!({ "key": key }))
                .collect();
            let expected: HashSet<String> = listing.expected.iter().cloned().collect();
            assert_each_once(&keys, "key", &expected);
        }
    }
    Ok(())
}

/// For every listing family, rows with no ties at page boundaries come back newest first for
/// time listings and by name for the others
#[tokio::test]
async fn untied_listings_keep_their_order() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    for family in FAMILIES {
        // entities stay within one partition's first read so the order is comparable
        let count = match family {
            Family::Entities => 3,
            _ => 7,
        };
        // page the listing 2 rows at a time
        let listing = build_listing(&client, family, count).await?;
        let rows = page_all_by(&client, &listing.path, listing.param, 2).await?;
        // the rows come back in the expected order
        assert_eq!(
            keys_at(&rows, listing.pointer),
            listing.expected,
            "{family:?} rows came back in a different order"
        );
    }
    Ok(())
}

/// A cursor id saved in the format listings wrote before their saved state changed shape is
/// reported as not found, so the client restarts its listing, while a cursor saved now resumes
#[tokio::test]
async fn cursors_saved_in_the_old_format_are_not_found() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // associate a file with 6 repos in one request so every row shares one timestamp
    let group = generators::groups(1, &client).await?.remove(0).name;
    let sha256 = client
        .files
        .create(generators::gen_sample(&group))
        .await?
        .sha256;
    let targets: Vec<String> = (0..6)
        .map(|i| format!("provider.tld/saved/repo{i}"))
        .collect();
    let mut req = AssociationRequest::new(
        AssociationKind::AssociatedWith,
        AssociationTarget::File(sha256.clone()),
    )
    .groups(vec![group.clone()]);
    for target in &targets {
        req = req.target(AssociationTarget::Repo(target.clone()));
    }
    client.associations.create(&req).await?;
    let path = format!("/api/files/associations/{sha256}");
    let all = page_all(&client, &path, 10).await?;
    let created = all[0]["created"].clone();
    // a time cursor saved in the old format, with its ties keyed by group
    let ns = &test_utilities::CONF.thorium.namespace;
    let tied = serde_json::to_string(&serde_json::json!({ "Repo": targets[2] }))?;
    let retain = serde_json::json!({
        "start": created,
        "end": "2021-01-01T00:00:00Z",
        "extra_filter": serde_json::to_string(&serde_json::json!({ "File": sha256 }))?,
        "group_by": [group],
        "ties": { group.clone(): tied },
        "tags_retain": null,
    });
    let id = Uuid::new_v4();
    redis_set(&format!("{ns}:cursors:Scylla:{id}"), &retain.to_string()).await?;
    let status = status_of(&client, &format!("{path}?limit=2&cursor={id}")).await?;
    assert_eq!(status, 404, "an old time cursor returned {status}");
    // the same cursor saved under its owner without a layout version
    let username = client.users.info().await?.username;
    let id = Uuid::new_v4();
    redis_set(
        &format!("{ns}:cursors:Scylla:{username}:{id}"),
        &retain.to_string(),
    )
    .await?;
    let status = status_of(&client, &format!("{path}?limit=2&cursor={id}")).await?;
    assert_eq!(status, 404, "an unversioned time cursor returned {status}");
    // a grouped cursor saved in the old format
    let id = Uuid::new_v4();
    let retain = serde_json::json!({
        "extra_filter": null,
        "group_by": [group],
        "ties": [],
        "current_sort_by": "a",
    });
    redis_set(
        &format!("{ns}:cursors:GroupedScylla:{id}"),
        &retain.to_string(),
    )
    .await?;
    let status = status_of(
        &client,
        &format!("/api/network-policies/?groups[]={group}&limit=2&cursor={id}"),
    )
    .await?;
    assert_eq!(status, 404, "an old grouped cursor returned {status}");
    // a simple cursor saved in the old format
    let id = Uuid::new_v4();
    let retain = serde_json::json!({ "partitions": ["saved-cluster"], "index": 0, "tie": "n0" });
    redis_set(
        &format!("{ns}:cursors:SimpleScylla:{id}"),
        &retain.to_string(),
    )
    .await?;
    let status = status_of(
        &client,
        &format!("/api/system/nodes/?clusters[]=saved-cluster&page_size=2&cursor={id}"),
    )
    .await?;
    assert_eq!(status, 404, "an old simple cursor returned {status}");
    // a cursor saved now still resumes and finishes with every target once
    let rows = page_all(&client, &path, 2).await?;
    let keys: Vec<Value> = keys_at(&rows, "/other/Repo")
        .into_iter()
        .map(|key| serde_json::json!({ "key": key }))
        .collect();
    assert_each_once(&keys, "key", &targets.iter().cloned().collect());
    Ok(())
}

/// A census bucket stored as a negative number fails the listing with a 500 instead of being
/// read
#[tokio::test]
async fn negative_census_buckets_fail() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // create a group to list
    let group = generators::groups(1, &client).await?.remove(0).name;
    // add a negative member to its 2012 census stream, which only a listing over 2012 reads, so
    // the admin listings other tests run over every group at the same time never see it
    let ns = &test_utilities::CONF.thorium.namespace;
    let size = test_utilities::CONF.thorium.files.partition_size;
    let at = Utc.with_ymd_and_hms(2012, 6, 1, 0, 0, 0).unwrap();
    let year = chrono::Datelike::year(&at);
    let bucket = thorium::utils::helpers::partition(at, year, size);
    let redis = test_utilities::redis_pool().await;
    let mut conn = redis
        .get()
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    let (): () = bb8_redis::redis::cmd("ZADD")
        .arg(format!("{ns}:census:samples:stream:{group}:{year}"))
        .arg(bucket)
        .arg("-5")
        .query_async(&mut *conn)
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    drop(conn);
    // a listing over 2012 refuses the stored bucket
    let path = format!(
        "/api/files/?groups[]={group}&start=2012-12-31T00:00:00Z&end=2012-01-01T00:00:00Z&limit=5"
    );
    let status = status_of(&client, &path).await?;
    assert_eq!(status, 500, "a negative census bucket returned {status}");
    Ok(())
}

/// Two groups whose different items share the page-boundary timestamp, and two kinds of one
/// group, each return every item exactly once across pages
#[tokio::test]
async fn ties_two_groups_distinct_items() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    let session = test_utilities::scylla_session().await?;
    let ns = &test_utilities::CONF.thorium.namespace;
    let groups: Vec<String> = generators::groups(2, &client)
        .await?
        .into_iter()
        .map(|group| group.name)
        .collect();
    // every row shares one timestamp in one bucket
    let at = Utc::now() - Duration::minutes(5);
    let year = chrono::Datelike::year(&at);
    let redis = test_utilities::redis_pool().await;
    let mut conn = redis
        .get()
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    // four different submissions in each group
    let size = test_utilities::CONF.thorium.files.partition_size;
    let bucket = thorium::utils::helpers::partition(at, year, size);
    let insert = format!(
        "INSERT INTO {ns}.samples_list (group, year, bucket, uploaded, id, sha256) VALUES (?, ?, ?, ?, ?, ?)"
    );
    let mut samples = HashSet::new();
    for group in &groups {
        for _ in 0..4 {
            let sha256 = format!("{:0>64}", Uuid::new_v4().simple());
            session
                .query_unpaged(
                    insert.clone(),
                    (group, year, bucket, at, Uuid::new_v4(), &sha256),
                )
                .await
                .map_err(|err| Error::new(err.to_string()))?;
            samples.insert(sha256);
        }
        let (): () = bb8_redis::redis::cmd("ZADD")
            .arg(format!("{ns}:census:samples:stream:{group}:{year}"))
            .arg(bucket)
            .arg(bucket)
            .query_async(&mut *conn)
            .await
            .map_err(|err| Error::new(err.to_string()))?;
    }
    drop(conn);
    let group_query = groups
        .iter()
        .map(|group| format!("groups[]={group}"))
        .collect::<Vec<String>>()
        .join("&");
    for limit in [1, 3, 5] {
        let rows = page_all(&client, &format!("/api/files/?{group_query}"), limit).await?;
        assert_each_once(&rows, "sha256", &samples);
    }
    // commits and branches of one repo in one group share the timestamp too
    let repo = repo_with_tied_commitishes(&client).await?;
    let expected: HashSet<String> = ["Branch:main", "Commit:c9", "Commit:c1", "Branch:a1"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    for limit in [1, 2, 3] {
        let rows = page_all(&client, &format!("/api/repos/commitishes/{repo}"), limit).await?;
        assert_each_once(&commitish_keys(&rows), "key", &expected);
    }
    Ok(())
}

/// A group with more non-empty buckets than one census read returns, listed together with a
/// group whose older bucket alone fills a page, returns every row of both groups exactly once
#[tokio::test]
async fn busy_groups_list_every_bucket() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // get a Scylla session, the namespace, and the files partition size to write rows directly
    let session = test_utilities::scylla_session().await?;
    let ns = &test_utilities::CONF.thorium.namespace;
    let size = test_utilities::CONF.thorium.files.partition_size;
    // create the two groups to list
    let groups: Vec<String> = generators::groups(2, &client)
        .await?
        .into_iter()
        .map(|group| group.name)
        .collect();
    // the first group has 150 rows one bucket apart, all newer than the second group's rows
    let busy = Utc.with_ymd_and_hms(2024, 6, 1, 0, 0, 0).unwrap();
    let mut rows: Vec<(&String, chrono::DateTime<Utc>)> = (0..150)
        .map(|index| {
            (
                &groups[0],
                busy + Duration::seconds(i64::from(size) * index),
            )
        })
        .collect();
    // the second group has 200 rows in one older bucket, 500 ms apart
    let full = Utc.with_ymd_and_hms(2024, 3, 1, 0, 0, 0).unwrap();
    rows.extend((0..200).map(|index| (&groups[1], full + Duration::milliseconds(500 * index))));
    // build the statement that writes a row to the files list
    let insert = format!(
        "INSERT INTO {ns}.samples_list (group, year, bucket, uploaded, id, sha256) VALUES (?, ?, ?, ?, ?, ?)"
    );
    // get a Redis connection to write the census entries with
    let redis = test_utilities::redis_pool().await;
    let mut conn = redis
        .get()
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    // track the busy group's samples and every sample written
    let mut busy_samples = HashSet::new();
    let mut samples = HashSet::new();
    // write each row and its census entry
    for (group, at) in rows {
        // find the year and bucket this row is in, and give it a unique sha256
        let year = chrono::Datelike::year(&at);
        let bucket = thorium::utils::helpers::partition(at, year, size);
        let sha256 = format!("{:0>64}", Uuid::new_v4().simple());
        // write the row to its group's bucket
        session
            .query_unpaged(
                insert.clone(),
                (group, year, bucket, at, Uuid::new_v4(), &sha256),
            )
            .await
            .map_err(|err| Error::new(err.to_string()))?;
        // mark the bucket non-empty in its group's census stream for that year
        let (): () = bb8_redis::redis::cmd("ZADD")
            .arg(format!("{ns}:census:samples:stream:{group}:{year}"))
            .arg(bucket)
            .arg(bucket)
            .query_async(&mut *conn)
            .await
            .map_err(|err| Error::new(err.to_string()))?;
        // track the sample, and whether it is in the busy group
        if group == &groups[0] {
            busy_samples.insert(sha256.clone());
        }
        samples.insert(sha256);
    }
    // release our Redis connection before listing
    drop(conn);
    // list both groups in pages that the older bucket alone can fill
    let path = format!("/api/files/?groups[]={}&groups[]={}", groups[0], groups[1]);
    let listed = page_all(&client, &path, 200).await?;
    // every row of the busy group comes back once, and so does every row of the other group
    let busy_listed: Vec<Value> = listed
        .iter()
        .filter(|row| {
            row["sha256"]
                .as_str()
                .is_some_and(|sha| busy_samples.contains(sha))
        })
        .cloned()
        .collect();
    assert_each_once(&busy_listed, "sha256", &busy_samples);
    assert_each_once(&listed, "sha256", &samples);
    Ok(())
}

/// Read the first page of a listing and return the cursor id it saved
///
/// # Arguments
///
/// * `client` - The client to list with
/// * `path` - The listing's path and query, with its page size
async fn first_cursor(client: &Thorium, path: &str) -> Result<String, Error> {
    // read the first page
    let page = get_json(client, path).await?;
    // a listing with more rows than one page saves a cursor
    let id = page["cursor"].as_str();
    assert!(id.is_some(), "{path} saved no cursor: {page}");
    Ok(id.unwrap_or_default().to_owned())
}

/// Send a GET to the API with this client's credentials and return its status and its body with
/// a cursor id replaced by `<id>`, so the answers for two different ids can be compared
///
/// # Arguments
///
/// * `client` - The client whose credentials to send
/// * `path` - The path and query to request, starting with `/api/`
/// * `id` - The cursor id to hide in the body
async fn answer_of(client: &Thorium, path: &str, id: &str) -> Result<(u16, String), Error> {
    // build the auth header the client would send
    let token = client.users.info().await?.token;
    let encoded = base64::engine::general_purpose::STANDARD.encode(token.as_bytes());
    // send our raw request so the cursor reaches the API unchanged
    let resp = reqwest::Client::new()
        .get(format!("{}{path}", client.host))
        .header("authorization", format!("token {encoded}"))
        .send()
        .await?;
    // keep the status, and hide the id in the body
    let status = resp.status().as_u16();
    let body = resp.text().await?.replace(id, "<id>");
    Ok((status, body))
}

/// Save a cursor on each listing as its owner and list each one another user resumed without
/// the 404 status and body an unknown id gets, checking the owner resumes it by one row
///
/// # Arguments
///
/// * `owner` - The client saving the cursors
/// * `other` - The client resuming them
/// * `listings` - Each listing's path for the owner, the path the other user resumes it with,
///   and the name of its page size parameter
async fn resumed_by_others(
    owner: &Thorium,
    other: &Thorium,
    listings: &[(String, String, &str)],
) -> Result<Vec<String>, Error> {
    // collect every listing whose cursor another user resumed
    let mut resumed = Vec::new();
    for (path, other_path, param) in listings {
        // the owner reads a first page and gets a cursor
        let id = first_cursor(owner, &format!("{path}&{param}=1")).await?;
        // the other user resumes it
        let theirs = format!("{other_path}&{param}=1&cursor={id}");
        let resume = answer_of(other, &theirs, &id).await?;
        // ask the same listing for an unknown id, which is a 404
        let unknown = Uuid::new_v4().to_string();
        let unknown_path = format!("{other_path}&{param}=1&cursor={unknown}");
        let missing = answer_of(other, &unknown_path, &unknown).await?;
        assert_eq!(
            missing.0, 404,
            "an unknown cursor on {path} returned {missing:?}"
        );
        // the resume must get the same status and body as the unknown id, and a cursor another
        // user paged no longer resumes by one row for its owner, so check the next listing
        if resume != missing {
            resumed.push(format!("{path}: {resume:?}"));
            continue;
        }
        // the owner still resumes it, reading one more row (a count reports its running total)
        let page = get_json(owner, &format!("{path}&{param}=1&cursor={id}")).await?;
        let resumed_one = page["data"]
            .as_array()
            .map_or(page["total"].as_u64() == Some(2), |rows| rows.len() == 1);
        assert!(
            resumed_one,
            "the owner didn't resume {path} by one row: {page}"
        );
    }
    Ok(resumed)
}

/// Build a user who is a member of one of the owner's groups
///
/// # Arguments
///
/// * `owner` - The admin client that owns the group
/// * `group` - The group to add the user to
async fn co_member(owner: &Thorium, group: &str) -> Result<Thorium, Error> {
    // build a new user and add them to the group
    let member = generators::client(owner).await?;
    let name = member.users.info().await?.username;
    let add = GroupUsersUpdate::default().direct_add(&name);
    owner
        .groups
        .update(group, &GroupUpdate::default().users(add))
        .await?;
    Ok(member)
}

/// Resume each listing over the same path its owner lists it with
///
/// # Arguments
///
/// * `listings` - Each listing's owner path, resume path, and page size name
fn same_paths<'a>(listings: &[(String, String, &'a str)]) -> Vec<(String, String, &'a str)> {
    listings
        .iter()
        .map(|(path, _, param)| (path.clone(), path.clone(), *param))
        .collect()
}

/// A cursor id saved by one user is not found for another user, with the same status and body
/// as an unknown id, while its owner still resumes it, for time, grouped, simple, and tag count
/// cursors; this holds for a user outside the owner's groups and for a member of them
#[tokio::test]
async fn cursors_resume_only_for_their_owner() -> Result<(), Error> {
    // the owner is the admin, and the other user is in none of the owner's groups
    let owner = test_utilities::admin_client().await?;
    let other = generators::client(&owner).await?;
    let group = generators::groups(1, &owner).await?.remove(0).name;
    let other_group = generators::groups(1, &other).await?.remove(0).name;
    // give the owner's group more files, entities, and network policies than one page holds
    for _ in 0..3 {
        owner.files.create(generators::gen_sample(&group)).await?;
        generators::vendor_entity(&group, &owner).await?;
    }
    generators::network_policies(std::slice::from_ref(&group), 3, &owner).await?;
    // and give a cluster more nodes than one page holds
    let cluster = format!("owned{}", &Uuid::new_v4().simple().to_string()[..8]);
    for index in 0..3 {
        let node = format!("node{index}");
        generators::node(&cluster, &node, Resources::default(), &owner).await?;
    }
    // each listing's base path, the path the other user resumes with, and its page size name
    let listings = [
        (
            format!("/api/files/?groups[]={group}"),
            format!("/api/files/?groups[]={other_group}"),
            "limit",
        ),
        (
            format!("/api/entities/?groups[]={group}"),
            format!("/api/entities/?groups[]={other_group}"),
            "limit",
        ),
        (
            format!("/api/network-policies/?groups[]={group}"),
            format!("/api/network-policies/?groups[]={other_group}"),
            "limit",
        ),
        (
            format!("/api/system/nodes/?clusters[]={cluster}"),
            format!("/api/system/nodes/?clusters[]={cluster}"),
            "page_size",
        ),
        (
            format!("/api/files/count/?groups[]={group}"),
            format!("/api/files/count/?groups[]={other_group}"),
            "limit",
        ),
    ];
    // collect the cursors a user outside the owner's groups resumed
    let mut resumed = resumed_by_others(&owner, &other, &listings).await?;
    // and those a member of the owner's group, who lists the same rows, resumed
    let member = co_member(&owner, &group).await?;
    let by_member = resumed_by_others(&owner, &member, &same_paths(&listings)).await?;
    resumed.extend(
        by_member
            .into_iter()
            .map(|path| format!("group member: {path}")),
    );
    // no other user resumes any of these cursors, while their owner does
    assert!(
        resumed.is_empty(),
        "another user resumed these cursors: {resumed:#?}"
    );
    Ok(())
}

/// A cursor id saved by one user is not found for another user, with the same status and body
/// as an unknown id, while its owner still resumes it, for tag filtered, association, and repo
/// time cursors and for reaction cursors kept in Redis; this holds for a user outside the
/// owner's groups and for a member of them
#[tokio::test]
async fn tag_association_repo_and_reaction_cursors_resume_only_for_their_owner() -> Result<(), Error>
{
    // the owner is the admin, and the other user is in none of the owner's groups
    let owner = test_utilities::admin_client().await?;
    let other = generators::client(&owner).await?;
    let group = generators::groups(1, &owner).await?.remove(0).name;
    let other_group = generators::groups(1, &other).await?.remove(0).name;
    // give the owner's group more tagged files, repos, and reactions than one page holds
    let key = format!("owned{}", &Uuid::new_v4().simple().to_string()[..8]);
    generators::samples_with_tag(&group, 3, &key, "value", &owner).await?;
    generators::repos(&group, 3, &owner).await?;
    let (reactions, _) = generators::reactions(&group, 3, None, &owner).await?;
    let pipeline = &reactions[0].pipeline;
    // associate a file of the owner's with more repos than one page holds
    let sha256 = owner
        .files
        .create(generators::gen_sample(&group))
        .await?
        .sha256;
    let mut req = AssociationRequest::new(
        AssociationKind::AssociatedWith,
        AssociationTarget::File(sha256.clone()),
    )
    .groups(vec![group.clone()]);
    for index in 0..3 {
        let target = format!("provider.tld/owned/{key}{index}");
        req = req.target(AssociationTarget::Repo(target));
    }
    owner.associations.create(&req).await?;
    // the other user resumes the association cursor on a file of their own
    let other_sha256 = other
        .files
        .create(generators::gen_sample(&other_group))
        .await?
        .sha256;
    // each listing's base path, the path the other user resumes it with, and its page size name
    let listings = [
        (
            format!("/api/files/?groups[]={group}&tags[{key}][]=value"),
            format!("/api/files/?groups[]={other_group}&tags[{key}][]=value"),
            "limit",
        ),
        (
            format!("/api/files/associations/{sha256}?groups[]={group}"),
            format!("/api/files/associations/{other_sha256}?groups[]={other_group}"),
            "limit",
        ),
        (
            format!("/api/repos/?groups[]={group}"),
            format!("/api/repos/?groups[]={other_group}"),
            "limit",
        ),
        (
            format!("/api/reactions/list/{pipeline}/?groups[]={group}"),
            format!("/api/reactions/list/{pipeline}/?groups[]={other_group}"),
            "limit",
        ),
    ];
    // collect the cursors a user outside the owner's groups resumed
    let mut resumed = resumed_by_others(&owner, &other, &listings).await?;
    // and those a member of the owner's group, who lists the same rows, resumed
    let member = co_member(&owner, &group).await?;
    let by_member = resumed_by_others(&owner, &member, &same_paths(&listings)).await?;
    resumed.extend(
        by_member
            .into_iter()
            .map(|path| format!("group member: {path}")),
    );
    // no other user resumes any of these cursors, while their owner does
    assert!(
        resumed.is_empty(),
        "another user resumed these cursors: {resumed:#?}"
    );
    Ok(())
}

/// A search cursor saved for one user is not found for another user, outside or inside the
/// owner's group, with the same status and body as an unknown id, while its owner's request
/// finds it
///
/// The cursor is written to Redis directly, so this runs without Elasticsearch: the owner's
/// resume gets past the lookup and fails later, when it searches.
#[tokio::test]
async fn search_cursors_resume_only_for_their_owner() -> Result<(), Error> {
    // the owner is the admin, and the other user is in none of the owner's groups
    let owner = test_utilities::admin_client().await?;
    let other = generators::client(&owner).await?;
    let group = generators::groups(1, &owner).await?.remove(0).name;
    let other_group = generators::groups(1, &other).await?.remove(0).name;
    // save a search cursor over the owner's group under the owner
    let ns = &test_utilities::CONF.thorium.namespace;
    let username = owner.users.info().await?.username;
    let id = Uuid::new_v4().to_string();
    let retain = serde_json::json!({
        "start": "2026-01-01T00:00:00Z",
        "end": "2020-01-01T00:00:00Z",
        "groups": [group],
        "query": "*",
        "pit": "unused",
        "search_after": [1],
    });
    let key = format!("{ns}:cursors:Elastic:v2:{username}:{id}");
    redis_set(&key, &retain.to_string()).await?;
    // the other user resuming it gets the same answer as for an unknown id
    let theirs = format!("/api/search/?groups[]={other_group}&limit=1&cursor={id}");
    let resume = answer_of(&other, &theirs, &id).await?;
    let unknown = Uuid::new_v4().to_string();
    let unknown_path = format!("/api/search/?groups[]={other_group}&limit=1&cursor={unknown}");
    let missing = answer_of(&other, &unknown_path, &unknown).await?;
    assert_eq!(
        missing.0, 404,
        "an unknown search cursor returned {missing:?}"
    );
    assert_eq!(
        resume, missing,
        "another user's search cursor is told from an unknown id"
    );
    // a member of the owner's group resuming it over that group also gets that answer
    let member = co_member(&owner, &group).await?;
    let shared_path = format!("/api/search/?groups[]={group}&limit=1&cursor={id}");
    let resume = answer_of(&member, &shared_path, &id).await?;
    let unknown_path = format!("/api/search/?groups[]={group}&limit=1&cursor={unknown}");
    let missing = answer_of(&member, &unknown_path, &unknown).await?;
    assert_eq!(
        resume, missing,
        "a group member's resume of another user's search cursor is told from an unknown id"
    );
    // the owner's resume finds the cursor, so it isn't a 404
    let mine = format!("/api/search/?groups[]={group}&limit=1&cursor={id}");
    let found = answer_of(&owner, &mine, &id).await?;
    assert_ne!(
        found.0, 404,
        "the owner's search cursor wasn't found: {found:?}"
    );
    Ok(())
}

/// The clients and groups of a requester who resumes cursors saved over groups it can't list
struct Narrowed {
    /// The admin, who saves cursors over the in-scope and out-of-scope groups
    admin: Thorium,
    /// The admin's scoped token, limited to the in-scope group
    scoped: Thorium,
    /// A user who saves cursors over the shared group and is then removed from it
    member: Thorium,
    /// The member's username
    member_name: String,
    /// The group the scoped token is limited to
    in_scope: String,
    /// An admin group outside the scoped token's scope
    out_scope: String,
    /// The group the member saves cursors over and is removed from
    shared: String,
}

/// Build an admin with a scoped token limited to one of two groups, and a member of a shared
/// group who also has a group of their own
async fn narrowed() -> Result<Narrowed, Error> {
    // get the admin and its in-scope, out-of-scope, and shared groups
    let admin = test_utilities::admin_client().await?;
    let mut groups = generators::groups(3, &admin).await?;
    let shared = groups.remove(2).name;
    let out_scope = groups.remove(1).name;
    let in_scope = groups.remove(0).name;
    // give the admin a scoped token limited to the in-scope group
    let req =
        ScopedTokenRequest::new(format!("narrowed{}", Uuid::new_v4().simple())).group(&in_scope);
    let token = admin.users.create_scoped_token(&req).await?.token;
    let scoped = Thorium::build(admin.host.clone())
        .token(&token)
        .build()
        .await?;
    // build a member with a group of their own, and add them to the shared group
    let member = generators::client(&admin).await?;
    generators::groups(1, &member).await?;
    let member_name = member.users.info().await?.username;
    let add = GroupUsersUpdate::default().direct_add(&member_name);
    admin
        .groups
        .update(&shared, &GroupUpdate::default().users(add))
        .await?;
    Ok(Narrowed {
        admin,
        scoped,
        member,
        member_name,
        in_scope,
        out_scope,
        shared,
    })
}

/// Resume each saved cursor and list every resume not answered exactly as an unknown id is
///
/// # Arguments
///
/// * `requester` - The client resuming the cursors
/// * `resumes` - Each cursor id and the listing path the requester resumes it with
async fn unrefused_resumes(
    requester: &Thorium,
    resumes: &[(String, String)],
) -> Result<Vec<String>, Error> {
    // collect every resume that isn't refused like an unknown id
    let mut unrefused = Vec::new();
    for (id, path) in resumes {
        // resume the saved cursor
        let resume = answer_of(requester, &format!("{path}&limit=1&cursor={id}"), id).await?;
        // ask the same listing for an unknown id
        let unknown = Uuid::new_v4().to_string();
        let unknown_path = format!("{path}&limit=1&cursor={unknown}");
        let missing = answer_of(requester, &unknown_path, &unknown).await?;
        // a resume must be refused, and with the same status and body as an unknown id
        if resume.0 == 200 || resume != missing {
            unrefused.push(format!("{path}: {resume:?}, unknown id: {missing:?}"));
        }
    }
    Ok(unrefused)
}

/// Save a cursor on each listing and pair its id with each path a requester resumes it with
///
/// # Arguments
///
/// * `owner` - The client saving the cursors
/// * `listings` - Each listing's path for the owner, and the paths the requester resumes it with
async fn saved_cursors(
    owner: &Thorium,
    listings: &[(String, Vec<String>)],
) -> Result<Vec<(String, String)>, Error> {
    // pair each saved cursor with every path it is resumed with
    let mut resumes = Vec::new();
    for (path, resume_paths) in listings {
        // the owner reads a first page and gets a cursor
        let id = first_cursor(owner, &format!("{path}&limit=1")).await?;
        for resume_path in resume_paths {
            resumes.push((id.clone(), resume_path.clone()));
        }
    }
    Ok(resumes)
}

/// Build the listings of some routes over some groups, and the paths a requester resumes them
/// with: over its own group, and over the groups it is allowed by default
///
/// # Arguments
///
/// * `routes` - The routes to list, such as `files` or `files/count`
/// * `groups` - The groups the cursors are saved over
/// * `own` - The group the requester names when it resumes
fn narrowed_listings(routes: &[&str], groups: &[&str], own: &str) -> Vec<(String, Vec<String>)> {
    // name every group the cursors are saved over
    let query = groups
        .iter()
        .map(|group| format!("groups[]={group}"))
        .collect::<Vec<_>>()
        .join("&");
    // list each route over those groups, resumed over the requester's group and over none
    routes
        .iter()
        .map(|route| {
            (
                format!("/api/{route}/?{query}"),
                vec![
                    format!("/api/{route}/?groups[]={own}"),
                    format!("/api/{route}/?"),
                ],
            )
        })
        .collect()
}

/// Resume each saved cursor once and list every resume that failed or returned a row with none
/// of its `sha256`, `id`, or `name` among the allowed keys
///
/// # Arguments
///
/// * `requester` - The client resuming the cursors
/// * `resumes` - Each cursor id and the listing path the requester resumes it with
/// * `allowed` - The keys of the rows the requester may get back
async fn rows_outside(
    requester: &Thorium,
    resumes: &[(String, String)],
    allowed: &HashSet<String>,
) -> Result<Vec<String>, Error> {
    // collect every failed resume and every row outside the allowed keys
    let mut outside = Vec::new();
    for (id, path) in resumes {
        // resume the saved cursor
        let (status, body) =
            answer_of(requester, &format!("{path}&limit=1&cursor={id}"), id).await?;
        if status != 200 {
            outside.push(format!("{path}: {status} {body}"));
            continue;
        }
        // check the key of every row it returned
        let page: Value = serde_json::from_str(&body)?;
        for row in page["data"].as_array().into_iter().flatten() {
            let known = ["sha256", "id", "name"].iter().any(|field| {
                row[*field]
                    .as_str()
                    .is_some_and(|key| allowed.contains(key))
            });
            if !known {
                outside.push(format!("{path}: {row}"));
            }
        }
    }
    Ok(outside)
}

/// Save a cursor over the in-scope and out-of-scope groups on each route, paired with the path
/// that resumes it over the in-scope group
///
/// # Arguments
///
/// * `narrowed` - The admin and its groups
/// * `routes` - The routes to list, such as `files`
async fn in_out_cursors(
    narrowed: &Narrowed,
    routes: &[&str],
) -> Result<Vec<(String, String)>, Error> {
    // list each route over both groups, resumed over the in-scope group
    let (in_scope, out_scope) = (&narrowed.in_scope, &narrowed.out_scope);
    let listings: Vec<(String, Vec<String>)> = routes
        .iter()
        .map(|route| {
            (
                format!("/api/{route}/?groups[]={in_scope}&groups[]={out_scope}"),
                vec![format!("/api/{route}/?groups[]={in_scope}")],
            )
        })
        .collect();
    saved_cursors(&narrowed.admin, &listings).await
}

/// Remove the member of the narrowed groups from the shared group
///
/// # Arguments
///
/// * `narrowed` - The admin, the member, and the shared group
async fn remove_member(narrowed: &Narrowed) -> Result<(), Error> {
    // directly remove the member from the shared group
    let remove = GroupUsersUpdate::default().direct_remove(&narrowed.member_name);
    narrowed
        .admin
        .groups
        .update(&narrowed.shared, &GroupUpdate::default().users(remove))
        .await?;
    Ok(())
}

/// Time and tag count cursors resume only over the groups the requester can still list: a
/// scoped token resumes its user's cursor over its in-scope group alone, a user removed from
/// every group a cursor was saved over gets the answer for an unknown id, and the cursors the
/// admin saved still resume for the admin
#[tokio::test]
async fn time_cursors_resume_only_over_groups_the_requester_can_list() -> Result<(), Error> {
    // build the admin, its scoped token, and the member of the shared group
    let narrowed = narrowed().await?;
    let admin = &narrowed.admin;
    // give every group more files and entities than one page holds, keeping the in-scope keys
    let mut in_scope = HashSet::new();
    for group in [&narrowed.in_scope, &narrowed.out_scope, &narrowed.shared] {
        for _ in 0..2 {
            let sha256 = admin
                .files
                .create(generators::gen_sample(group))
                .await?
                .sha256;
            let entity = generators::vendor_entity(group, admin)
                .await?
                .id
                .to_string();
            if *group == narrowed.in_scope {
                in_scope.extend([sha256, entity]);
            }
        }
    }
    // the admin saves file and entity cursors for its scoped token to resume, and file count
    // and listing cursors to resume itself
    let scoped_resumes = in_out_cursors(&narrowed, &["files", "entities"]).await?;
    let count_resumes = in_out_cursors(&narrowed, &["files/count"]).await?;
    let admin_resumes = in_out_cursors(&narrowed, &["files", "entities"]).await?;
    // the member saves cursors over the shared group alone
    let routes = ["files", "entities", "files/count"];
    let member_listings = narrowed_listings(&routes, &[&narrowed.shared], &narrowed.shared);
    let member_resumes = saved_cursors(&narrowed.member, &member_listings).await?;
    remove_member(&narrowed).await?;
    // the removed member resumes none of their cursors
    let unrefused = unrefused_resumes(&narrowed.member, &member_resumes).await?;
    assert!(
        unrefused.is_empty(),
        "these cursors were resumed over a group the requester can't list: {unrefused:#?}"
    );
    // the scoped token resumes the admin's cursors but reads only its in-scope group
    let outside = rows_outside(&narrowed.scoped, &scoped_resumes, &in_scope).await?;
    assert!(
        outside.is_empty(),
        "a scoped token resumed a cursor outside its scope: {outside:#?}"
    );
    // and it resumes the admin's file count over its in-scope group
    for (id, path) in &count_resumes {
        let page = get_json(&narrowed.scoped, &format!("{path}&limit=1&cursor={id}")).await?;
        assert_eq!(page["total"].as_u64(), Some(2), "{path} counted {page}");
    }
    // the admin still resumes the cursors it saved over both groups by one row
    for (id, path) in &admin_resumes {
        let path = path.replace(
            &format!("groups[]={}", narrowed.in_scope),
            &format!(
                "groups[]={}&groups[]={}",
                narrowed.in_scope, narrowed.out_scope
            ),
        );
        let page = get_json(admin, &format!("{path}&limit=1&cursor={id}")).await?;
        let rows = page["data"].as_array().map_or(0, Vec::len);
        assert_eq!(rows, 1, "the admin didn't resume {path} by one row: {page}");
    }
    Ok(())
}

/// Grouped cursors resume only over the groups the requester can still list: a scoped token
/// resumes its user's cursor over its in-scope group alone, a user removed from every group a
/// cursor was saved over gets the answer for an unknown id, and the cursors the admin saved
/// still resume for the admin
#[tokio::test]
async fn grouped_cursors_resume_only_over_groups_the_requester_can_list() -> Result<(), Error> {
    // build the admin, its scoped token, and the member of the shared group
    let narrowed = narrowed().await?;
    let admin = &narrowed.admin;
    // give every group more network policies than one page holds, keeping the in-scope names
    let mut in_scope = HashSet::new();
    for group in [&narrowed.in_scope, &narrowed.out_scope, &narrowed.shared] {
        let policies = generators::network_policies(std::slice::from_ref(group), 2, admin).await?;
        if *group == narrowed.in_scope {
            in_scope.extend(policies.into_iter().map(|policy| policy.name));
        }
    }
    // the admin saves cursors for its scoped token to resume and to resume itself
    let scoped_resumes = in_out_cursors(&narrowed, &["network-policies"]).await?;
    let admin_resumes = in_out_cursors(&narrowed, &["network-policies"]).await?;
    // the member saves cursors over the shared group alone
    let routes = ["network-policies"];
    let member_listings = narrowed_listings(&routes, &[&narrowed.shared], &narrowed.shared);
    let member_resumes = saved_cursors(&narrowed.member, &member_listings).await?;
    remove_member(&narrowed).await?;
    // the removed member resumes none of their cursors
    let unrefused = unrefused_resumes(&narrowed.member, &member_resumes).await?;
    assert!(
        unrefused.is_empty(),
        "these cursors were resumed over a group the requester can't list: {unrefused:#?}"
    );
    // the scoped token resumes the admin's cursors but reads only its in-scope group
    let outside = rows_outside(&narrowed.scoped, &scoped_resumes, &in_scope).await?;
    assert!(
        outside.is_empty(),
        "a scoped token resumed a cursor outside its scope: {outside:#?}"
    );
    // the admin still resumes the cursors it saved over both groups by one row
    for (id, path) in &admin_resumes {
        let path = path.replace(
            &format!("groups[]={}", narrowed.in_scope),
            &format!(
                "groups[]={}&groups[]={}",
                narrowed.in_scope, narrowed.out_scope
            ),
        );
        let page = get_json(admin, &format!("{path}&limit=1&cursor={id}")).await?;
        let rows = page["data"].as_array().map_or(0, Vec::len);
        assert_eq!(rows, 1, "the admin didn't resume {path} by one row: {page}");
    }
    Ok(())
}

/// An admin paging every group's files and network policies without naming groups keeps paging
/// after an unrelated, empty group is deleted
#[tokio::test]
async fn admin_listing_survives_group_deletion() -> Result<(), Error> {
    // get admin client and two groups, the second left empty
    let client = test_utilities::admin_client().await?;
    let groups: Vec<String> = generators::groups(2, &client)
        .await?
        .into_iter()
        .map(|group| group.name)
        .collect();
    // give the first group two files and network policies so each listing takes two pages
    generators::samples(&groups[0], 2, &client).await?;
    generators::network_policies(std::slice::from_ref(&groups[0]), 2, &client).await?;
    // start listing every group's files and network policies
    let mut cursors = Vec::new();
    for route in ["files", "network-policies"] {
        let id = first_cursor(&client, &format!("/api/{route}/?limit=1")).await?;
        cursors.push((route, id));
    }
    // delete the empty second group, then continue each listing
    client.groups.delete(&groups[1]).await?;
    for (route, id) in &cursors {
        let path = format!("/api/{route}/?limit=1&cursor={id}");
        let (status, body) = answer_of(&client, &path, id).await?;
        assert_eq!(
            status, 200,
            "continuing {route} after deleting group {}: {body}",
            groups[1]
        );
    }
    Ok(())
}

/// A user removed from one of two groups whose rows tie at the page boundary resumes their
/// listing over the group they are still in, and the saved tie in the removed group is never
/// read: no row of the removed group comes back
#[tokio::test]
async fn resumed_listing_never_reads_a_removed_groups_tie() -> Result<(), Error> {
    // build the member's groups, one shared with the admin and one of their own
    let narrowed = narrowed().await?;
    let member_groups = narrowed.member.users.info().await?.groups;
    let own = member_groups
        .iter()
        .find(|group| **group != narrowed.shared)
        .cloned()
        .unwrap_or_default();
    let session = test_utilities::scylla_session().await?;
    let ns = &test_utilities::CONF.thorium.namespace;
    // every row shares one timestamp in one bucket
    let at = Utc::now() - Duration::minutes(5);
    let year = chrono::Datelike::year(&at);
    let size = test_utilities::CONF.thorium.files.partition_size;
    let bucket = thorium::utils::helpers::partition(at, year, size);
    let insert = format!(
        "INSERT INTO {ns}.samples_list (group, year, bucket, uploaded, id, sha256) VALUES (?, ?, ?, ?, ?, ?)"
    );
    let redis = test_utilities::redis_pool().await;
    let mut conn = redis
        .get()
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    // four different submissions in each group, keeping the ones in the member's own group
    let mut kept = HashSet::new();
    for group in [&own, &narrowed.shared] {
        for _ in 0..4 {
            let sha256 = format!("{:0>64}", Uuid::new_v4().simple());
            session
                .query_unpaged(
                    insert.clone(),
                    (group, year, bucket, at, Uuid::new_v4(), &sha256),
                )
                .await
                .map_err(|err| Error::new(err.to_string()))?;
            if *group == own {
                kept.insert(sha256);
            }
        }
        let (): () = bb8_redis::redis::cmd("ZADD")
            .arg(format!("{ns}:census:samples:stream:{group}:{year}"))
            .arg(bucket)
            .arg(bucket)
            .query_async(&mut *conn)
            .await
            .map_err(|err| Error::new(err.to_string()))?;
    }
    // the member reads one row of both groups, leaving tied rows in each
    let both = format!("/api/files/?groups[]={own}&groups[]={}", narrowed.shared);
    let page = get_json(&narrowed.member, &format!("{both}&limit=1")).await?;
    let id = page["cursor"].as_str().unwrap_or_default().to_owned();
    // the saved cursor holds a tie in the shared group
    let username = &narrowed.member_name;
    let key = format!("{ns}:cursors:Scylla:v2:{username}:{id}");
    let saved: Option<String> = bb8_redis::redis::cmd("GET")
        .arg(&key)
        .query_async(&mut *conn)
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    drop(conn);
    let saved: Value = serde_json::from_str(&saved.unwrap_or_default())?;
    let tied_shared = saved["ties"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|tie| tie[0] == narrowed.shared.as_str());
    assert!(tied_shared, "no tie was saved in the shared group: {saved}");
    remove_member(&narrowed).await?;
    // page the rest over the member's own group
    let mut rows: Vec<Value> = page["data"].as_array().cloned().unwrap_or_default();
    let mut cursor = Some(id);
    let mut pages = 0;
    while let Some(id) = cursor {
        pages += 1;
        assert!(pages < MAX_PAGES, "the resumed listing never ended");
        let path = format!("/api/files/?groups[]={own}&limit=1&cursor={id}");
        let page = get_json(&narrowed.member, &path).await?;
        rows.extend(page["data"].as_array().cloned().unwrap_or_default());
        cursor = page["cursor"].as_str().map(str::to_owned);
    }
    // every row after the first comes from the member's own group, and each comes back once
    let resumed: Vec<&str> = rows[1..]
        .iter()
        .filter_map(|row| row["sha256"].as_str())
        .collect();
    let outside: Vec<&&str> = resumed.iter().filter(|sha| !kept.contains(**sha)).collect();
    assert!(
        outside.is_empty(),
        "the resumed listing read rows of the removed group: {outside:?}"
    );
    let first = rows[0]["sha256"].as_str().unwrap_or_default();
    let expected: HashSet<String> = kept.iter().filter(|sha| *sha != first).cloned().collect();
    let got: HashSet<String> = resumed.iter().map(|sha| (*sha).to_owned()).collect();
    assert_eq!(
        got, expected,
        "the resumed listing skipped rows of the kept group"
    );
    Ok(())
}

/// The clients and groups a non-member listing test uses
struct NonMember {
    /// A user in neither group
    other: Thorium,
    /// A group the other user isn't in
    group: String,
    /// The other user's own group
    other_group: String,
}

/// Build an admin's group and a user who is only in a group of their own
async fn non_member() -> Result<(Thorium, NonMember), Error> {
    // get admin client and a user that isn't in the admin's group
    let owner = test_utilities::admin_client().await?;
    let other = generators::client(&owner).await?;
    let group = generators::groups(1, &owner).await?.remove(0).name;
    let other_group = generators::groups(1, &other).await?.remove(0).name;
    Ok((
        owner,
        NonMember {
            other,
            group,
            other_group,
        },
    ))
}

/// Check that a user listing a group they aren't in is refused
///
/// # Arguments
///
/// * `client` - The client of the user who isn't in the group
/// * `path` - The listing's path and query naming the group
async fn assert_refused(client: &Thorium, path: &str) -> Result<(), Error> {
    // the listing is refused before anything is listed
    let status = status_of(client, path).await?;
    assert_eq!(status, 401, "{path} returned {status} for a non-member");
    Ok(())
}

/// A user listing files in a group they aren't in is refused
#[tokio::test]
async fn files_listing_refuses_a_non_member() -> Result<(), Error> {
    // give a group a file that its non-member must not list
    let (owner, case) = non_member().await?;
    owner
        .files
        .create(generators::gen_sample(&case.group))
        .await?;
    // listing that group is refused
    assert_refused(&case.other, &format!("/api/files/?groups[]={}", case.group)).await
}

/// A user listing entities in a group they aren't in is refused
#[tokio::test]
async fn entities_listing_refuses_a_non_member() -> Result<(), Error> {
    // give a group an entity that its non-member must not list
    let (owner, case) = non_member().await?;
    generators::vendor_entity(&case.group, &owner).await?;
    // listing that group is refused
    let path = format!("/api/entities/?groups[]={}", case.group);
    assert_refused(&case.other, &path).await
}

/// A user listing a shared file's associations in a group they aren't in is refused
#[tokio::test]
async fn associations_listing_refuses_a_non_member() -> Result<(), Error> {
    // a file in both groups has an association in the group the user isn't in
    let (owner, case) = non_member().await?;
    let mut req = generators::gen_sample(&case.group);
    req.groups.push(case.other_group.clone());
    let sha256 = owner.files.create(req).await?.sha256;
    let assoc = AssociationRequest::new(
        AssociationKind::AssociatedWith,
        AssociationTarget::File(sha256.clone()),
    )
    .groups(vec![case.group.clone()])
    .target(AssociationTarget::Repo(
        "provider.tld/refused/repo".to_owned(),
    ));
    owner.associations.create(&assoc).await?;
    // listing the file's associations in that group is refused
    let path = format!("/api/files/associations/{sha256}?groups[]={}", case.group);
    assert_refused(&case.other, &path).await
}

/// A user listing a shared repo's commitishes in a group they aren't in is refused
#[tokio::test]
async fn commitishes_listing_refuses_a_non_member() -> Result<(), Error> {
    // a repo in both groups
    let (owner, case) = non_member().await?;
    let mut req = generators::gen_repo(&case.group);
    req.groups.push(case.other_group.clone());
    owner.repos.create(&req).await?;
    // listing the repo's commitishes in the group the user isn't in is refused
    let path = format!("/api/repos/commitishes/{}?groups[]={}", req.url, case.group);
    assert_refused(&case.other, &path).await
}

/// A user listing network policies in a group they aren't in is refused
#[tokio::test]
async fn network_policies_listing_refuses_a_non_member() -> Result<(), Error> {
    // give a group a network policy that its non-member must not list
    let (owner, case) = non_member().await?;
    generators::network_policies(std::slice::from_ref(&case.group), 1, &owner).await?;
    // listing that group is refused
    let path = format!("/api/network-policies/?groups[]={}", case.group);
    assert_refused(&case.other, &path).await
}

/// A user listing repos in a group they aren't in is refused
#[tokio::test]
async fn repos_listing_refuses_a_non_member() -> Result<(), Error> {
    // give a group a repo that its non-member must not list
    let (owner, case) = non_member().await?;
    generators::repos(&case.group, 1, &owner).await?;
    // listing that group is refused
    assert_refused(&case.other, &format!("/api/repos/?groups[]={}", case.group)).await
}

/// A bucket that one commitish kind's census stream reaches before another kind's stream does
/// is read once per page, so its commitishes are listed once with each group once
#[tokio::test]
async fn commitish_buckets_are_pulled_once() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // create a group and a repo in it
    let group = generators::groups(1, &client).await?.remove(0).name;
    let repo = generators::repos(&group, 1, &client).await?.remove(0).url;
    // upload a data blob for our commitishes
    let path = std::env::temp_dir().join(format!("thorium-pulled-once-{}", Uuid::new_v4()));
    tokio::fs::write(&path, "commitishes pulled once").await?;
    let tar = TarredRepo {
        name: "data".to_owned(),
        repo: path.clone(),
    };
    let upload = client.repos.upload(&repo, tar, vec![group.clone()]).await;
    tokio::fs::remove_file(&path).await?;
    let data = upload?.sha256;
    // one commit in each of 40 consecutive buckets, more than one census read returns
    let size = i64::from(test_utilities::CONF.thorium.repos.partition_size);
    let now = Utc::now();
    let at = |index: i64| now - Duration::seconds(index * size + 5);
    let mut commitishes: HashMap<String, CommitishRequest> = (0..40)
        .map(|index| {
            let commit = CommitishRequest::Commit(CommitRequest {
                author: "io".to_owned(),
                timestamp: at(index),
                topic: None,
                description: None,
                truncated: false,
            });
            (format!("c{index}"), commit)
        })
        .collect();
    // a branch only in the bucket of c35, below the commit stream's first read
    let branch = CommitishRequest::Branch(BranchRequest {
        commit: "c35".to_owned(),
        timestamp: at(35),
    });
    commitishes.insert("b35".to_owned(), branch);
    // listings stop at the earliest commit, so give one before our commitishes
    let map = CommitishMapRequest {
        groups: vec![group.clone()],
        earliest: Some(at(45)),
        end: true,
        commitishes,
    };
    client.repos.add_commits(&repo, &data, &map).await?;
    // one page holds every commitish
    let page = get_json(&client, &format!("/api/repos/commitishes/{repo}?limit=100")).await?;
    let rows = page["data"].as_array().cloned().unwrap_or_default();
    // every commit and the branch are listed exactly once
    let mut expected: HashSet<String> = (0..40).map(|index| format!("Commit:c{index}")).collect();
    expected.insert("Branch:b35".to_owned());
    assert_each_once(&commitish_keys(&rows), "key", &expected);
    // each commitish names its group once
    let repeated: Vec<&Value> = rows
        .iter()
        .filter(|row| {
            let inner = row.as_object().and_then(|map| map.values().next());
            let groups = inner.and_then(|inner| inner["groups"].as_array());
            groups.is_none_or(|groups| groups.len() != 1)
        })
        .collect();
    assert!(
        repeated.is_empty(),
        "commitishes repeat a group: {repeated:?}"
    );
    Ok(())
}
