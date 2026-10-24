//! The census statements count the same partitions the API records in its census streams

use bb8_redis::redis::AsyncCommands;
use chrono::{Duration, Utc};
use futures::StreamExt;
use std::collections::{BTreeSet, HashMap};

use thorium::models::schema::queries::ScyllaQueries;
use thorium::models::{
    Association, AssociationKind, AssociationRequest, AssociationTarget, CensusSupport,
    CommitRequest, Commitish, CommitishMapRequest, CommitishRequest, Entity, EntityMetadataRequest,
    EntityRequest, Repo, Sample, TagCensusCaseInsensitive, TagRequest, TarredRepo,
};
use thorium::test_utilities::{self, generators};
use thorium::{Error, Thorium};

/// The census members one census kind calls for: each stream key and the buckets in it
type Members = BTreeSet<(String, i32)>;

/// Upload a repo data blob and add one commit to it, as a repo's commitishes are written
///
/// # Arguments
///
/// * `client` - The client to write with
/// * `repo` - The repo to add a commit to
/// * `group` - The group to share the commit with
/// * `hash` - The commit's hash
async fn commit(client: &Thorium, repo: &str, group: &str, hash: &str) -> Result<(), Error> {
    // write the blob's content to a file of its own
    let path = std::env::temp_dir().join(format!(
        "thorium-census-data-{}",
        generators::gen_string(16)
    ));
    tokio::fs::write(&path, hash).await?;
    // upload the file as the repo's data
    let tar = TarredRepo {
        name: "data".to_owned(),
        repo: path.clone(),
    };
    let resp = client.repos.upload(repo, tar, vec![group.to_owned()]).await;
    tokio::fs::remove_file(&path).await?;
    let data = resp?.sha256;
    // add one commit from an hour ago to the blob as its only batch
    let timestamp = Utc::now() - Duration::hours(1);
    let commit = CommitishRequest::Commit(CommitRequest {
        author: "vesta".to_owned(),
        timestamp,
        topic: None,
        description: None,
        truncated: false,
    });
    let batch = CommitishMapRequest {
        groups: vec![group.to_owned()],
        earliest: Some(timestamp),
        end: true,
        commitishes: HashMap::from([(hash.to_owned(), commit)]),
    };
    client.repos.add_commits(repo, &data, &batch).await?;
    Ok(())
}

/// Write rows of every census kind into one group through the API
///
/// # Arguments
///
/// * `client` - The client to write with
/// * `group` - The group to write into
async fn seed(client: &Thorium, group: &str) -> Result<(), Error> {
    // tagged samples and repos fill the sample, repo, and both tag censuses
    generators::samples_tagged(group, 3, client).await?;
    let (_, _, repos) = generators::repos_tagged(group, 2, client).await?;
    // a commit on each repo fills the commitish census
    for (index, repo) in repos.iter().enumerate() {
        let url = client.repos.get(&repo.url).await?.url;
        commit(client, &url, group, &format!("c{index}")).await?;
    }
    // entities fill the entity census, and an association between two of them and a sample
    // fills the association census from both sides
    let mut targets = Vec::new();
    for _ in 0..2 {
        let name = generators::gen_string(24);
        let req = EntityRequest::new(&name, EntityMetadataRequest::Other, vec![group]);
        let id = client.entities.create(req).await?.id;
        targets.push(AssociationTarget::Entity { id, name });
    }
    let sha256 = client
        .files
        .create(generators::gen_sample(group))
        .await?
        .sha256;
    let source = targets.remove(0);
    let req = AssociationRequest::new(AssociationKind::AssociatedWith, source)
        .target(targets.remove(0))
        .target(AssociationTarget::File(sha256));
    client.associations.create(&req).await?;
    Ok(())
}

/// Read the members a census kind calls for through its census statement, keeping the
/// partitions of one group; fails unless the statement the census prepares is the generated one
///
/// # Arguments
///
/// * `name` - The generated census statement's name
/// * `group` - The group to keep
async fn counted<T: CensusSupport>(name: &str, group: &str) -> Members {
    let session = test_utilities::scylla_session().await.unwrap();
    let ns = &test_utilities::CONF.thorium.namespace;
    // the census prepares the statement generated from the table's declaration
    let generated = ScyllaQueries::statements(ns)
        .into_iter()
        .find(|(statement, _)| statement == name)
        .map_or_else(
            || panic!("no generated census statement {name}"),
            |(_, cql)| cql,
        );
    let prepared = T::scan_prepared_statement(&session, ns).await.unwrap();
    assert_eq!(prepared.get_statement(), generated, "{name}");
    // count every partition of the ring
    let pager = session
        .execute_iter(prepared, (i64::MIN, i64::MAX))
        .await
        .unwrap();
    let mut rows = pager.rows_stream::<T::Row>().unwrap();
    let mut members = Members::new();
    while let Some(row) = rows.next().await {
        let row = row.unwrap();
        // keep the partitions of this group, whose stream keys name it
        let key = T::stream_key_from_row(ns, &row);
        if key.contains(&format!(":{group}:")) {
            members.insert((key, T::get_bucket(&row)));
        }
    }
    members
}

/// Read the census members the API wrote for one census family and group
///
/// # Arguments
///
/// * `family` - The census family, as its stream keys name it
/// * `group` - The group to read
async fn recorded(family: &str, group: &str) -> Members {
    let ns = &test_utilities::CONF.thorium.namespace;
    let pool = test_utilities::redis_pool().await;
    let mut conn = pool.get().await.unwrap();
    // find this family's streams for the group
    let pattern = format!("{ns}:census:{family}:stream:*");
    // scan for the keys rather than list them, so a large Redis is not blocked while we read
    let keys: Vec<String> = {
        let found = conn.scan_match::<_, String>(&pattern).await.unwrap();
        found.collect().await
    };
    let mut members = Members::new();
    for key in keys {
        // a family's key names the group after its kind segments
        if !key.contains(&format!(":{group}:")) {
            continue;
        }
        let buckets: Vec<i32> = conn.zrange(&key, 0, -1).await.unwrap();
        members.extend(buckets.into_iter().map(|bucket| (key.clone(), bucket)));
    }
    members
}

/// For each census statement, the buckets counted from Scylla by the generated census statement
/// are the census members the API wrote for the same rows
#[tokio::test]
async fn census_statements_count_the_recorded_buckets() -> Result<(), Error> {
    // write rows of every census kind into a fresh group
    let client = test_utilities::admin_client().await?;
    let group = generators::groups(1, &client).await?.remove(0).name;
    seed(&client, &group).await?;
    // each census kind counts the members its family recorded
    let kinds = [
        (
            "samples",
            counted::<Sample>("samples_list.census", &group).await,
        ),
        ("repos", counted::<Repo>("repos_list.census", &group).await),
        (
            "commitish",
            counted::<Commitish>("commitish_list.census", &group).await,
        ),
        (
            "tags",
            counted::<TagRequest<Sample>>("tags.census", &group).await,
        ),
        (
            "tags_case_insensitive",
            counted::<TagCensusCaseInsensitive<Sample>>("tags_case_insensitive.census", &group)
                .await,
        ),
        (
            "entities",
            counted::<Entity>("entities.census", &group).await,
        ),
        (
            "associations",
            counted::<Association>("associations.census", &group).await,
        ),
    ];
    for (family, counted) in kinds {
        let recorded = recorded(family, &group).await;
        assert!(!recorded.is_empty(), "{family} recorded no census members");
        assert_eq!(counted, recorded, "{family}");
    }
    Ok(())
}
