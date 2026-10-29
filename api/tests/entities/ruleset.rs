//! Integration tests for `RuleSet` entities and their capped membership

mod common;

use uuid::Uuid;

use thorium::models::{
    AssociationKind, AssociationListOpts, AssociationTarget, EntityMetadata, EntityMetadataRequest,
    EntityMetadataUpdate, EntityRequest, EntityUpdate, RuleSetRequest,
};
use thorium::test_utilities::generators;
use thorium::{Thorium, fail, is};

/// Create a rule entity from some metadata and return its id
///
/// # Arguments
///
/// * `client` - The client to create this rule with
/// * `group` - The group this rule should be in
/// * `meta` - The rule metadata request to create
async fn create_rule(
    client: &Thorium,
    group: &str,
    meta: EntityMetadataRequest,
) -> Result<Uuid, thorium::Error> {
    // build the entity request for this rule with a random name
    let req = EntityRequest::new(generators::gen_string(16), meta, vec![group.to_owned()]);
    // create this rule entity
    let resp = client.entities.create(req).await?;
    Ok(resp.id)
}

/// List the member ids a rule sets associations point at
///
/// # Arguments
///
/// * `client` - The client to list associations with
/// * `ruleset` - The id of the rule set to list members for
async fn list_member_associations(
    client: &Thorium,
    ruleset: Uuid,
) -> Result<Vec<Uuid>, thorium::Error> {
    // list this rule sets associations
    let opts = AssociationListOpts::default().limit(1000);
    let mut cursor = client.associations.list_entity(ruleset, &opts).await?;
    // collect the member ids from the rule set member associations
    let mut members = Vec::default();
    loop {
        // crawl this page for rule set member associations
        for assoc in cursor.data.drain(..) {
            // skip any associations that are not rule set members
            if assoc.kind == AssociationKind::RuleSetMember {
                // keep any entity targets
                if let AssociationTarget::Entity { id, .. } = &assoc.other {
                    members.push(*id);
                }
            }
        }
        // stop crawling once this cursor is exhausted
        if cursor.exhausted() {
            break;
        }
        // get the next page of associations
        cursor.refill().await?;
    }
    Ok(members)
}

/// Verify rule sets create with mixed engine members and hydrate them
#[tokio::test]
async fn create() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // create two yara rules and one sigma rule to be members
    let yara_one = create_rule(&client, &group, generators::gen_yara_meta()).await?;
    let yara_two = create_rule(&client, &group, generators::gen_yara_meta()).await?;
    let sigma = create_rule(&client, &group, generators::gen_sigma_meta()).await?;
    // build a rule set holding all three rules across both engines
    let req = generators::gen_entity(
        &group,
        generators::gen_ruleset_meta(vec![yara_one, yara_two, sigma]),
    );
    // create the rule set and make sure it matches and lists
    common::check_create(&client, &group, req.clone()).await?;
    // get the created rule set back by listing our group
    let listed = common::list_all(&client, &group).await?;
    let ruleset = listed
        .into_iter()
        .find(|entity| entity.name == req.name)
        .expect("created rule set was not listed");
    // the hydrated members must carry their tags for the hunt snapshot filter
    let EntityMetadata::RuleSet(meta) = &ruleset.metadata else {
        panic!("rule set entity has the wrong metadata kind");
    };
    assert!(
        meta.rules
            .iter()
            .all(|rule| rule.tags.contains_key("submitter")),
        "hydrated rule set members are missing their tags"
    );
    // the membership must also be navigable through the association graph
    let members = list_member_associations(&client, ruleset.id).await?;
    assert_eq!(members.len(), 3);
    for rule_id in [yara_one, yara_two, sigma] {
        assert!(members.contains(&rule_id), "member {rule_id} is missing");
    }
    Ok(())
}

/// Verify rule sets can be created empty
#[tokio::test]
async fn create_empty() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // build a rule set with no members
    let req = generators::gen_entity(&group, generators::gen_ruleset_meta(Vec::default()));
    // create the rule set and make sure it matches and lists
    common::check_create(&client, &group, req).await?;
    Ok(())
}

/// Verify a rule set naming a nonexistent member is a 404
#[tokio::test]
async fn create_missing_member_404() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // build a rule set naming a member that does not exist
    let req = generators::gen_entity(&group, generators::gen_ruleset_meta(vec![Uuid::new_v4()]));
    // this create must 404 on the missing member
    let resp = client.entities.create(req).await;
    fail!(resp, 404);
    Ok(())
}

