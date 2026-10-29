//! Integration tests for `YaraRule` entity CRUD and quarantine exemptions

mod common;

use std::collections::BTreeSet;

use test_utilities::generators;
use thorium::fail;
use thorium::models::{
    Entity, EntityMetadata, EntityMetadataRequest, EntityMetadataUpdate, EntityUpdate, TagRequest,
    YaraRule, YaraRuleAppliesTo,
};
use thorium::test_utilities;

/// Pull the yara rule metadata off an entity or panic
///
/// # Arguments
///
/// * `entity` - The entity to pull yara rule metadata from
fn yara_meta(entity: &Entity) -> &YaraRule {
    // make sure this entity carries yara rule metadata
    match &entity.metadata {
        EntityMetadata::YaraRule(rule) => rule,
        meta => panic!("entity carries the wrong metadata kind: {meta:?}"),
    }
}

/// Verify a yara entity can be created and matches its request
#[tokio::test]
async fn create() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // build a yara rule entity request and verify creation
    let req = generators::gen_entity(&group, generators::gen_yara_meta());
    common::check_create(&client, &group, req).await
}

/// Verify a yara entity is updated as requested
#[tokio::test]
async fn update() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // build a yara rule entity request and verify updates apply
    let req = generators::gen_entity(&group, generators::gen_yara_meta());
    common::check_update_meta(&client, req, generators::gen_yara_update).await
}

/// Verify a yara entity can be deleted
#[tokio::test]
async fn delete() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // build a yara rule entity request and verify deletion
    let req = generators::gen_entity(&group, generators::gen_yara_meta());
    common::check_delete(&client, req).await
}

/// Verify creating a yara entity with a broken rule body is rejected with a 400
#[tokio::test]
async fn invalid_rule_create_400() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // build a yara rule with a syntactically broken body; the client side
    // constructor only does pure checks so this still builds locally
    let rule = YaraRule::new("rule broken {", YaraRuleAppliesTo::Samples)
        .expect("failed to build broken yara rule");
    // build a yara rule entity request around our broken rule
    let req = generators::gen_entity(&group, EntityMetadataRequest::YaraRule(rule));
    // the server must compile validate on create and reject this with a 400
    let resp = client.entities.create(req).await;
    fail!(resp, 400);
    Ok(())
}

/// Verify updating a yara entity with a broken rule body is rejected with a 400
///
/// This asserts the update path re-validates rule bodies, which the sigma
/// update path does not do.
#[tokio::test]
async fn invalid_rule_update_400() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // create a valid yara rule entity to update
    let req = generators::gen_entity(&group, generators::gen_yara_meta());
    let (_, entity) = generators::entity(req, &client).await?;
    // build an update that swaps in a syntactically broken rule body
    let update = EntityUpdate::default().metadata(EntityMetadataUpdate::YaraRule {
        yara_rule: Some("rule broken {".to_owned()),
        score: None,
        add_applies_to: Vec::new(),
        remove_applies_to: Vec::new(),
        add_actions: Vec::new(),
        remove_actions: BTreeSet::new(),
    });
    // the server must compile validate on update and reject this with a 400
    let resp = client.entities.update(entity.id, update).await;
    fail!(resp, 400);
    Ok(())
}

/// Verify setting an exemption also clears noisy/slow quarantine status
#[tokio::test]
async fn exemption_set_clears_noisy_slow_tags() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // create a yara rule entity to exempt
    let req = generators::gen_entity(&group, generators::gen_yara_meta());
    let (_, entity) = generators::entity(req, &client).await?;
    // quarantine the rule exactly like the scanner would
    let noisy_tag = TagRequest::<Entity>::default()
        .groups(vec![group.clone()])
        .add(YaraRule::STATUS_TAG, YaraRule::STATUS_NOISY);
    client.entities.tag(entity.id, &noisy_tag).await?;
    // exempt the rule from quarantine
    let resp = client.entities.set_quarantine_exemption(entity.id).await?;
    assert_eq!(resp.status().as_u16(), 204);
    // get the rule back and make sure the flag is set
    let updated = client.entities.get(entity.id).await?;
    assert!(yara_meta(&updated).quarantine_exempt);
    // the exemption must have cleared the noisy quarantine status in the
    // same action so the rule rejoins scanning
    assert!(updated.tags.get(YaraRule::STATUS_TAG).is_none());
    Ok(())
}

