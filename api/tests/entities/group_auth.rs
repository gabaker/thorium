//! Integration tests for group changes and association writes on existing entity kinds
//!
//! These cover the authorization rules every kind follows, using Devices and their Vendor edges
//! because they are the entity edges that can be created through entity forms.

use chrono::Utc;
use reqwest::multipart::{Form, Part};
use thorium::models::{
    Association, AssociationKind, AssociationRequest, AssociationTarget, Entity, EntityListOpts,
    EntityMetadata, EntityRequest, EntityUpdate, GroupAllowed, GroupAllowedUpdate, GroupUpdate,
    ScopedTokenRequest, TreeOpts, TreeQuery, TreeSupport, UserRole,
};
use thorium::test_utilities::generators::{GroupRoles, TestUser};
use thorium::test_utilities::{self, generators};
use thorium::{Error, Thorium, fail, is};
use uuid::Uuid;

/// Create a plain `User` role user
///
/// # Arguments
///
/// * `admin` - The admin client to create the user with
async fn plain_user(admin: &Thorium) -> Result<TestUser, Error> {
    // create a user with the plain user role
    generators::user_with_role(admin, UserRole::User).await
}

/// Create a vendor in some groups as the admin
///
/// # Arguments
///
/// * `groups` - The groups the vendor should be in
/// * `admin` - The admin client to create the vendor with
async fn vendor_in(groups: &[&str], admin: &Thorium) -> Result<Entity, Error> {
    // build a vendor request in the first group
    let mut req = generators::gen_entity(groups[0], generators::gen_vendor_meta());
    // put the vendor in every requested group
    req.groups = groups.iter().map(|group| (*group).to_owned()).collect();
    // create the vendor and get it back
    let (_, vendor) = generators::entity(req, admin).await?;
    Ok(vendor)
}

/// Create a device in some groups with a list of vendors
///
/// # Arguments
///
/// * `groups` - The groups the device should be in
/// * `vendors` - The vendors that developed this device
/// * `client` - The client to create the device with
async fn device_in(groups: &[&str], vendors: Vec<Uuid>, client: &Thorium) -> Result<Entity, Error> {
    // build a device request in the first group
    let mut req = generators::gen_entity(groups[0], generators::gen_device_meta(vendors));
    // put the device in every requested group
    req.groups = groups.iter().map(|group| (*group).to_owned()).collect();
    // create the device and get it back
    let (_, device) = generators::entity(req, client).await?;
    Ok(device)
}

/// Build an association target for an entity
///
/// # Arguments
///
/// * `entity` - The entity to target
fn target(entity: &Entity) -> AssociationTarget {
    AssociationTarget::Entity {
        id: entity.id,
        name: entity.name.clone(),
    }
}

/// Build a `DevelopedBy` association request from a device to a vendor
///
/// # Arguments
///
/// * `device` - The device the association starts at
/// * `vendor` - The vendor that developed the device
fn developed_by(device: &Entity, vendor: &Entity) -> AssociationRequest {
    AssociationRequest::new(AssociationKind::DevelopedBy, target(device)).target(target(vendor))
}

/// Get the ids of the vendors a device shows
///
/// # Arguments
///
/// * `device` - The device to get vendors from
fn vendor_ids(device: &Entity) -> Vec<Uuid> {
    match &device.metadata {
        EntityMetadata::Device(meta) => meta.vendors.iter().map(|vendor| vendor.id).collect(),
        _ => Vec::new(),
    }
}

