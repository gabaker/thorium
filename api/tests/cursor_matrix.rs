//! Tests that every cursor family pages through rows spread over many buckets and several years,
//! leap years included, returning each row once in order

use chrono::{DateTime, Duration, TimeZone, Utc};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

use thorium::models::{CommitishKinds, EntityKinds, Resources, TagType};
use thorium::test_utilities::{self, generators};
use thorium::utils::helpers;
use thorium::{Error, Thorium};

/// The most pages a listing may take before it is treated as never ending
const MAX_PAGES: usize = 200;

/// The newest timestamp every time listing in these tests starts from
const START: &str = "2024-12-31T00:00:00Z";

/// The oldest timestamp every time listing in these tests ends at
const END: &str = "2020-06-01T00:00:00Z";

/// Send a GET to the API with this client's credentials and return its JSON body
///
/// # Arguments
///
/// * `client` - The client whose credentials to send
/// * `path` - The path and query to request, starting with `/api/`
async fn get_json(client: &Thorium, path: &str) -> Result<Value, Error> {
    // get the token for this client
    let token = client.users.info().await?.token;
    // send our raw request so the cursor and limit reach the API unchanged
    let resp = reqwest::Client::new()
        .get(format!("{}{path}", client.host))
        .header("authorization", test_utilities::auth_header(&token))
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
/// # Arguments
///
/// * `client` - The client to list with
/// * `path` - The listing's path and query, without its page size or cursor
/// * `param` - The name of the page size parameter
/// * `limit` - The page size to request
async fn page_all(
    client: &Thorium,
    path: &str,
    param: &str,
    limit: usize,
) -> Result<Vec<Value>, Error> {
    let sep = if path.contains('?') { '&' } else { '?' };
    let mut rows = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_PAGES {
        // request the next page, continuing from our cursor if we have one
        let page_path = match &cursor {
            Some(id) => format!("{path}{sep}{param}={limit}&cursor={id}"),
            None => format!("{path}{sep}{param}={limit}"),
        };
        let page = get_json(client, &page_path).await?;
        let data = page["data"].as_array().cloned().unwrap_or_default();
        assert!(data.len() <= limit, "{path} returned too many rows");
        rows.extend(data);
        // stop once the cursor is exhausted
        match page["cursor"].as_str() {
            Some(id) => cursor = Some(id.to_owned()),
            None => return Ok(rows),
        }
    }
    panic!("{path} never ended after {MAX_PAGES} pages");
}

/// The timestamps the time listings' rows are spread over: the last seconds of a leap year and
/// of a regular year, the first seconds of the next year, a leap day, and a run of rows one
/// bucket apart that spans more buckets than one pull reads
///
/// # Arguments
///
/// * `partition_size` - The partition size of the listing's table, in seconds
fn spread(partition_size: u16) -> Vec<DateTime<Utc>> {
    let at = |year, month, day, hour, minute, second| {
        Utc.with_ymd_and_hms(year, month, day, hour, minute, second)
            .unwrap()
    };
    let mut times = vec![
        at(2020, 12, 31, 23, 59, 30),
        at(2021, 1, 1, 0, 0, 0),
        at(2021, 1, 1, 0, 0, 1),
        at(2023, 12, 31, 23, 59, 59),
        at(2024, 1, 1, 0, 0, 5),
        at(2024, 2, 29, 12, 0, 0),
    ];
    // one row in each of 120 consecutive buckets
    let base = at(2024, 6, 1, 0, 0, 30);
    times.extend((0..120).map(|step| base + Duration::seconds(i64::from(partition_size) * step)));
    times
}

/// The year and bucket a row written at a timestamp is stored in
///
/// # Arguments
///
/// * `time` - The row's timestamp
/// * `partition_size` - The partition size of the row's table, in seconds
fn year_bucket(time: DateTime<Utc>, partition_size: u16) -> (i32, i32) {
    let year = chrono::Datelike::year(&time);
    (year, helpers::partition(time, year, partition_size))
}

/// Add buckets to a census stream, as writing rows to them does
///
/// # Arguments
///
/// * `key` - The census stream's key
/// * `bucket` - The bucket to add
async fn census(key: &str, bucket: i32) -> Result<(), Error> {
    let redis = test_utilities::redis_pool().await;
    let mut conn = redis
        .get()
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    let (): () = bb8_redis::redis::cmd("ZADD")
        .arg(key)
        .arg(bucket)
        .arg(bucket)
        .query_async(&mut *conn)
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    Ok(())
}

/// Run one statement against the test keyspace
///
/// # Arguments
///
/// * `session` - The Scylla session
/// * `cql` - The statement, with `{ns}` for the keyspace
/// * `values` - The values to bind
async fn execute(
    session: &scylla::client::session::Session,
    cql: &str,
    values: impl scylla::serialize::row::SerializeRow,
) -> Result<(), Error> {
    let ns = &test_utilities::CONF.thorium.namespace;
    session
        .query_unpaged(cql.replace("{ns}", ns), values)
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    Ok(())
}

/// Read the timestamp of a listed row at a JSON pointer
///
/// # Arguments
///
/// * `row` - The listed row
/// * `pointer` - The pointer to its timestamp
fn time_at(row: &Value, pointer: &str) -> DateTime<Utc> {
    let raw = row
        .pointer(pointer)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("{row} has no timestamp at {pointer}"));
    DateTime::parse_from_rfc3339(raw)
        .unwrap()
        .with_timezone(&Utc)
}

