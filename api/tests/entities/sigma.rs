//! Integration tests for `SigmaRule` entity CRUD

mod common;

use test_utilities::generators;
use thorium::test_utilities;

/// Verify a sigma entity can be created and matches its request
#[tokio::test]
async fn create() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // build a sigma rule entity request and verify creation
    let req = generators::gen_entity(&group, generators::gen_sigma_meta());
    common::check_create(&client, &group, req).await
}

/// Verify a sigma entity is updated as requested
#[tokio::test]
async fn update() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // build a sigma rule entity request and verify updates apply
    let req = generators::gen_entity(&group, generators::gen_sigma_meta());
    common::check_update_meta(&client, req, generators::gen_sigma_update).await
}

/// Verify a sigma entity can be deleted
#[tokio::test]
async fn delete() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // build a sigma rule entity request and verify deletion
    let req = generators::gen_entity(&group, generators::gen_sigma_meta());
    common::check_delete(&client, req).await
}

/// Verify creating a sigma entity with an invalid jq filter action is rejected
#[tokio::test]
async fn create_rejects_bad_jq() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // build a sigma rule entity request with an action whose jq filter is invalid
    let req = generators::gen_entity(&group, generators::gen_sigma_meta_bad_jq());
    // try to create this entity and make sure it is rejected
    let error = client
        .entities
        .create(req)
        .await
        .expect_err("sigma rule with a bad jq filter action was accepted");
    // the api should reject it as a bad request
    assert_eq!(error.status().map(|code| code.as_u16()), Some(400));
    Ok(())
}

/// Overwrite an entity's stored kind data with JSON no entity kind parses from
///
/// # Arguments
///
/// * `id` - The id of the entity to corrupt
async fn corrupt_kind_data(id: uuid::Uuid) -> Result<(), thorium::Error> {
    // get a Scylla session and the namespace to write rows directly
    let session = test_utilities::scylla_session().await?;
    let ns = &test_utilities::CONF.thorium.namespace;
    // read the keys of every row of this entity from the by id view
    let select =
        format!("SELECT kind, group, year, bucket, created FROM {ns}.entities_by_id WHERE id = ?");
    let rows = session
        .query_unpaged(select, (id,))
        .await
        .map_err(|err| thorium::Error::new(err.to_string()))?
        .into_rows_result()
        .map_err(|err| thorium::Error::new(err.to_string()))?;
    // collect the key columns of each row
    let keys = rows
        .rows::<(String, String, i32, i32, chrono::DateTime<chrono::Utc>)>()
        .map_err(|err| thorium::Error::new(err.to_string()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| thorium::Error::new(err.to_string()))?;
    // make sure we found this entity's rows
    assert!(!keys.is_empty(), "entity {id} has no rows to corrupt");
    // build the statement that overwrites a row's kind data in the base table
    let update = format!(
        "UPDATE {ns}.entities SET kind_data = ? WHERE kind = ? AND group = ? AND year = ? \
         AND bucket = ? AND created = ? AND id = ?"
    );
    // overwrite each row with an action variant this build doesn't know
    for (kind, group, year, bucket, created) in keys {
        session
            .query_unpaged(
                update.clone(),
                (
                    r#"{"SigmaRule":{"actions":[{"NotAnAction":{}}]}}"#,
                    kind,
                    group,
                    year,
                    bucket,
                    created,
                    id,
                ),
            )
            .await
            .map_err(|err| thorium::Error::new(err.to_string()))?;
    }
    Ok(())
}

/// Verify listings skip an entity whose stored kind data can't be read, page across it, and
/// that reading it by id names it as unreadable
#[tokio::test]
async fn unreadable_kind_data_is_skipped() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // create three sigma rules in this group
    let mut ids = Vec::with_capacity(3);
    for _ in 0..3 {
        let req = generators::gen_entity(&group, generators::gen_sigma_meta());
        let (_, entity) = generators::entity(req, &client).await?;
        ids.push(entity.id);
    }
    // corrupt the stored kind data of the middle rule
    let bad = ids[1];
    corrupt_kind_data(bad).await?;
    // the valid rules are the ones a detailed listing should return
    let mut valid = vec![ids[0], ids[2]];
    valid.sort();
    // list the details of this group's sigma rules one per page and in one page
    for page_size in [1, 10] {
        let opts = thorium::models::EntityListOpts::default()
            .groups(vec![group.clone()])
            .kind(thorium::models::EntityKinds::SigmaRule)
            .page_size(page_size);
        let mut cursor = client.entities.list_details(&opts).await?;
        let mut listed: Vec<uuid::Uuid> = cursor.data.drain(..).map(|entity| entity.id).collect();
        // page through the rest of the cursor, bounding pages so a stuck cursor fails
        let mut pages = 1;
        while !cursor.exhausted() {
            cursor.refill().await?;
            listed.extend(cursor.data.drain(..).map(|entity| entity.id));
            pages += 1;
            assert!(pages <= 10, "the details cursor never finished");
        }
        // only the readable rules are returned
        listed.sort();
        assert_eq!(listed, valid, "page size {page_size}");
    }
    // a plain listing doesn't read kind data so it still lists every rule
    let opts = thorium::models::EntityListOpts::default()
        .groups(vec![group.clone()])
        .kind(thorium::models::EntityKinds::SigmaRule);
    let mut cursor = client.entities.list(&opts).await?;
    let mut listed: Vec<uuid::Uuid> = cursor.data.drain(..).map(|line| line.id).collect();
    // page through the rest of the plain listing
    while !cursor.exhausted() {
        cursor.refill().await?;
        listed.extend(cursor.data.drain(..).map(|line| line.id));
    }
    // every rule, readable or not, is listed
    listed.sort();
    let mut all = ids.clone();
    all.sort();
    assert_eq!(listed, all);
    // reading the corrupt rule by id fails with an error naming it as unreadable
    let error = client
        .entities
        .get(bad)
        .await
        .expect_err("an unreadable entity was returned");
    assert_eq!(error.status().map(|code| code.as_u16()), Some(500));
    // the error names the entity
    let msg = error.msg().unwrap_or_default();
    assert!(
        msg.contains("unreadable") && msg.contains(&bad.to_string()),
        "{msg}"
    );
    // the valid rules can still be read by id
    client.entities.get(ids[0]).await?;
    Ok(())
}