/// Check whether a tree scoped to a group links a vendor to a device
///
/// # Arguments
///
/// * `admin` - The admin client to grow the tree with
/// * `group` - The group to scope the tree to
/// * `vendor` - The vendor the tree starts from
/// * `device` - The device to look for a branch to
async fn linked_in(
    admin: &Thorium,
    group: &str,
    vendor: Uuid,
    device: Uuid,
) -> Result<bool, Error> {
    // start a tree from the vendor in just this group
    let mut query = TreeQuery::default().entity(vendor);
    query.groups = vec![group.to_owned()];
    // gather parents too, since the device points at the vendor
    let opts = TreeOpts {
        gather_parents: Some(true),
        ..TreeOpts::default()
    };
    let tree = admin.trees.start(&opts, &query).await?;
    // get the hashes of both ends
    let vendor_hash = Entity::tree_hash_direct(&vendor);
    let device_hash = Entity::tree_hash_direct(&device);
    // look for a branch between them in either direction
    let linked = [(vendor_hash, device_hash), (device_hash, vendor_hash)]
        .iter()
        .any(|(left, right)| {
            tree.branches
                .get(left)
                .is_some_and(|branches| branches.iter().any(|branch| branch.node == *right))
        });
    Ok(linked)
}

/// Adding a group needs edit rights and the `Entities` flag in it, also through a scoped token,
/// and can't overwrite a copy in a group the caller can't see
#[tokio::test]
async fn add_group_needs_edit_rights() -> Result<(), Error> {
    // get an admin client and our test user
    let admin = test_utilities::admin_client().await?;
    let user = plain_user(&admin).await?;
    // the user edits one group, only monitors another, and edits a third that disallows entities
    let editable = generators::group_with_roles(
        &GroupRoles::default().user(&user.username),
        GroupAllowed::default(),
        &admin,
    )
    .await?;
    let monitored = generators::group_with_roles(
        &GroupRoles::default().monitor(&user.username),
        GroupAllowed::default(),
        &admin,
    )
    .await?;
    let closed = generators::group_with_roles(
        &GroupRoles::default().user(&user.username),
        GroupAllowed::default().disable_entities(),
        &admin,
    )
    .await?;
    // a group the user isn't in at all
    let hidden =
        generators::group_with_roles(&GroupRoles::default(), GroupAllowed::default(), &admin)
            .await?;
    // a device in the editable and hidden groups
    let vendor = vendor_in(&[&editable], &admin).await?;
    let device = device_in(&[&editable, &hidden], vec![vendor.id], &admin).await?;
    // adding a group the user only monitors is refused
    let update = EntityUpdate::default().group(&monitored);
    let resp = user.client.entities.update(device.id, update).await;
    fail!(resp, 401);
    // adding a group that disallows entities is refused
    let update = EntityUpdate::default().group(&closed);
    let resp = user.client.entities.update(device.id, update).await;
    fail!(resp, 401);
    // adding the hidden group the device is already in is refused and leaves its copy alone
    let update = EntityUpdate::default()
        .group(&hidden)
        .name(generators::gen_string(16));
    let resp = user.client.entities.update(device.id, update).await;
    fail!(resp, 401);
    let stored = admin.entities.get(device.id).await?;
    is!(stored.name, device.name);
    // nothing was added to the device
    let mut groups = stored.groups.clone();
    groups.sort();
    let mut expected = vec![editable.clone(), hidden.clone()];
    expected.sort();
    is!(groups, expected);
    // a user who can edit both groups but uses a token scoped to one of them
    let editor = plain_user(&admin).await?;
    let in_scope = generators::group_with_roles(
        &GroupRoles::default().user(&editor.username),
        GroupAllowed::default(),
        &admin,
    )
    .await?;
    let out_of_scope = generators::group_with_roles(
        &GroupRoles::default().user(&editor.username),
        GroupAllowed::default(),
        &admin,
    )
    .await?;
    let device = device_in(&[&in_scope], vec![], &admin).await?;
    let scoped = editor
        .client
        .users
        .create_scoped_token(&ScopedTokenRequest::new("group-auth").group(&in_scope))
        .await?;
    let scoped_client = Thorium::build(&admin.host)
        .token(&scoped.token)
        .build()
        .await?;
    // the scoped token can't add a group outside its scope
    let update = EntityUpdate::default().group(&out_of_scope);
    let resp = scoped_client.entities.update(device.id, update).await;
    fail!(resp, 401);
    // the full token can add it
    let update = EntityUpdate::default().group(&out_of_scope);
    editor.client.entities.update(device.id, update).await?;
    let stored = admin.entities.get(device.id).await?;
    is!(stored.groups.contains(&out_of_scope), true);
    Ok(())
}