/// Check that a listing returned each expected key once, newest first
///
/// # Arguments
///
/// * `name` - The listing, for failures
/// * `rows` - The listed rows
/// * `key` - The pointer to each row's key
/// * `time` - The pointer to each row's timestamp
/// * `expected` - The keys the listing must return
fn assert_listed(name: &str, rows: &[Value], key: &str, time: &str, expected: &HashSet<String>) {
    // count each key
    let mut seen: HashMap<String, usize> = HashMap::new();
    for row in rows {
        let value = match row.pointer(key) {
            Some(Value::String(value)) => value.clone(),
            Some(other) => other.to_string(),
            None => panic!("{name}: {row} has no key at {key}"),
        };
        *seen.entry(value).or_default() += 1;
    }
    let missing = expected
        .iter()
        .filter(|key| !seen.contains_key(*key))
        .count();
    let repeated = seen.values().filter(|count| **count > 1).count();
    assert!(
        missing == 0 && repeated == 0 && seen.len() == expected.len(),
        "{name}: {missing} missing and {repeated} repeated of {} rows",
        rows.len()
    );
    // the rows come back newest first
    let times = rows
        .iter()
        .map(|row| time_at(row, time))
        .collect::<Vec<_>>();
    assert!(
        times.windows(2).all(|pair| pair[0] >= pair[1]),
        "{name}: rows aren't newest first"
    );
}

/// Sample listings page newest first across buckets and years from a descending table
#[tokio::test]
async fn sample_listings_cross_buckets_and_years() -> Result<(), Error> {
    let client = test_utilities::admin_client().await?;
    let session = test_utilities::scylla_session().await?;
    let ns = &test_utilities::CONF.thorium.namespace;
    let size = test_utilities::CONF.thorium.files.partition_size;
    let group = generators::groups(1, &client).await?.remove(0).name;
    // write one submission at each timestamp, with its census bucket
    let mut expected = HashSet::new();
    for time in spread(size) {
        let (year, bucket) = year_bucket(time, size);
        let sha256 = format!("{:0>64}", Uuid::new_v4().simple());
        execute(
            &session,
            "INSERT INTO {ns}.samples_list (group, year, bucket, uploaded, id, sha256) VALUES (?, ?, ?, ?, ?, ?)",
            (&group, year, bucket, time, Uuid::new_v4(), &sha256),
        )
        .await?;
        census(
            &format!("{ns}:census:samples:stream:{group}:{year}"),
            bucket,
        )
        .await?;
        expected.insert(sha256);
    }
    // page through them at several page sizes
    let path = format!("/api/files/?groups[]={group}&start={START}&end={END}");
    for limit in [1, 7, 50] {
        let rows = page_all(&client, &path, "limit", limit).await?;
        assert_listed("files", &rows, "/sha256", "/uploaded", &expected);
    }
    Ok(())
}

/// Repo listings page newest first across buckets and years
#[tokio::test]
async fn repo_listings_cross_buckets_and_years() -> Result<(), Error> {
    let client = test_utilities::admin_client().await?;
    let session = test_utilities::scylla_session().await?;
    let ns = &test_utilities::CONF.thorium.namespace;
    let size = test_utilities::CONF.thorium.repos.partition_size;
    let group = generators::groups(1, &client).await?.remove(0).name;
    // write one repo submission at each timestamp, with its census bucket
    let mut expected = HashSet::new();
    for time in spread(size) {
        let (year, bucket) = year_bucket(time, size);
        let url = format!("provider.tld/matrix/{}", Uuid::new_v4().simple());
        execute(
            &session,
            "INSERT INTO {ns}.repos_list (group, year, bucket, uploaded, id, url) VALUES (?, ?, ?, ?, ?, ?)",
            (&group, year, bucket, time, Uuid::new_v4(), &url),
        )
        .await?;
        census(&format!("{ns}:census:repos:stream:{group}:{year}"), bucket).await?;
        expected.insert(url);
    }
    let path = format!("/api/repos/?groups[]={group}&start={START}&end={END}");
    for limit in [7, 50] {
        let rows = page_all(&client, &path, "limit", limit).await?;
        assert_listed("repos", &rows, "/url", "/uploaded", &expected);
    }
    Ok(())
}

