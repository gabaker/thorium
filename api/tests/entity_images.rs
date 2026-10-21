//! Tests that an entity image's client file name can't put the image outside its entity's prefix

use thorium::client::{ClientSettings, Users};
use thorium::models::{
    AuthResponse, EntityMetadataRequest, EntityRequest, EntityResponse, GroupUpdate,
    GroupUsersUpdate, UserCreate,
};
use thorium::test_utilities::{self, CONF, generators};
use thorium::{Error, Thorium};
use uuid::Uuid;

/// The bytes of the image the victim entity owns
const VICTIM_IMAGE: &[u8] = b"VICTIM-IMAGE";

/// The bytes an attacker tries to write over the victim's image
const ATTACKER_IMAGE: &[u8] = b"ATTACKER-IMAGE";

/// Create a user who is in no groups and return their name and token
///
/// # Arguments
///
/// * `host` - The API to create the user in
async fn create_user(host: &str) -> Result<(String, String), Error> {
    // create a user that needs no email verification
    let name = generators::gen_string(24);
    let create =
        UserCreate::new(&name, generators::gen_string(32), "fake@fake.gov").skip_verification();
    let auth = Users::create(
        host,
        create,
        Some(&CONF.thorium.secret_key),
        &ClientSettings::default(),
    )
    .await?;
    match auth {
        AuthResponse::Authed { token, .. } => Ok((name, token)),
        AuthResponse::VerifyEmail(_) => Err(Error::new(format!("{name} needs to verify an email"))),
    }
}

/// Build the PNG image part of an entity form with a raw file name
///
/// # Arguments
///
/// * `file_name` - The file name to send exactly as given
/// * `data` - The image's bytes
fn image_part(file_name: &str, data: &'static [u8]) -> Result<reqwest::multipart::Part, Error> {
    // send the image as a PNG with the file name unchanged
    let part = reqwest::multipart::Part::bytes(data)
        .file_name(file_name.to_owned())
        .mime_str("image/png")?;
    Ok(part)
}

/// Build a request for an `Other` entity in a group
///
/// # Arguments
///
/// * `group` - The group to create the entity in
fn other(group: &str) -> EntityRequest {
    // other entities need nothing but a name and groups
    EntityRequest::new(
        generators::gen_string(16),
        EntityMetadataRequest::Other,
        [group],
    )
}

/// Create an entity with an image and return the status and body of the response
///
/// # Arguments
///
/// * `host` - The API to create the entity in
/// * `token` - The token of the user creating the entity
/// * `req` - The entity to create, without an image
/// * `file_name` - The image's file name, sent exactly as given
/// * `data` - The image's bytes
async fn create_entity(
    host: &str,
    token: &str,
    req: EntityRequest,
    file_name: &str,
    data: &'static [u8],
) -> Result<(u16, String), Error> {
    // add the image to the entity's form, whose file names reach the API as given
    let form = req.to_form()?.part("image", image_part(file_name, data)?);
    // send the create request
    let resp = reqwest::Client::new()
        .post(format!("{host}/api/entities/"))
        .header("authorization", test_utilities::auth_header(token))
        .multipart(form)
        .send()
        .await?;
    let status = resp.status().as_u16();
    Ok((status, resp.text().await?))
}

/// Replace an entity's image and return the status of the response
///
/// # Arguments
///
/// * `host` - The API the entity is in
/// * `token` - The token of the user updating the entity
/// * `id` - The id of the entity to update
/// * `file_name` - The image's file name, sent exactly as given
/// * `data` - The image's bytes
async fn update_image(
    host: &str,
    token: &str,
    id: Uuid,
    file_name: &str,
    data: &'static [u8],
) -> Result<u16, Error> {
    // build a form whose file names aren't percent encoded, so they reach the API as given
    let form = reqwest::multipart::Form::new()
        .percent_encode_noop()
        .text("clear_description", "false")
        .part("image", image_part(file_name, data)?);
    // send the update request
    let resp = reqwest::Client::new()
        .patch(format!("{host}/api/entities/{id}"))
        .header("authorization", test_utilities::auth_header(token))
        .multipart(form)
        .send()
        .await?;
    Ok(resp.status().as_u16())
}

/// Download an entity's image, returning the status and body
///
/// # Arguments
///
/// * `host` - The API the entity is in
/// * `token` - A token that can see the entity
/// * `id` - The id of the entity
async fn get_image(host: &str, token: &str, id: Uuid) -> Result<(u16, Vec<u8>), Error> {
    // request the entity's image
    let resp = reqwest::Client::new()
        .get(format!("{host}/api/entities/{id}/image"))
        .header("authorization", test_utilities::auth_header(token))
        .send()
        .await?;
    let status = resp.status().as_u16();
    Ok((status, resp.bytes().await?.to_vec()))
}