/// Removing a group needs the same rights as a delete
#[tokio::test]
async fn remove_group_needs_delete_rights() -> Result<(), Error> {
    // get an admin client and our test users
    let admin = test_utilities::admin_client().await?;
    let user = plain_user(&admin).await?;
    let manager = plain_user(&admin).await?;
    // the user edits both groups and the manager manages the group being removed
    let a = generators::group_with_roles(
        &GroupRoles::default()
            .user(&user.username)
            .user(&manager.username),
        GroupAllowed::default(),
        &admin,
    )
    .await?;
    let b = generators::group_with_roles(
        &GroupRoles::default()
            .user(&user.username)
            .manager(&manager.username),
        GroupAllowed::default(),
        &admin,
    )
    .await?;
    // a device in both groups submitted by the admin
    let device = device_in(&[&a, &b], vec![], &admin).await?;
    // a plain user who isn't the submitter can't remove a group
    let update = EntityUpdate::default().remove_group(&b);
    let resp = user.client.entities.update(device.id, update).await;
    fail!(resp, 401);
    let stored = admin.entities.get(device.id).await?;
    is!(stored.groups.len(), 2);
    // a manager of the removed group can
    let update = EntityUpdate::default().remove_group(&b);
    manager.client.entities.update(device.id, update).await?;
    let stored = admin.entities.get(device.id).await?;
    is!(stored.groups, vec![a.clone()]);
    // a submitter can remove a group they only use
    let submitted = device_in(&[&a, &b], vec![], &user.client).await?;
    let update = EntityUpdate::default().remove_group(&b);
    user.client.entities.update(submitted.id, update).await?;
    let stored = admin.entities.get(submitted.id).await?;
    is!(stored.groups, vec![a.clone()]);
    Ok(())
}

/// A missing group is a `401` to non-admins and a `404` to admins, removing a group the entity
/// isn't in is a `401` without the delete right, and device vendors need a writable shared group
/// while a device with no vendors needs no association rights
#[tokio::test]
async fn group_change_and_device_form_codes() -> Result<(), Error> {
    // get an admin client and a user who edits two groups
    let admin = test_utilities::admin_client().await?;
    let user = plain_user(&admin).await?;
    let roles = GroupRoles::default().user(&user.username);
    let a = generators::group_with_roles(&roles, GroupAllowed::default(), &admin).await?;
    let b = generators::group_with_roles(&roles, GroupAllowed::default(), &admin).await?;
    // a device in a submitted by the admin
    let device = device_in(&[&a], vec![], &admin).await?;
    // adding a group that doesn't exist is a 401 to the user and a 404 to the admin
    let missing = format!("missing-{}", generators::gen_string(16));
    let update = EntityUpdate::default().group(&missing);
    let resp = user.client.entities.update(device.id, update).await;
    fail!(resp, 401);
    let update = EntityUpdate::default().group(&missing);
    let resp = admin.entities.update(device.id, update).await;
    fail!(resp, 404);
    // removing a group the device isn't in is a 401 without the delete right and a 400 with it
    let update = EntityUpdate::default().remove_group(&b);
    let resp = user.client.entities.update(device.id, update).await;
    fail!(resp, 401);
    let update = EntityUpdate::default().remove_group(&b);
    let resp = admin.entities.update(device.id, update).await;
    fail!(resp, 400);
    let stored = admin.entities.get(device.id).await?;
    is!(stored.groups, vec![a.clone()]);
    // a device form whose vendor shares no group with it is a 400 and writes nothing
    let b_vendor = vendor_in(&[&b], &admin).await?;
    let req = generators::gen_entity(&a, generators::gen_device_meta(vec![b_vendor.id]));
    let name = req.name.clone();
    let resp = user.client.entities.create(req).await;
    fail!(resp, 400);
    let opts = EntityListOpts::default()
        .groups(vec![a.clone(), b.clone()])
        .limit(1000);
    let listed = admin.entities.list_details(&opts).await?;
    is!(listed.data.iter().any(|entity| entity.name == name), false);
    // a device with no vendors is created in a group that disallows associations
    let closed = generators::group_with_roles(
        &roles,
        GroupAllowed::default().disable_associations(),
        &admin,
    )
    .await?;
    let created = device_in(&[&closed], vec![], &user.client).await?;
    let stored = admin.entities.get(created.id).await?;
    is!(stored.groups, vec![closed.clone()]);
    Ok(())
}