/// Entity listings of two kinds page newest first across buckets and years from an ascending
/// table read in reverse
#[tokio::test]
async fn entity_listings_cross_buckets_and_years() -> Result<(), Error> {
    let client = test_utilities::admin_client().await?;
    let session = test_utilities::scylla_session().await?;
    let ns = &test_utilities::CONF.thorium.namespace;
    let size = test_utilities::CONF.thorium.entities.partition_size;
    let group = generators::groups(1, &client).await?.remove(0).name;
    // write one entity at each timestamp, alternating between two kinds
    let mut expected = HashSet::new();
    for (index, time) in spread(size).into_iter().enumerate() {
        let (year, bucket) = year_bucket(time, size);
        let kind = if index % 2 == 0 {
            EntityKinds::Device
        } else {
            EntityKinds::Other
        };
        let id = Uuid::new_v4();
        execute(
            &session,
            "INSERT INTO {ns}.entities (kind, group, year, bucket, created, id, name) VALUES (?, ?, ?, ?, ?, ?, ?)",
            (serde_json::to_string(&kind)?, &group, year, bucket, time, id, format!("matrix-{index}")),
        )
        .await?;
        census(
            &format!("{ns}:census:entities:stream:{group}:{year}"),
            bucket,
        )
        .await?;
        expected.insert(id.to_string());
    }
    let path = format!(
        "/api/entities/?groups[]={group}&kinds[]=Device&kinds[]=Other&start={START}&end={END}"
    );
    for limit in [7, 50] {
        let rows = page_all(&client, &path, "limit", limit).await?;
        assert_listed("entities", &rows, "/id", "/created", &expected);
    }
    Ok(())
}

/// Association listings page newest first across buckets and years from a table whose ties
/// cluster ascending
#[tokio::test]
async fn association_listings_cross_buckets_and_years() -> Result<(), Error> {
    let client = test_utilities::admin_client().await?;
    let session = test_utilities::scylla_session().await?;
    let ns = &test_utilities::CONF.thorium.namespace;
    let size = test_utilities::CONF.thorium.associations.partition_size;
    let group = generators::groups(1, &client).await?.remove(0).name;
    // associate from a real sample
    let sha256 = client
        .files
        .create(generators::gen_sample(&group))
        .await?
        .sha256;
    let source = serde_json::to_string(&serde_json::json!({ "File": sha256 }))?;
    // write one association to a new repo at each timestamp
    let mut expected = HashSet::new();
    for time in spread(size) {
        let (year, bucket) = year_bucket(time, size);
        let url = format!("provider.tld/matrix/{}", Uuid::new_v4().simple());
        let target = serde_json::to_string(&serde_json::json!({ "Repo": url }))?;
        execute(
            &session,
            "INSERT INTO {ns}.associations (group, year, bucket, source, created, target, direction, kind, submitter) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            (&group, year, bucket, &source, time, &target, "To", "AssociatedWith", "matrix"),
        )
        .await?;
        census(
            &format!("{ns}:census:associations:stream:{group}:{source}:{year}"),
            bucket,
        )
        .await?;
        expected.insert(url);
    }
    let path = format!("/api/files/associations/{sha256}?groups[]={group}&start={START}&end={END}");
    for limit in [7, 50] {
        let rows = page_all(&client, &path, "limit", limit).await?;
        assert_listed("associations", &rows, "/other/Repo", "/created", &expected);
    }
    Ok(())
}

