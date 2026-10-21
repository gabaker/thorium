//! Small seeding helpers that write data through the API

use thorium::Thorium;
use thorium::client::ResultsClient;
use thorium::models::{
    Buffer, EntityKinds, EntityRequest, EntityResponse, GroupRequest, OutputDisplayType,
    OutputRequest, SampleRequest,
};
use thorium::test_utilities::generators;
use uuid::Uuid;

/// Creates a group owned by the calling admin
///
/// # Arguments
///
/// * `client` - An admin client
/// * `name` - The group's name
pub async fn group(client: &Thorium, name: &str) {
    // create the group with default membership
    client
        .groups
        .create(&GroupRequest::new(name))
        .await
        .unwrap_or_else(|err| panic!("failed to create group {name}: {err}"));
}

/// Uploads a sample into a group and returns its sha256
///
/// # Arguments
///
/// * `client` - A client allowed to upload into the group
/// * `group` - The group to upload into
/// * `name` - The file name to give the sample
/// * `data` - The sample's bytes
pub async fn sample(client: &Thorium, group: &str, name: &str, data: &[u8]) -> String {
    // upload the bytes as a named buffer
    let request = SampleRequest::new_buffer(Buffer::new(data.to_vec()).name(name), vec![group]);
    client
        .files
        .create(request)
        .await
        .unwrap_or_else(|err| panic!("failed to upload sample {name}: {err}"))
        .sha256
}

/// Uploads a profile icon for the harness admin, which the API stores in the graphics bucket
///
/// # Arguments
///
/// * `api_url` - The API's URL
/// * `token` - The admin's token
///
/// # Panics
///
/// Panics if the API refuses the upload.
pub async fn icon(api_url: &str, token: &str) {
    // the API authenticates a base64 token and takes the image as a multipart field
    let auth = auth_header(token);
    let part = reqwest::multipart::Part::bytes(b"\x89PNG\r\n\x1a\nfixture icon".to_vec())
        .file_name("icon.png")
        .mime_str("image/png")
        .expect("a valid mime type");
    let form = reqwest::multipart::Form::new().part("image", part);
    let response = reqwest::Client::new()
        .post(format!("{api_url}/api/users/image"))
        .header("authorization", auth)
        .multipart(form)
        .send()
        .await
        .expect("failed to upload an icon");
    assert!(
        response.status().is_success(),
        "the icon upload returned {}",
        response.status()
    );
}

/// Builds the authorization header the API takes for a token
///
/// # Arguments
///
/// * `token` - The token
fn auth_header(token: &str) -> String {
    // the API reads a base64-encoded token
    format!("token {}", data_encoding::BASE64.encode(token.as_bytes()))
}

/// Creates a device entity with an image, which the API stores in the graphics bucket, and
/// returns its id
///
/// # Arguments
///
/// * `api_url` - The API's URL
/// * `token` - The admin's token
/// * `group` - The group to create the entity in
/// * `name` - The entity's name
/// * `image` - The image's bytes
///
/// # Panics
///
/// Panics if the API refuses the entity.
pub async fn entity_image(
    api_url: &str,
    token: &str,
    group: &str,
    name: &str,
    image: &[u8],
) -> Uuid {
    // the client has no image field, so add the image to the entity's form by hand
    let request = EntityRequest::new(name, generators::gen_device_meta(Vec::new()), [group]);
    let part = reqwest::multipart::Part::bytes(image.to_vec())
        .file_name("device.png")
        .mime_str("image/png")
        .expect("a valid mime type");
    let form = request
        .to_form()
        .expect("the entity's form builds")
        .part("image", part);
    let response = reqwest::Client::new()
        .post(format!("{api_url}/api/entities/"))
        .header("authorization", auth_header(token))
        .multipart(form)
        .send()
        .await
        .expect("failed to create an entity");
    assert!(
        response.status().is_success(),
        "the entity create returned {}",
        response.status()
    );
    response
        .json::<EntityResponse>()
        .await
        .expect("the entity response decodes")
        .id
}

/// Adds a result to a sample that found entities of some kinds, so the API writes an entity blob
/// per kind into the results bucket, and returns the result's id
///
/// # Arguments
///
/// * `client` - A client allowed to add results to the sample
/// * `sample` - The sample's sha256
/// * `kinds` - The kinds of entity found, each with its count and blob
///
/// # Panics
///
/// Panics if the API refuses the result.
pub async fn result_with_entities(
    client: &Thorium,
    sample: &str,
    kinds: &[(EntityKinds, usize, &[u8])],
) -> Uuid {
    // one blob per kind beside the result
    let mut request = OutputRequest::new(
        sample.to_owned(),
        "entity-finder",
        "found entities",
        OutputDisplayType::String,
    );
    for (kind, count, blob) in kinds {
        request = request.entities(*kind, *count, Buffer::new(blob.to_vec()));
    }
    client
        .files
        .create_result(request)
        .await
        .unwrap_or_else(|err| panic!("failed to add a result with entities: {err}"))
        .id
}