/// A device form with vendors is a `401` that writes nothing when the user can't write
/// associations in any of the device's groups
#[tokio::test]
async fn device_form_needs_a_writable_group() -> Result<(), Error> {
    // get an admin client and a user who edits a group that disallows associations
    let admin = test_utilities::admin_client().await?;
    let user = plain_user(&admin).await?;
    let roles = GroupRoles::default().user(&user.username);
    let closed = generators::group_with_roles(
        &roles,
        GroupAllowed::default().disable_associations(),
        &admin,
    )
    .await?;
    // a vendor in the closed group
    let vendor = vendor_in(&[&closed], &admin).await?;
    // a device form in the closed group naming the vendor is refused
    let req = generators::gen_entity(&closed, generators::gen_device_meta(vec![vendor.id]));
    let name = req.name.clone();
    let resp = user.client.entities.create(req).await;
    fail!(resp, 401);
    // nothing was written
    let opts = EntityListOpts::default()
        .groups(vec![closed.clone()])
        .limit(1000);
    let listed = admin.entities.list_details(&opts).await?;
    is!(listed.data.iter().any(|entity| entity.name == name), false);
    Ok(())
}

/// An entity's edges follow its group changes
#[tokio::test]
async fn edges_follow_group_changes() -> Result<(), Error> {
    // get an admin client and a user who can only see group b
    let admin = test_utilities::admin_client().await?;
    let b_user = plain_user(&admin).await?;
    let a = generators::group_with_roles(&GroupRoles::default(), GroupAllowed::default(), &admin)
        .await?;
    let b = generators::group_with_roles(
        &GroupRoles::default().user(&b_user.username),
        GroupAllowed::default(),
        &admin,
    )
    .await?;
    // a vendor in both groups and a device in a that it developed
    let vendor = vendor_in(&[&a, &b], &admin).await?;
    let device = device_in(&[&a], vec![vendor.id], &admin).await?;
    // the edge starts out in a only
    is!(linked_in(&admin, &a, vendor.id, device.id).await?, true);
    is!(linked_in(&admin, &b, vendor.id, device.id).await?, false);
    // add group b to the device
    admin
        .entities
        .update(device.id, EntityUpdate::default().group(&b))
        .await?;
    // a b-only user sees the vendor on the device
    let seen = b_user.client.entities.get(device.id).await?;
    is!(vendor_ids(&seen), vec![vendor.id]);
    is!(linked_in(&admin, &b, vendor.id, device.id).await?, true);
    // remove group a from the device
    admin
        .entities
        .update(device.id, EntityUpdate::default().remove_group(&a))
        .await?;
    // the vendor's associations in a no longer hold an edge to the device
    is!(linked_in(&admin, &a, vendor.id, device.id).await?, false);
    // the edge in b is untouched
    is!(linked_in(&admin, &b, vendor.id, device.id).await?, true);
    let seen = b_user.client.entities.get(device.id).await?;
    is!(vendor_ids(&seen), vec![vendor.id]);
    Ok(())
}