/// Commitish listings of two kinds page newest first across buckets and years
#[tokio::test]
async fn commitish_listings_cross_buckets_and_years() -> Result<(), Error> {
    let client = test_utilities::admin_client().await?;
    let session = test_utilities::scylla_session().await?;
    let ns = &test_utilities::CONF.thorium.namespace;
    let size = test_utilities::CONF.thorium.repos.partition_size;
    let group = generators::groups(1, &client).await?.remove(0).name;
    let repo = generators::repos(&group, 1, &client).await?.remove(0).url;
    // write one commit or branch at each timestamp, with its kind's census bucket
    let mut expected = HashSet::new();
    for (index, time) in spread(size).into_iter().enumerate() {
        let (year, bucket) = year_bucket(time, size);
        let kind = if index % 2 == 0 {
            CommitishKinds::Commit
        } else {
            CommitishKinds::Branch
        };
        let key = format!("matrix{index:03}");
        execute(
            &session,
            "INSERT INTO {ns}.commitish_list (kind, group, year, bucket, repo, timestamp, key, repo_data) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            (kind.as_str(), &group, year, bucket, &repo, time, &key, "data"),
        )
        .await?;
        census(
            &format!("{ns}:census:commitish:stream:{kind}:{group}:{repo}:{year}"),
            bucket,
        )
        .await?;
        expected.insert(format!("{kind}:{key}"));
    }
    let path = format!(
        "/api/repos/commitishes/{repo}?groups[]={group}&kinds[]=Commit&kinds[]=Branch&start={START}&end={END}"
    );
    for limit in [7, 50] {
        let rows = page_all(&client, &path, "limit", limit).await?;
        // key each commitish by its kind and its hash or name
        let keyed = rows
            .iter()
            .map(|row| {
                let (kind, inner) = row.as_object().and_then(|map| map.iter().next()).unwrap();
                let key = inner
                    .get("hash")
                    .or_else(|| inner.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                serde_json::json!({ "key": format!("{kind}:{key}"), "timestamp": inner["timestamp"] })
            })
            .collect::<Vec<_>>();
        assert_listed("commitishes", &keyed, "/key", "/timestamp", &expected);
    }
    Ok(())
}

/// Tag-filtered listings page newest first across buckets and years, matching tags as given
/// and in any case
#[tokio::test]
async fn tag_listings_cross_buckets_and_years() -> Result<(), Error> {
    let client = test_utilities::admin_client().await?;
    let session = test_utilities::scylla_session().await?;
    let ns = &test_utilities::CONF.thorium.namespace;
    let size = test_utilities::CONF.thorium.tags.files.partition_size;
    let group = generators::groups(1, &client).await?.remove(0).name;
    // tag one new item at each timestamp with a key only this run uses
    let key = format!("Matrix{}", &Uuid::new_v4().simple().to_string()[..8]);
    let value = "Corn";
    let mut expected = HashSet::new();
    for time in spread(size) {
        let (year, bucket) = year_bucket(time, size);
        let item = format!("{:0>64}", Uuid::new_v4().simple());
        execute(
            &session,
            "INSERT INTO {ns}.tags (type, group, year, bucket, key, value, uploaded, item, key_lower, value_lower) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            (
                "Files",
                &group,
                year,
                bucket,
                &key,
                value,
                time,
                &item,
                key.to_lowercase(),
                value.to_lowercase(),
            ),
        )
        .await?;
        let files = TagType::Files;
        census(
            &format!("{ns}:census:tags:stream:{files}:{group}:{key}:{value}:{year}"),
            bucket,
        )
        .await?;
        census(
            &format!(
                "{ns}:census:tags_case_insensitive:stream:{files}:{group}:{}:{}:{year}",
                key.to_lowercase(),
                value.to_lowercase()
            ),
            bucket,
        )
        .await?;
        expected.insert(item);
    }
    // list by the tag as written, and in another case
    let exact =
        format!("/api/files/?groups[]={group}&tags[{key}][]={value}&start={START}&end={END}");
    let any_case = format!(
        "/api/files/?groups[]={group}&tags[{}][]={}&tags_case_insensitive=true&start={START}&end={END}",
        key.to_uppercase(),
        value.to_lowercase()
    );
    for path in [exact, any_case] {
        for limit in [7, 50] {
            let rows = page_all(&client, &path, "limit", limit).await?;
            assert_listed("tagged files", &rows, "/sha256", "/uploaded", &expected);
        }
    }
    Ok(())
}

/// Node listings page through each cluster in turn, by node name
#[tokio::test]
async fn node_listings_page_through_clusters() -> Result<(), Error> {
    let client = test_utilities::admin_client().await?;
    let run = &Uuid::new_v4().simple().to_string()[..8];
    let clusters = [format!("matrix{run}b"), format!("matrix{run}a")];
    // register three nodes in the first cluster and two in the second
    let mut expected = Vec::new();
    for (cluster, count) in clusters.iter().zip([3, 2]) {
        for index in 0..count {
            let node = format!("node{index}");
            generators::node(cluster, &node, Resources::default(), &client).await?;
            expected.push(format!("{cluster}/{node}"));
        }
    }
    // the clusters are listed in the order asked for, and each cluster's nodes by name
    let path = format!(
        "/api/system/nodes/?clusters[]={}&clusters[]={}",
        clusters[0], clusters[1]
    );
    for page_size in [1, 2, 10] {
        let rows = page_all(&client, &path, "page_size", page_size).await?;
        let listed = rows
            .iter()
            .map(|row| {
                format!(
                    "{}/{}",
                    row["cluster"].as_str().unwrap(),
                    row["node"].as_str().unwrap()
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(listed, expected, "page size {page_size}");
    }
    Ok(())
}
