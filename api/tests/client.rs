//! Integration tests for the client's masquerade and refresh behavior

use thorium::client::{ClientSettings, Users};
use thorium::models::{
    AssociationKind, AssociationRequest, AssociationTarget, ElasticSearchOpts, EntityRequest,
    EntityUpdate, GroupUpdate, GroupUsersUpdate, UserCreate, UserRole,
};
use thorium::test_utilities::{self, generators};
use thorium::{Error, Thorium};
use uuid::Uuid;

/// Get the HTTP status an error carries, failing when it carries none
///
/// # Arguments
///
/// * `result` - The result that should be an error with a status
/// * `context` - What was expected, for failure messages
fn error_status<T>(result: Result<T, Error>, context: &str) -> Result<u16, Error> {
    match result {
        // a success means the call was not limited to the masqueraded user
        Ok(_) => Err(Error::new(format!("{context}: the request succeeded"))),
        // pull the status code out of the error
        Err(error) => error
            .status()
            .map(|status| status.as_u16())
            .ok_or_else(|| Error::new(format!("{context}: the error carried no status: {error}"))),
    }
}

/// Create an entity and return its id
///
/// # Arguments
///
/// * `client` - The client to create the entity with
/// * `group` - The group to create the entity in
async fn create_entity(client: &Thorium, group: &str) -> Result<Uuid, Error> {
    // build a random entity in this group
    let req: EntityRequest = generators::gen_entity(group, generators::gen_other_meta());
    // create it and keep its id
    Ok(client.entities.create(req).await?.id)
}

/// Build an association request between two entities
///
/// # Arguments
///
/// * `source` - The source entity's id
/// * `target` - The target entity's id
fn association(source: Uuid, target: Uuid) -> AssociationRequest {
    // link the two entities with a generic association
    AssociationRequest::new(
        AssociationKind::AssociatedWith,
        AssociationTarget::Entity {
            id: source,
            name: "source".to_owned(),
        },
    )
    .target(AssociationTarget::Entity {
        id: target,
        name: "target".to_owned(),
    })
}

/// Build search options limited to one group
///
/// # Arguments
///
/// * `group` - The group to search in
fn search_in(group: &str) -> ElasticSearchOpts {
    // search everything in this one group
    let mut opts = ElasticSearchOpts::new("*");
    opts.groups = vec![group.to_owned()];
    opts
}

/// A masqueraded client acts as the user for entities, associations, and search
#[tokio::test]
async fn masquerade_limits_entities_associations_and_search() -> Result<(), Error> {
    // get an admin client and a client for a new user
    let admin = test_utilities::admin_client().await?;
    let user_client = generators::client(&admin).await?;
    // the user owns group A and only the admin is in group B
    let group_a = generators::groups(1, &user_client).await?.remove(0).name;
    let group_b = generators::groups(1, &admin).await?.remove(0).name;
    // masquerade a copy of the admin client as the user
    let user = user_client.users.info().await?;
    let mut masked = admin.clone();
    masked.masquerade(&user);
    // an entity created through the masqueraded client is submitted by the user
    let a_id = create_entity(&masked, &group_a).await?;
    let a_entity = admin.entities.get(a_id).await?;
    assert_eq!(
        a_entity.submitter, user.username,
        "masqueraded entity submitter"
    );
    // an entity in group B is hidden from the masqueraded client just as from the user
    let b_id = create_entity(&admin, &group_b).await?;
    test_utilities::assert_status(masked.entities.get(b_id).await, 404, "masqueraded get in B")?;
    // a search of group B is refused the same way it is for the user's own client
    let masked_search = error_status(
        masked.search.search(&search_in(&group_b)).await,
        "masqueraded search of B",
    )?;
    let user_search = error_status(
        user_client.search.search(&search_in(&group_b)).await,
        "user search of B",
    )?;
    assert_eq!(masked_search, user_search, "search of B status");
    assert!(
        matches!(masked_search, 401 | 404),
        "search of B returned {masked_search}"
    );
    // an association naming the group B entity fails as it does for the user's own client
    let masked_assoc = error_status(
        masked.associations.create(&association(a_id, b_id)).await,
        "masqueraded association to B",
    )?;
    let user_assoc = error_status(
        user_client
            .associations
            .create(&association(a_id, b_id))
            .await,
        "user association to B",
    )?;
    assert_eq!(masked_assoc, user_assoc, "association to B status");
    assert!(
        matches!(masked_assoc, 401 | 404),
        "association to B returned {masked_assoc}"
    );
    // after reverting, the same reads succeed as the admin
    masked.revert_masquerade();
    masked.entities.get(b_id).await?;
    // the admin's search of B must get past group authorization, though the Elastic query
    // behind it can still fail when the test deployment has no result indexes
    if let Err(error) = masked.search.search(&search_in(&group_b)).await {
        let status = error.status().map(|status| status.as_u16());
        assert!(
            !matches!(status, Some(401 | 403 | 404)),
            "admin search of B after revert was refused: {error}"
        );
    }
    // associations need a group holding both ends, so the admin adds group B to the A entity
    let update = EntityUpdate::default().group(&group_b);
    masked.entities.update(a_id, update).await?;
    masked.associations.create(&association(a_id, b_id)).await?;
    Ok(())
}

/// A refresh moves every handler and the revert target to the refreshed token
#[tokio::test]
async fn refresh_replaces_masquerade_and_revert_target() -> Result<(), Error> {
    // get an admin client and create a user with a known password
    let admin = test_utilities::admin_client().await?;
    let username = generators::gen_string(24);
    let password = generators::gen_string(64);
    let bp = UserCreate::new(&username, &password, "fake@fake.gov")
        .skip_verification()
        .role(UserRole::User);
    let secret_key = Some(&test_utilities::CONF.thorium.secret_key);
    Users::create(&admin.host, bp, secret_key, &ClientSettings::default()).await?;
    // build a client for this user and a group it owns
    let mut client = Thorium::build(&admin.host)
        .basic_auth(username.clone(), password.clone())
        .build()
        .await?;
    let group = generators::groups(1, &client).await?.remove(0).name;
    // refresh this user's token, which logs out the token the client was built with
    client.refresh(&username, &password).await?;
    // entities created after the refresh are submitted by the refreshed user
    let id = create_entity(&client, &group).await?;
    assert_eq!(
        admin.entities.get(id).await?.submitter,
        username,
        "submitter after refresh"
    );
    // masquerade as a user that shares the group, so its creates are submitted by that user
    let other_client = generators::client(&admin).await?;
    let other = other_client.users.info().await?;
    let users_req = GroupUsersUpdate::default().direct_add(&other.username);
    let update = GroupUpdate::default().users(users_req);
    client.groups.update(&group, &update).await?;
    client.masquerade(&other);
    let id = create_entity(&client, &group).await?;
    assert_eq!(
        admin.entities.get(id).await?.submitter,
        other.username,
        "submitter while masquerading"
    );
    // the old token was logged out, so reverting must land on the refreshed token
    client.revert_masquerade();
    let id = create_entity(&client, &group).await?;
    assert_eq!(
        admin.entities.get(id).await?.submitter,
        username,
        "submitter after revert"
    );
    Ok(())
}