/// Edges aren't copied into an added group that doesn't allow associations
#[tokio::test]
async fn edges_skip_groups_without_associations() -> Result<(), Error> {
    // get an admin client, an open group, and a group that disallows associations
    let admin = test_utilities::admin_client().await?;
    let a = generators::group_with_roles(&GroupRoles::default(), GroupAllowed::default(), &admin)
        .await?;
    let closed = generators::group_with_roles(
        &GroupRoles::default(),
        GroupAllowed::default().disable_associations(),
        &admin,
    )
    .await?;
    // a vendor in both groups and a device in a that it developed
    let vendor = vendor_in(&[&a, &closed], &admin).await?;
    let device = device_in(&[&a], vec![vendor.id], &admin).await?;
    // add the closed group to the device
    admin
        .entities
        .update(device.id, EntityUpdate::default().group(&closed))
        .await?;
    let stored = admin.entities.get(device.id).await?;
    is!(stored.groups.contains(&closed), true);
    // no edge was copied into the closed group, and the edge in a is untouched
    is!(
        linked_in(&admin, &closed, vendor.id, device.id).await?,
        false
    );
    is!(linked_in(&admin, &a, vendor.id, device.id).await?, true);
    Ok(())
}

/// Rows copied into an added group are created at the group change and submitted by the user who
/// added the group, while the original rows keep their own time and submitter
#[tokio::test]
async fn copied_edges_take_the_adder_and_add_time() -> Result<(), Error> {
    // get an admin client and a user who edits both groups
    let admin = test_utilities::admin_client().await?;
    let admin_name = admin.users.info().await?.username;
    let adder = plain_user(&admin).await?;
    let roles = GroupRoles::default().user(&adder.username);
    let a = generators::group_with_roles(&roles, GroupAllowed::default(), &admin).await?;
    let b = generators::group_with_roles(&roles, GroupAllowed::default(), &admin).await?;
    // a vendor in both groups and a device in a that it developed, both made by the admin
    let vendor = vendor_in(&[&a, &b], &admin).await?;
    let device = device_in(&[&a], vec![vendor.id], &admin).await?;
    // the user adds group b to the device
    let before_add = Utc::now();
    adder
        .client
        .entities
        .update(device.id, EntityUpdate::default().group(&b))
        .await?;
    let after_add = Utc::now();
    // check the rows from both ends of the edge
    for (id, other) in [(device.id, vendor.id), (vendor.id, device.id)] {
        // get this end's rows that point at the other end
        let rows: Vec<Association> = admin
            .entities
            .associations(id)
            .await?
            .into_iter()
            .filter(|row| matches!(&row.other, AssociationTarget::Entity { id: row_id, .. } if *row_id == other))
            .collect();
        // the original row in a keeps the admin and its creation time
        let original = rows
            .iter()
            .find(|row| row.groups.contains(&a))
            .ok_or_else(|| Error::new(format!("no row in {a} for {id}")))?;
        is!(original.submitter, admin_name);
        let made_before_add = original.created < before_add;
        is!(made_before_add, true);
        is!(original.groups.contains(&b), false);
        // the copy in b is created at the group change by the user who added b
        let copy = rows
            .iter()
            .find(|row| row.groups.contains(&b))
            .ok_or_else(|| Error::new(format!("no row in {b} for {id}")))?;
        is!(copy.submitter, adder.username);
        let made_at_add = copy.created >= before_add && copy.created <= after_add;
        is!(made_at_add, true);
        is!(copy.kind, original.kind);
        is!(copy.direction, original.direction);
    }
    Ok(())
}

