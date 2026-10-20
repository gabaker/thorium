//! Tests that listings page through rows sharing a timestamp without skipping or repeating any

use base64::Engine;
use chrono::{Duration, Utc};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

use thorium::models::{
    AssociationKind, AssociationRequest, AssociationTarget, BranchRequest, Buffer, CommitRequest,
    CommitishMapRequest, CommitishRequest, EntityRequest, GitTagRequest, RepoRequest, Resources,
    SampleRequest, TarredRepo,
};
use thorium::test_utilities::{self, generators};
use thorium::{Error, Thorium};

/// The most pages a listing may take before it is treated as never ending
const MAX_PAGES: usize = 50;

/// The user the admin client is bootstrapped as, whose name scopes every cursor it saves
const ADMIN_USER: &str = "thorium";

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

/// A commitish cursor saved with a tie holding only a key resumes and finishes
#[tokio::test]
async fn saved_commitish_cursor_resumes() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // build a repo whose commitishes main, c9, c1, and a1 share one timestamp
    let repo = repo_with_tied_commitishes(&client).await?;
    let path = format!("/api/repos/commitishes/{repo}");
    // read a first page of two rows, which ends inside the tied rows
    let page = get_json(&client, &format!("{path}?limit=2")).await?;
    let first = commitish_keys(page["data"].as_array().map_or(&[], Vec::as_slice));
    let id = page["cursor"]
        .as_str()
        .ok_or_else(|| Error::new("the first page has no cursor"))?
        .to_owned();
    // rewrite the saved cursor's ties to hold only the key of the first unconsumed row
    let ns = &test_utilities::CONF.thorium.namespace;
    let key = format!("{ns}:cursors:Scylla:{ADMIN_USER}:{id}");
    let redis = test_utilities::redis_pool().await;
    let mut conn = redis
        .get()
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    let saved: String = bb8_redis::redis::cmd("GET")
        .arg(&key)
        .query_async(&mut *conn)
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    let mut retain: Value = serde_json::from_str(&saved)?;
    let ties = retain["ties"]
        .as_object_mut()
        .ok_or_else(|| Error::new(format!("the saved cursor has no ties: {saved}")))?;
    for tie in ties.values_mut() {
        if let Some(key) = tie.get("key").cloned() {
            *tie = key;
        }
    }
    let (): () = bb8_redis::redis::cmd("SET")
        .arg(&key)
        .arg(retain.to_string())
        .query_async(&mut *conn)
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    drop(conn);
    // resume the saved cursor two rows at a time until it ends
    let mut rows = Vec::new();
    let mut cursor = Some(id);
    for _ in 0..MAX_PAGES {
        let Some(current) = cursor.take() else { break };
        let page = get_json(&client, &format!("{path}?limit=2&cursor={current}")).await?;
        rows.extend(page["data"].as_array().cloned().unwrap_or_default());
        cursor = page["cursor"].as_str().map(str::to_owned);
    }
    assert!(cursor.is_none(), "the saved cursor never finished");
    // together the two halves return every commitish once
    let mut keys = first;
    keys.extend(commitish_keys(&rows));
    let expected: HashSet<String> = ["Branch:main", "Commit:c9", "Commit:c1", "Branch:a1"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    assert_each_once(&keys, "key", &expected);
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

/// An association cursor saved with a tie holding only a target resumes and finishes
#[tokio::test]
async fn saved_association_cursor_resumes() -> Result<(), Error> {
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
    // get the timestamp the rows share
    let path = format!("/api/files/associations/{sha256}");
    let all = page_all(&client, &path, 10).await?;
    let created = all[0]["created"].clone();
    // a tie saved with only a target holds the target column, the serialized repo target
    let tied = serde_json::to_string(&serde_json::json!({ "Repo": targets[2] }))?;
    // save a cursor in the format a first page of two rows writes, with a tie holding only a
    // target
    let id = Uuid::new_v4();
    let retain = serde_json::json!({
        "start": created,
        "end": "2021-01-01T00:00:00Z",
        "extra_filter": serde_json::to_string(&serde_json::json!({ "File": sha256 }))?,
        "group_by": [group],
        "ties": { group.clone(): tied },
        "tags_retain": null,
    });
    let ns = &test_utilities::CONF.thorium.namespace;
    let redis = test_utilities::redis_pool().await;
    let mut conn = redis
        .get()
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    let (): () = bb8_redis::redis::cmd("SET")
        .arg(format!("{ns}:cursors:Scylla:{ADMIN_USER}:{id}"))
        .arg(retain.to_string())
        .query_async(&mut *conn)
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    drop(conn);
    // resume the saved cursor two rows at a time until it ends
    let mut rows = Vec::new();
    let mut cursor = Some(id.to_string());
    for _ in 0..MAX_PAGES {
        let Some(current) = cursor.take() else { break };
        let page = get_json(&client, &format!("{path}?limit=2&cursor={current}")).await?;
        rows.extend(page["data"].as_array().cloned().unwrap_or_default());
        cursor = page["cursor"].as_str().map(str::to_owned);
    }
    assert!(cursor.is_none(), "the saved cursor never finished");
    // it returns the tied target and every target after it once
    let keys: Vec<Value> = keys_at(&rows, "/other/Repo")
        .into_iter()
        .map(|key| serde_json::json!({ "key": key }))
        .collect();
    assert_each_once(&keys, "key", &targets[2..].iter().cloned().collect());
    Ok(())
}