/// Verify only admins may manage quarantine exemptions
#[tokio::test]
async fn exemption_requires_admin() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // create a non admin member of the group so the rule is visible to them
    let user = common::member(&client, &[&group]).await?;
    // create a yara rule entity as the admin
    let req = generators::gen_entity(&group, generators::gen_yara_meta());
    let (_, entity) = generators::entity(req, &client).await?;
    // a non admin must not be able to set an exemption
    let resp = user.entities.set_quarantine_exemption(entity.id).await;
    fail!(resp, 401);
    // a non admin must not be able to clear an exemption either
    let resp = user.entities.clear_quarantine_exemption(entity.id).await;
    fail!(resp, 401);
    Ok(())
}

/// Verify an exemption survives an unrelated metadata update
#[tokio::test]
async fn exemption_survives_unrelated_update() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // create a yara rule entity to exempt
    let req = generators::gen_entity(&group, generators::gen_yara_meta());
    let (_, entity) = generators::entity(req, &client).await?;
    // exempt the rule from quarantine
    client.entities.set_quarantine_exemption(entity.id).await?;
    // build an update that swaps in a compile valid rule body and a new score
    let update = EntityUpdate::default().metadata(EntityMetadataUpdate::YaraRule {
        yara_rule: Some("rule updated { condition: true }".to_owned()),
        score: Some(42),
        add_applies_to: Vec::new(),
        remove_applies_to: Vec::new(),
        add_actions: Vec::new(),
        remove_actions: BTreeSet::new(),
    });
    client.entities.update(entity.id, update).await?;
    // get the rule back and make sure the update applied
    let updated = client.entities.get(entity.id).await?;
    let rule = yara_meta(&updated);
    assert_eq!(rule.rule, "rule updated { condition: true }");
    assert_eq!(rule.score, 42);
    // the exemption flag must survive the unrelated update
    assert!(rule.quarantine_exempt);
    Ok(())
}

/// Verify clearing an exemption leaves tags alone
#[tokio::test]
async fn exemption_clear_leaves_tags_alone() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // create a yara rule entity to exempt
    let req = generators::gen_entity(&group, generators::gen_yara_meta());
    let (_, entity) = generators::entity(req, &client).await?;
    // exempt the rule from quarantine
    client.entities.set_quarantine_exemption(entity.id).await?;
    // add an unrelated tag to the rule
    let (key, value) = (generators::gen_string(12), generators::gen_string(12));
    let unrelated = TagRequest::<Entity>::default()
        .groups(vec![group.clone()])
        .add(&key, &value);
    client.entities.tag(entity.id, &unrelated).await?;
    // tag the rule Noisy like the scanner would so the clear has a quarantine
    // status it must leave alone (only setting an exemption clears statuses)
    let noisy = TagRequest::<Entity>::default()
        .groups(vec![group.clone()])
        .add(YaraRule::STATUS_TAG, YaraRule::STATUS_NOISY);
    client.entities.tag(entity.id, &noisy).await?;
    // clear the exemption
    let resp = client
        .entities
        .clear_quarantine_exemption(entity.id)
        .await?;
    assert_eq!(resp.status().as_u16(), 204);
    // get the rule back and make sure only the flag changed
    let updated = client.entities.get(entity.id).await?;
    assert!(!yara_meta(&updated).quarantine_exempt);
    // the unrelated tag must be untouched by the clear
    assert!(
        updated
            .tags
            .get(&key)
            .is_some_and(|values| values.contains_key(&value))
    );
    // clearing must never delete (or add to) the quarantine status values;
    // only setting an exemption clears the stale noisy/slow statuses
    assert!(
        updated
            .tags
            .get(YaraRule::STATUS_TAG)
            .is_some_and(|values| values.contains_key(YaraRule::STATUS_NOISY))
    );
    Ok(())
}

/// Verify exemptions reject non yara entities with a 400
#[tokio::test]
async fn exemption_rejects_non_yara() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // create a sigma rule which quarantine exemption does not apply to
    let req = generators::gen_entity(&group, generators::gen_sigma_meta());
    let (_, entity) = generators::entity(req, &client).await?;
    // an exemption on a non yara entity must be rejected
    let resp = client.entities.set_quarantine_exemption(entity.id).await;
    fail!(resp, 400);
    Ok(())
}