/// Association writes need edit rights and the `Associations` flag, a shared group, and explicit
/// groups that hold both ends
#[tokio::test]
async fn association_writes_are_authorized() -> Result<(), Error> {
    // get an admin client and our test users
    let admin = test_utilities::admin_client().await?;
    let monitor = plain_user(&admin).await?;
    let user = plain_user(&admin).await?;
    // a group the monitor only monitors and the user edits
    let a = generators::group_with_roles(
        &GroupRoles::default()
            .monitor(&monitor.username)
            .user(&user.username),
        GroupAllowed::default(),
        &admin,
    )
    .await?;
    let device = device_in(&[&a], vec![], &admin).await?;
    let vendor = vendor_in(&[&a], &admin).await?;
    // a monitor can't write associations
    let resp = monitor
        .client
        .associations
        .create(&developed_by(&device, &vendor))
        .await;
    fail!(resp, 401);
    is!(linked_in(&admin, &a, vendor.id, device.id).await?, false);
    // a group that disallows associations refuses them
    let closed = generators::group_with_roles(
        &GroupRoles::default().user(&user.username),
        GroupAllowed::default().disable_associations(),
        &admin,
    )
    .await?;
    let closed_device = device_in(&[&closed], vec![], &admin).await?;
    let closed_vendor = vendor_in(&[&closed], &admin).await?;
    let resp = user
        .client
        .associations
        .create(&developed_by(&closed_device, &closed_vendor))
        .await;
    fail!(resp, 401);
    // a device form with vendors is refused before anything is written
    let req = generators::gen_entity(&closed, generators::gen_device_meta(vec![closed_vendor.id]));
    let name = req.name.clone();
    let resp = user.client.entities.create(req).await;
    fail!(resp, 401);
    let opts = EntityListOpts::default()
        .groups(vec![closed.clone()])
        .limit(1000);
    let listed = admin.entities.list_details(&opts).await?;
    is!(listed.data.iter().any(|entity| entity.name == name), false);
    // two groups the user edits with no entity in both
    let b = generators::group_with_roles(
        &GroupRoles::default().user(&user.username),
        GroupAllowed::default(),
        &admin,
    )
    .await?;
    let b_vendor = vendor_in(&[&b], &admin).await?;
    // no shared group is a 400 naming the groups
    let resp = user
        .client
        .associations
        .create(&developed_by(&device, &b_vendor))
        .await;
    fail!(resp, 400);
    let msg = resp.unwrap_err().msg().unwrap_or_default();
    is!(msg.contains(&a) && msg.contains(&b), true, msg);
    // explicit groups must hold the source
    let mut req = developed_by(&device, &b_vendor);
    req.groups = vec![b.clone()];
    let resp = user.client.associations.create(&req).await;
    fail!(resp, 400);
    // explicit groups must hold every target
    let mut req = developed_by(&device, &b_vendor);
    req.groups = vec![a.clone()];
    let resp = user.client.associations.create(&req).await;
    fail!(resp, 400);
    is!(linked_in(&admin, &a, b_vendor.id, device.id).await?, false);
    is!(linked_in(&admin, &b, b_vendor.id, device.id).await?, false);
    // an editor can write an association with a shared group
    user.client
        .associations
        .create(&developed_by(&device, &vendor))
        .await?;
    is!(linked_in(&admin, &a, vendor.id, device.id).await?, true);
    Ok(())
}

/// Overlap kinds keep partial-view writes
#[tokio::test]
async fn partial_view_updates() -> Result<(), Error> {
    // get an admin client, an editor of a, and a viewer of b
    let admin = test_utilities::admin_client().await?;
    let editor = plain_user(&admin).await?;
    let viewer = plain_user(&admin).await?;
    let a = generators::group_with_roles(
        &GroupRoles::default().user(&editor.username),
        GroupAllowed::default(),
        &admin,
    )
    .await?;
    let b = generators::group_with_roles(
        &GroupRoles::default().user(&viewer.username),
        GroupAllowed::default(),
        &admin,
    )
    .await?;
    // a device in both groups
    let device = device_in(&[&a, &b], vec![], &admin).await?;
    // the editor of a renames it
    let name = generators::gen_string(16);
    editor
        .client
        .entities
        .update(device.id, EntityUpdate::default().name(&name))
        .await?;
    // only the copy in a changed
    let edited = editor.client.entities.get(device.id).await?;
    is!(edited.name, name);
    let b_copy = viewer.client.entities.get(device.id).await?;
    is!(b_copy.name, device.name);
    Ok(())
}