/// Check whether an object exists in the graphics bucket
///
/// # Arguments
///
/// * `key` - The key of the object
async fn object_exists(key: &str) -> bool {
    // a failed head means there is no object at this key
    test_utilities::s3_client(&CONF.thorium.s3)
        .head_object()
        .bucket(&CONF.thorium.graphics.bucket)
        .key(key)
        .send()
        .await
        .is_ok()
}

/// Add a user to a group as a plain user
///
/// # Arguments
///
/// * `admin` - An admin client
/// * `group` - The group to add the user to
/// * `user` - The name of the user to add
async fn add_to_group(admin: &Thorium, group: &str, user: &str) -> Result<(), Error> {
    // add the user directly to the group's users
    let users = GroupUsersUpdate::default().direct_add(user);
    admin
        .groups
        .update(group, &GroupUpdate::default().users(users))
        .await?;
    Ok(())
}

/// Get the id from an entity create response body
///
/// # Arguments
///
/// * `body` - The body of the create response
fn entity_id(body: &str) -> Result<Uuid, Error> {
    // the body is an entity response with the new entity's id
    let resp: EntityResponse = serde_json::from_str(body)?;
    Ok(resp.id)
}

/// The parts of a test shared by every attack: the API, a victim entity, and an attacker
struct Setup {
    /// The address of the API
    host: String,
    /// The token of the victim, who can see the victim entity
    victim_token: String,
    /// The id of the victim entity, whose image is `logo.png`
    victim: Uuid,
    /// The group the victim entity is in
    victim_group: String,
    /// The attacker's token
    attacker_token: String,
}

/// Create a victim entity with an image in one group and an attacker who is not in that group
///
/// # Arguments
///
/// * `attacker_group` - Whether the attacker gets a group of their own to edit entities in
async fn setup(attacker_group: bool) -> Result<(Setup, Option<String>), Error> {
    // get an admin client to set up groups with
    let admin = test_utilities::admin_client().await?;
    let host = admin.host.clone();
    // create the victim's group and a victim user in it
    let victim_group = generators::groups(1, &admin).await?.remove(0).name;
    let (victim_user, victim_token) = create_user(&host).await?;
    add_to_group(&admin, &victim_group, &victim_user).await?;
    // the victim creates an entity in their group whose image is logo.png
    let (status, body) = create_entity(
        &host,
        &victim_token,
        other(&victim_group),
        "logo.png",
        VICTIM_IMAGE,
    )
    .await?;
    assert_eq!(status, 200, "victim create failed: {body}");
    let victim = entity_id(&body)?;
    // create an attacker who isn't in the victim's group
    let (attacker, attacker_token) = create_user(&host).await?;
    // give the attacker a group of their own if asked
    let own_group = if attacker_group {
        let group = generators::groups(1, &admin).await?.remove(0).name;
        add_to_group(&admin, &group, &attacker).await?;
        Some(group)
    } else {
        None
    };
    let setup = Setup {
        host,
        victim_token,
        victim,
        victim_group,
        attacker_token,
    };
    Ok((setup, own_group))
}

/// Make sure the victim entity still serves its own image
///
/// # Arguments
///
/// * `setup` - The test's victim and API
async fn assert_victim_intact(setup: &Setup) -> Result<(), Error> {
    // download the victim's image as the victim
    let (status, body) = get_image(&setup.host, &setup.victim_token, setup.victim).await?;
    assert_eq!(
        (status, String::from_utf8_lossy(&body).into_owned()),
        (200, String::from_utf8_lossy(VICTIM_IMAGE).into_owned()),
        "the victim's image was changed"
    );
    Ok(())
}

/// A user in another group can't create an entity whose image overwrites another entity's image
#[tokio::test]
async fn absolute_name_on_create_cannot_overwrite_another_groups_image() -> Result<(), Error> {
    // create a victim and an attacker with a group of their own
    let (setup, own_group) = setup(true).await?;
    let own_group = own_group.expect("the attacker has a group");
    // create an entity in the attacker's group whose image name points at the victim's image
    let name = format!("/{}/logo.png", setup.victim);
    let (status, body) = create_entity(
        &setup.host,
        &setup.attacker_token,
        other(&own_group),
        &name,
        ATTACKER_IMAGE,
    )
    .await?;
    // the create is refused and the victim keeps their image
    assert_eq!(
        status, 400,
        "create with {name:?} returned {status}: {body}"
    );
    assert_victim_intact(&setup).await
}

