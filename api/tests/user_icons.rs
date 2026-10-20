//! Tests that user icons stay within their Thorium namespace when namespaces share a graphics
//! bucket

use std::process::{Child, Command};

use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{
    BucketLifecycleConfiguration, ExpirationStatus, LifecycleExpiration, LifecycleRule,
    LifecycleRuleFilter,
};
use base64::Engine;
use thorium::Error;
use thorium::client::{ClientSettings, Users};
use thorium::models::{AuthResponse, UserCreate};
use thorium::test_utilities::{self, CONF};

/// The environment variable that marks a child process serving one namespace's API
const CHILD_MARKER: &str = "THORIUM_USER_ICONS_CHILD";

/// The longest a child API keeps serving before it exits on its own, in seconds
const CHILD_LIFETIME: u64 = 600;

/// Serve the test API for the namespace and port in this process's environment until killed
///
/// This only runs as a child of [`icons_stay_in_their_namespace`], which starts one per namespace.
#[tokio::test]
#[ignore = "run by icons_stay_in_their_namespace once per namespace"]
async fn icon_api_child() -> Result<(), Error> {
    // only serve when started by the parent test
    if std::env::var(CHILD_MARKER).is_err() {
        return Ok(());
    }
    // wipe this namespace, start its API, and bootstrap its admin
    test_utilities::admin_client().await?;
    // keep serving until the parent kills us, or give up after our lifetime
    tokio::time::sleep(std::time::Duration::from_secs(CHILD_LIFETIME)).await;
    Ok(())
}

/// The child API processes a test started, killed when the test ends however it ends
struct ChildApis(Vec<Child>);