/// Verify a rule set naming a non rule member is a 400
#[tokio::test]
async fn create_non_rule_member_400() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // create an entity that is not a rule of any engine
    let other = create_rule(&client, &group, generators::gen_other_meta()).await?;
    // build a rule set naming that non rule entity as a member
    let req = generators::gen_entity(&group, generators::gen_ruleset_meta(vec![other]));
    // this create must 400 on the non rule member
    let resp = client.entities.create(req).await;
    fail!(resp, 400);
    Ok(())
}

/// Verify the membership cap rejects oversized rule sets before any fetches
#[tokio::test]
async fn create_over_cap_400() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // build a rule set with one more member than the default cap; the cap
    // check fires before any existence fetch so random ids are fine
    let over_cap: Vec<Uuid> = (0..1001).map(|_| Uuid::new_v4()).collect();
    let req = generators::gen_entity(&group, generators::gen_ruleset_meta(over_cap));
    // this create must 400 on the membership cap
    let resp = client.entities.create(req).await;
    fail!(resp, 400);
    Ok(())
}

/// Verify rule set membership is editable through the generic update path
#[tokio::test]
async fn update_membership() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // create rules for the initial membership and the added member
    let keep = create_rule(&client, &group, generators::gen_yara_meta()).await?;
    let remove = create_rule(&client, &group, generators::gen_yara_meta()).await?;
    let add = create_rule(&client, &group, generators::gen_sigma_meta()).await?;
    // build a rule set holding the member we keep and the one we remove
    let req = generators::gen_entity(&group, generators::gen_ruleset_meta(vec![keep, remove]));
    // create then update the rule set swapping one member for another
    common::check_update_meta(&client, req, |_| EntityMetadataUpdate::RuleSet {
        add_rules: vec![add],
        remove_rules: vec![remove],
    })
    .await?;
    Ok(())
}

/// Verify adding a non rule member through update is a 400
#[tokio::test]
async fn update_add_non_rule_400() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // create a rule set with one valid member
    let member = create_rule(&client, &group, generators::gen_yara_meta()).await?;
    let req = generators::gen_entity(&group, generators::gen_ruleset_meta(vec![member]));
    let (_, ruleset) = generators::entity(req, &client).await?;
    // create an entity that is not a rule of any engine
    let other = create_rule(&client, &group, generators::gen_other_meta()).await?;
    // build an update adding that non rule entity as a member
    let update = EntityUpdate::default().metadata(EntityMetadataUpdate::RuleSet {
        add_rules: vec![other],
        remove_rules: Vec::default(),
    });
    // this update must 400 on the non rule member
    let resp = client.entities.update(ruleset.id, update).await;
    fail!(resp, 400);
    Ok(())
}

/// Verify deleting a rule set leaves its member rules intact
#[tokio::test]
async fn delete_leaves_members() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // create a rule to be a member
    let member = create_rule(&client, &group, generators::gen_yara_meta()).await?;
    // create a rule set holding that member
    let req = generators::gen_entity(&group, generators::gen_ruleset_meta(vec![member]));
    let (_, ruleset) = generators::entity(req, &client).await?;
    // delete the rule set
    client.entities.delete(ruleset.id).await?;
    // the rule set must be gone
    let resp = client.entities.get(ruleset.id).await;
    fail!(resp, 404);
    // the member rule must still exist with no dangling member association
    let rule = client.entities.get(member).await?;
    is!(rule.id, member);
    // the member rules association back to the rule set must be pruned
    let members = list_member_associations(&client, member).await?;
    assert!(
        members.is_empty(),
        "rule still carries rule set associations after delete"
    );
    Ok(())
}

/// Verify a rule set create built from a raw request round trips its members
#[tokio::test]
async fn request_round_trip() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // create a rule to be a member
    let member = create_rule(&client, &group, generators::gen_yara_meta()).await?;
    // build a rule set request directly from the model
    let req = EntityRequest::new(
        generators::gen_string(16),
        EntityMetadataRequest::RuleSet(RuleSetRequest::new(vec![member])),
        vec![group.clone()],
    );
    // create the rule set and make sure it matches and lists
    common::check_create(&client, &group, req).await?;
    Ok(())
}