/// A user can't point their own entity's image at another group's entity image by updating it
#[tokio::test]
async fn absolute_name_on_update_cannot_overwrite_another_groups_image() -> Result<(), Error> {
    // create a victim and an attacker with a group of their own
    let (setup, own_group) = setup(true).await?;
    let own_group = own_group.expect("the attacker has a group");
    // create an ordinary entity in the attacker's group
    let (status, body) = create_entity(
        &setup.host,
        &setup.attacker_token,
        other(&own_group),
        "mine.png",
        ATTACKER_IMAGE,
    )
    .await?;
    assert_eq!(status, 200, "attacker create failed: {body}");
    let own = entity_id(&body)?;
    // replace its image with one whose name points at the victim's image
    let name = format!("/{}/logo.png", setup.victim);
    let status = update_image(
        &setup.host,
        &setup.attacker_token,
        own,
        &name,
        ATTACKER_IMAGE,
    )
    .await?;
    // the update is refused, the victim keeps their image, and the attacker keeps theirs
    assert_eq!(status, 400, "update with {name:?} returned {status}");
    assert_victim_intact(&setup).await?;
    let (status, body) = get_image(&setup.host, &setup.attacker_token, own).await?;
    assert_eq!((status, body.as_slice()), (200, ATTACKER_IMAGE));
    Ok(())
}

/// A user in no groups can't overwrite or delete an image with a create that is refused
#[tokio::test]
async fn refused_create_cannot_overwrite_an_image() -> Result<(), Error> {
    // create a victim and an attacker in no groups
    let (setup, _) = setup(false).await?;
    // try to create an entity in the victim's group whose image name points at the victim's image
    let name = format!("/{}/logo.png", setup.victim);
    let (status, body) = create_entity(
        &setup.host,
        &setup.attacker_token,
        other(&setup.victim_group),
        &name,
        ATTACKER_IMAGE,
    )
    .await?;
    // the create is refused and the victim keeps their image
    assert!(
        (400..500).contains(&status),
        "create with {name:?} returned {status}: {body}"
    );
    assert_victim_intact(&setup).await
}

/// Image names with separators, traversal, or no name are refused and write nothing, and plain
/// names keep working
#[tokio::test]
async fn names_that_leave_the_entity_prefix_are_refused() -> Result<(), Error> {
    // create a victim and an attacker with a group of their own
    let (setup, own_group) = setup(true).await?;
    let own_group = own_group.expect("the attacker has a group");
    // create an ordinary entity with a plain image name
    let (status, body) = create_entity(
        &setup.host,
        &setup.attacker_token,
        other(&own_group),
        "my logo.png",
        ATTACKER_IMAGE,
    )
    .await?;
    assert_eq!(status, 200, "create with a plain name failed: {body}");
    let own = entity_id(&body)?;
    assert!(object_exists(&format!("{own}/my logo.png")).await);
    // every unsafe name is refused on create and on update
    let marker = Uuid::new_v4().simple().to_string();
    let names = [
        format!("/{marker}.png"),
        format!("../{marker}.png"),
        format!("x//{marker}.png"),
        format!("a/{marker}.png"),
        String::new(),
    ];
    for name in &names {
        let (status, body) = create_entity(
            &setup.host,
            &setup.attacker_token,
            other(&own_group),
            name,
            ATTACKER_IMAGE,
        )
        .await?;
        assert_eq!(
            status, 400,
            "create with {name:?} returned {status}: {body}"
        );
        let status = update_image(
            &setup.host,
            &setup.attacker_token,
            own,
            name,
            ATTACKER_IMAGE,
        )
        .await?;
        assert_eq!(status, 400, "update with {name:?} returned {status}");
    }
    // nothing was written at a key those names would have escaped to
    for key in [
        format!("/{marker}.png"),
        format!("{marker}.png"),
        format!("{own}/x//{marker}.png"),
        format!("{own}/x/{marker}.png"),
        format!("{own}/a/{marker}.png"),
        format!("{own}.png"),
    ] {
        assert!(
            !object_exists(&key).await,
            "an object was written at {key:?}"
        );
    }
    // the entity keeps its image through the refused updates
    let (status, body) = get_image(&setup.host, &setup.attacker_token, own).await?;
    assert_eq!((status, body.as_slice()), (200, ATTACKER_IMAGE));
    // a plain name without an extension still replaces the image
    let status = update_image(
        &setup.host,
        &setup.attacker_token,
        own,
        "logo",
        VICTIM_IMAGE,
    )
    .await?;
    assert_eq!(status, 204, "update with a plain name returned {status}");
    let (status, body) = get_image(&setup.host, &setup.attacker_token, own).await?;
    assert_eq!((status, body.as_slice()), (200, VICTIM_IMAGE));
    assert!(object_exists(&format!("{own}/logo.png")).await);
    assert_victim_intact(&setup).await
}