impl Drop for ChildApis {
    /// Kill and reap every child API
    fn drop(&mut self) {
        // stop each child and wait for it so none is left running
        for child in &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// The name of the graphics bucket both namespaces share
fn graphics_bucket() -> String {
    // test buckets are named after the run's namespace
    format!("{}-icons", CONF.thorium.namespace.replace('_', "-"))
}

/// Find a port no one is listening on, so a child API doesn't collide with other test runs
fn free_port() -> Result<u16, Error> {
    // let the OS pick a free port, then release it for the child to bind
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

/// Start an API for a namespace beside the run's own in a child process
///
/// Returns the child and the address of its API.
///
/// # Arguments
///
/// * `suffix` - The suffix that makes this namespace from the run's namespace
/// * `port` - The port this API listens on
fn start_api(suffix: &str, port: u16) -> Result<(Child, String), Error> {
    // run the child test with this namespace, port, and the shared graphics bucket
    let child = Command::new(std::env::current_exe()?)
        .args(["--exact", "icon_api_child", "--ignored"])
        .env(CHILD_MARKER, "1")
        .env(
            "THORIUM__THORIUM__NAMESPACE",
            format!("{}_{suffix}", CONF.thorium.namespace),
        )
        .env("THORIUM__THORIUM__PORT", port.to_string())
        .env("THORIUM__THORIUM__GRAPHICS__BUCKET", graphics_bucket())
        .spawn()?;
    Ok((child, format!("http://127.0.0.1:{port}")))
}

/// Create a user in an API, retrying until the API is up, and return their token
///
/// # Arguments
///
/// * `host` - The API to create the user in
/// * `name` - The name of the user
async fn create_user(host: &str, name: &str) -> Result<String, Error> {
    // keep trying while the child API starts
    let mut attempts = 0;
    loop {
        // create a user that needs no email verification
        let create = UserCreate::new(name, "CornSilkPw7Q9z", "io@fake.gov").skip_verification();
        let attempt = Users::create(
            host,
            create,
            Some(&CONF.thorium.secret_key),
            &ClientSettings::default(),
        )
        .await;
        match attempt {
            Ok(AuthResponse::Authed { token, .. }) => return Ok(token),
            Ok(AuthResponse::VerifyEmail(_)) => {
                return Err(Error::new(format!("{name} needs to verify an email")));
            }
            // give up once the API has had five minutes to start
            Err(error) if attempts == 300 => return Err(error),
            Err(_) => {
                attempts += 1;
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
    }
}

/// Build the authorization header for a token
///
/// # Arguments
///
/// * `token` - The token to authorize with
fn auth(token: &str) -> String {
    // tokens are sent base64 encoded
    let encoded = base64::engine::general_purpose::STANDARD.encode(token.as_bytes());
    format!("token {encoded}")
}

/// Upload a PNG icon for the user a token belongs to
///
/// # Arguments
///
/// * `host` - The API to upload to
/// * `token` - The user's token
/// * `data` - The icon's bytes
async fn upload_icon(host: &str, token: &str, data: &'static [u8]) -> Result<(), Error> {
    // send the icon as the image field of a form
    let part = reqwest::multipart::Part::bytes(data)
        .file_name("icon.png")
        .mime_str("image/png")?;
    let form = reqwest::multipart::Form::new().part("image", part);
    let resp = reqwest::Client::new()
        .post(format!("{host}/api/users/image"))
        .header("authorization", auth(token))
        .multipart(form)
        .send()
        .await?;
    assert!(
        resp.status().is_success(),
        "upload returned {}",
        resp.status()
    );
    Ok(())
}

/// Get a user's icon, returning the status and body
///
/// # Arguments
///
/// * `host` - The API to get the icon from
/// * `token` - A token to authorize with
/// * `name` - The user whose icon to get
async fn get_icon(host: &str, token: &str, name: &str) -> Result<(u16, Vec<u8>), Error> {
    // request the user's icon
    let resp = reqwest::Client::new()
        .get(format!("{host}/api/users/user/{name}/image"))
        .header("authorization", auth(token))
        .send()
        .await?;
    let status = resp.status().as_u16();
    Ok((status, resp.bytes().await?.to_vec()))
}

/// Delete the icon of the user a token belongs to
///
/// # Arguments
///
/// * `host` - The API to delete the icon in
/// * `token` - The user's token
async fn delete_icon(host: &str, token: &str) -> Result<(), Error> {
    // delete this user's icon
    let resp = reqwest::Client::new()
        .delete(format!("{host}/api/users/image"))
        .header("authorization", auth(token))
        .send()
        .await?;
    assert!(
        resp.status().is_success(),
        "delete returned {}",
        resp.status()
    );
    Ok(())
}

/// Check whether an object exists in the shared graphics bucket
///
/// # Arguments
///
/// * `s3` - The S3 client to check with
/// * `key` - The key of the object
async fn object_exists(s3: &aws_sdk_s3::Client, key: &str) -> bool {
    // a failed head means the object is gone
    s3.head_object()
        .bucket(graphics_bucket())
        .key(key)
        .send()
        .await
        .is_ok()
}

/// Two namespaces sharing a graphics bucket each keep their own icon for a user with the same
/// name, and an icon saved under the key used before icons were namespaced is still served and
/// deleted
#[tokio::test]
async fn icons_stay_in_their_namespace() -> Result<(), Error> {
    // create the graphics bucket both namespaces share, expiring its objects after a day
    let s3 = test_utilities::s3_client(&CONF.thorium.s3);
    let rule = LifecycleRule::builder()
        .id("test")
        .filter(LifecycleRuleFilter::builder().prefix("").build())
        .expiration(LifecycleExpiration::builder().days(1).build())
        .status(ExpirationStatus::Enabled)
        .build()
        .unwrap();
    let lifecycle = BucketLifecycleConfiguration::builder()
        .rules(rule)
        .build()
        .unwrap();
    test_utilities::init_bucket(&s3, lifecycle, &graphics_bucket()).await?;
    // start an API for each of two namespaces on free ports
    let (first, host_a) = start_api("ica", free_port()?)?;
    let mut children = ChildApis(vec![first]);
    let (second, host_b) = start_api("icb", free_port()?)?;
    children.0.push(second);
    // create a user with the same name in each namespace
    let alice = format!("alice-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
    let token_a = create_user(&host_a, &alice).await?;
    let token_b = create_user(&host_b, &alice).await?;
    // upload a different icon in each namespace
    upload_icon(&host_a, &token_a, b"ICON-FROM-NAMESPACE-A").await?;
    upload_icon(&host_b, &token_b, b"ICON-FROM-NAMESPACE-B").await?;
    // each namespace serves its own icon, stored under a key naming the namespace
    let (status, body) = get_icon(&host_a, &token_a, &alice).await?;
    assert_eq!(
        (status, body.as_slice()),
        (200, &b"ICON-FROM-NAMESPACE-A"[..])
    );
    let (status, body) = get_icon(&host_b, &token_b, &alice).await?;
    assert_eq!(
        (status, body.as_slice()),
        (200, &b"ICON-FROM-NAMESPACE-B"[..])
    );
    let key_a = format!("users/{}_ica/{alice}/icon.png", CONF.thorium.namespace);
    assert!(object_exists(&s3, &key_a).await, "no icon at {key_a}");
    // deleting the icon in one namespace leaves the other's
    delete_icon(&host_b, &token_b).await?;
    let (status, _) = get_icon(&host_b, &token_b, &alice).await?;
    assert_eq!(status, 404, "the deleted icon is still served");
    let (status, body) = get_icon(&host_a, &token_a, &alice).await?;
    assert_eq!(
        (status, body.as_slice()),
        (200, &b"ICON-FROM-NAMESPACE-A"[..])
    );
    // store an icon for a second user under the key used before icons were namespaced
    let europa = format!("europa-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
    let token_europa = create_user(&host_a, &europa).await?;
    let legacy = format!("users/{europa}/icon.png");
    s3.put_object()
        .bucket(graphics_bucket())
        .key(&legacy)
        .content_type("image/png")
        .body(ByteStream::from_static(b"LEGACY-ICON"))
        .send()
        .await
        .map_err(|err| Error::new(format!("{err:?}")))?;
    let redis = test_utilities::redis_pool().await;
    let mut conn = redis
        .get()
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    let (): () = bb8_redis::redis::cmd("HSET")
        .arg(format!("{}_ica:user_data:{europa}", CONF.thorium.namespace))
        .arg("image")
        .arg(&legacy)
        .query_async(&mut *conn)
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    drop(conn);
    // the legacy icon is still served, and deleting it removes its key
    let (status, body) = get_icon(&host_a, &token_a, &europa).await?;
    assert_eq!((status, body.as_slice()), (200, &b"LEGACY-ICON"[..]));
    delete_icon(&host_a, &token_europa).await?;
    assert!(
        !object_exists(&s3, &legacy).await,
        "the legacy icon was kept"
    );
    // stop both APIs
    drop(children);
    Ok(())
}