/// Send an entity form with an image named `logo.png`
///
/// # Arguments
///
/// * `user` - The user sending the form
/// * `method` - The method to send the form with
/// * `url` - The url to send the form to
/// * `form` - The form to add the image to
/// * `image` - The image's bytes
async fn send_with_image(
    user: &TestUser,
    method: reqwest::Method,
    url: &str,
    form: Form,
    image: &[u8],
) -> Result<reqwest::Response, Error> {
    // add the image to the form
    let form = form.part(
        "image",
        Part::bytes(image.to_vec())
            .file_name("logo.png")
            .mime_str("image/png")?,
    );
    // send the form as this user
    let resp = reqwest::Client::new()
        .request(method, url)
        .header("authorization", user.auth_header())
        .multipart(form)
        .send()
        .await?;
    // make sure the request worked and report why if it didn't
    if !resp.status().is_success() {
        return Err(Error::new(format!(
            "{} failed with {}: {}",
            url,
            resp.status(),
            resp.text().await?
        )));
    }
    Ok(resp)
}

/// Replacing an image with one of the same name keeps the new image, and deletes still work
#[tokio::test]
async fn same_name_image_replacement() -> Result<(), Error> {
    // get an admin client and an editor of a group
    let admin = test_utilities::admin_client().await?;
    let user = plain_user(&admin).await?;
    let group = generators::group_with_roles(
        &GroupRoles::default().user(&user.username),
        GroupAllowed::default(),
        &admin,
    )
    .await?;
    // create an entity with an image
    let req = EntityRequest::new(
        generators::gen_string(16),
        generators::gen_other_meta(),
        vec![group.clone()],
    );
    let url = format!("{}/api/entities/", admin.host);
    let resp =
        send_with_image(&user, reqwest::Method::POST, &url, req.to_form()?, b"first").await?;
    let created: serde_json::Value = resp.json().await?;
    let id: Uuid = serde_json::from_value(created["id"].clone())?;
    // replace the image with a new one of the same name
    let url = format!("{}/api/entities/{id}", admin.host);
    let form = EntityUpdate::default().to_form()?;
    send_with_image(&user, reqwest::Method::PATCH, &url, form, b"second").await?;
    // the new image is kept
    let url = format!("{}/api/entities/{id}/image", admin.host);
    let image = reqwest::Client::new()
        .get(&url)
        .header("authorization", user.auth_header())
        .send()
        .await?;
    is!(image.status().is_success(), true, image.status());
    let bytes = image.bytes().await?.to_vec();
    is!(bytes, b"second".to_vec());
    // deleting the entity removes it
    user.client.entities.delete(id).await?;
    let resp = user.client.entities.get(id).await;
    fail!(resp, 404);
    Ok(())
}

/// `GroupAllowedUpdate.associations` changes only a group's Associations flag
#[tokio::test]
async fn group_associations_flag_update() -> Result<(), Error> {
    // get an admin client and a group that allows everything
    let admin = test_utilities::admin_client().await?;
    let group =
        generators::group_with_roles(&GroupRoles::default(), GroupAllowed::default(), &admin)
            .await?;
    // disallow associations in this group
    let update = GroupUpdate {
        allowed: GroupAllowedUpdate::default().disable_associations(),
        ..GroupUpdate::default()
    };
    admin.groups.update(&group, &update).await?;
    // only the associations flag changed
    let stored = admin.groups.get(&group).await?;
    is!(stored.allowed.associations, false);
    is!(
        stored.allowed.entities && stored.allowed.files && stored.allowed.tags,
        true
    );
    // allow associations again
    let update = GroupUpdate {
        allowed: GroupAllowedUpdate::default().enable_associations(),
        ..GroupUpdate::default()
    };
    admin.groups.update(&group, &update).await?;
    let stored = admin.groups.get(&group).await?;
    is!(stored.allowed.associations, true);
    Ok(())
}
