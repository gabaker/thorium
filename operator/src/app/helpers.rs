use aws_credential_types::provider::SharedCredentialsProvider;
use aws_sdk_s3::{
    Client,
    config::Credentials,
    types::{BucketLocationConstraint, CreateBucketConfiguration},
};
use thorium::{Error, conf::S3};

use aws_sdk_s3::error::{DisplayErrorContext, SdkError};
use aws_sdk_s3::operation::create_bucket::CreateBucketError;

use crate::k8s::clusters::ClusterMeta;

/// Build an API url string
///
/// Get the thorium host from operator args or the target ThoriumCluster instance being configured.
/// The url string will be none outside of a development environment when running in kubernetes
/// within a pod.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `url` - The Thorium API URL passed to the operator as an argument
pub fn get_thorium_host(meta: &ClusterMeta, url: Option<&String>) -> String {
    match url {
        // grab url if passed to the operator as an arg, mostly for development
        Some(url) => url.to_owned(),
        // use internal k8s networking by default
        None => {
            format!(
                "http://thorium-api.{}.svc.cluster.local:80",
                &meta.namespace
            )
        }
    }
}

/// Get the location constraint to create a bucket in a region with
///
/// AWS rejects a `us-east-1` constraint since that is where buckets go without one, so no
/// constraint is sent for it or when no region is set.
///
/// # Arguments
///
/// * `region` - The S3 region from the config, if one is set
fn location_constraint(region: Option<&str>) -> Option<BucketLocationConstraint> {
    // only send a constraint for a set region other than us-east-1
    region
        .filter(|region| !region.is_empty() && *region != "us-east-1")
        .map(BucketLocationConstraint::from)
}

/// Find where the scheme of an S3 endpoint ends, if it has one
///
/// Returns the index of the `://` after the scheme. Text before a `://` only counts as a scheme
/// when it is made of the characters a scheme may hold, so a `://` inside a password doesn't
/// count.
///
/// # Arguments
///
/// * `endpoint` - The S3 endpoint to find the scheme of
fn scheme_end(endpoint: &str) -> Option<usize> {
    endpoint.find("://").filter(|end| {
        !endpoint[..*end].is_empty()
            && endpoint[..*end]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    })
}

/// Find the user info in an S3 endpoint
///
/// Returns the range of everything after the scheme up to the last `@`, not including the `@`.
/// An S3 endpoint has no query to hold an `@`, so taking up to the last one drops a password
/// holding `@`, `/`, `?`, or `#` whole.
///
/// # Arguments
///
/// * `endpoint` - The S3 endpoint to find the user info of
fn endpoint_user_info(endpoint: &str) -> Option<std::ops::Range<usize>> {
    // the user info starts after the scheme if there is one
    let start = scheme_end(endpoint).map_or(0, |end| end + 3);
    // and runs up to the last '@'
    endpoint[start..].rfind('@').map(|at| start..start + at)
}

/// Name an S3 endpoint without any user info in its URL
///
/// The user info is stripped as text first, so a password holding `/`, `?`, or `#` that a URL
/// parser would read as a port or path is dropped too. An endpoint without a scheme is read as an
/// `https` URL and named without the scheme. An endpoint that doesn't parse to a URL with a host
/// is hidden.
///
/// # Arguments
///
/// * `endpoint` - The S3 endpoint to name
fn redact_endpoint(endpoint: &str) -> String {
    // strip the user info and its '@' as text before parsing
    let stripped = match endpoint_user_info(endpoint) {
        Some(range) => format!("{}{}", &endpoint[..range.start], &endpoint[range.end + 1..]),
        None => endpoint.to_owned(),
    };
    // read an endpoint without a scheme as an https URL
    let schemeless = scheme_end(&stripped).is_none();
    let parsed = if schemeless {
        reqwest::Url::parse(&format!("https://{stripped}"))
    } else {
        reqwest::Url::parse(&stripped)
    };
    match parsed {
        Ok(mut url) if url.has_host() => {
            // clear any user info the parser still found
            let _ = url.set_username("");
            let _ = url.set_password(None);
            // serialize the URL without the root path the parser adds to a bare host
            let mut redacted = url.to_string();
            if redacted.ends_with('/') && !stripped.ends_with('/') {
                redacted.pop();
            }
            // drop the scheme added to an endpoint that had none
            match redacted.strip_prefix("https://") {
                Some(rest) if schemeless => rest.to_owned(),
                _ => redacted,
            }
        }
        // an endpoint that isn't a URL with a host is hidden in case it holds credentials
        _ => "<unparseable endpoint>".to_owned(),
    }
}

/// Remove an S3 endpoint's user info from a message, such as an SDK error that repeats the
/// endpoint
///
/// The endpoint as configured is replaced by its redacted name, and its user info is dropped
/// wherever else it shows up before an `@`, as written or escaped in a debug message.
///
/// # Arguments
///
/// * `text` - The message to redact
/// * `endpoint` - The S3 endpoint as configured
fn redact_endpoint_in(text: &str, endpoint: &str) -> String {
    // an empty endpoint can't show up in a message
    if endpoint.is_empty() {
        return text.to_owned();
    }
    // name the endpoint, as written or escaped, by its redacted form
    let named = redact_endpoint(endpoint);
    let mut redacted = text.replace(endpoint, &named);
    redacted = redacted.replace(&endpoint.escape_debug().to_string(), &named);
    // drop the user info wherever else it comes before an '@'
    if let Some(range) = endpoint_user_info(endpoint).filter(|range| !range.is_empty()) {
        let user_info = &endpoint[range];
        for form in [user_info.to_owned(), user_info.escape_debug().to_string()] {
            redacted = redacted.replace(&format!("{form}@"), "");
        }
    }
    redacted
}

/// Describe a bucket that S3 refused to create
///
/// # Arguments
///
/// * `config` - The Thorium S3 configuration
/// * `bucket_name` - The bucket that couldn't be created
/// * `cause` - The error S3 returned, which may repeat the endpoint as configured
fn create_bucket_error(config: &S3, bucket_name: &str, cause: &str) -> Error {
    // name the endpoint without any credentials in its URL
    let endpoint = redact_endpoint(&config.endpoint);
    // drop the endpoint's credentials from the cause too, since the SDK repeats an endpoint it
    // can't parse in its error
    let cause = redact_endpoint_in(cause, &config.endpoint);
    // show the region setting as configured, telling an empty one apart from an unset one
    let region = config
        .region
        .as_deref()
        .map_or_else(|| "unset".to_owned(), |region| format!("{region:?}"));
    // name the bucket, where it was created, and how to work around a gateway that refuses it
    Error::new(format!(
        "Failed to create bucket {bucket_name} at {endpoint} (thorium.s3.region: {region}): \
         {cause}. Pre-create the buckets and set thorium.s3.skip_bucket_auto_create to work \
         around this"
    ))
}

/// Create an S3 bucket
///
/// A `CreateBucketConfiguration` is only sent when the region needs a location constraint.
///
/// # Arguments
///
/// * `config` - The Thorium S3 configuration
/// * `client` - API client for S3 interface
/// * `bucket_name` - Name of bucket to create
pub async fn create_bucket(config: &S3, client: &Client, bucket_name: &str) -> Result<(), Error> {
    // if bucket creation is disabled then log the bucket we skipped creating
    if config.skip_bucket_auto_create {
        // log the bucket we aren't creating
        println!("Skipping bucket creation: {bucket_name}");
        // just return instead of creating buckets
        return Ok(());
    }
    // start building the request to create the bucket
    let mut request = client.create_bucket().bucket(bucket_name);
    // pin the bucket to our region only when it needs a constraint, since some S3 gateways
    // such as Ceph RGW reject an empty CreateBucketConfiguration
    if let Some(constraint) = location_constraint(config.region.as_deref()) {
        // build the configuration holding our location constraint
        let bucket_config = CreateBucketConfiguration::builder()
            .location_constraint(constraint)
            .build();
        request = request.create_bucket_configuration(bucket_config);
    }
    // attempt to create the bucket
    let response = request.send().await;
    match response {
        // bucket was created
        Ok(_) => {
            println!("Created S3 bucket {bucket_name}");
            Ok(())
        }
        Err(error) => match error {
            SdkError::ServiceError(service_err) => match service_err.err() {
                // bucket already exists
                CreateBucketError::BucketAlreadyExists(msg) => Err(Error::new(format!(
                    "Failed to create bucket {bucket_name}: {msg}"
                ))),
                // Bucket already exists and we likely have permissions to write
                CreateBucketError::BucketAlreadyOwnedByYou(_msg) => {
                    println!("Bucket already exists: {bucket_name}");
                    Ok(())
                }
                // any other service error fails bucket creation
                other => Err(create_bucket_error(
                    config,
                    bucket_name,
                    &DisplayErrorContext(other).to_string(),
                )),
            },
            // any error that isn't a service error fails bucket creation too
            _ => Err(create_bucket_error(
                config,
                bucket_name,
                &DisplayErrorContext(&error).to_string(),
            )),
        },
    }
}

/// Create the S3 buckets required for a ThoriumCluster
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
pub async fn create_all_buckets(meta: &ClusterMeta) -> Result<(), Error> {
    // get s3 portion of config
    let s3 = &meta.cluster.spec.config.thorium.s3;
    let config = &meta.cluster.spec.config.thorium;
    // get our s3 credentials
    let creds = Credentials::new(&s3.access_key, &s3.secret_token, None, None, "Thorium");
    // build our s3 config
    let mut s3_config_builder = aws_sdk_s3::config::Builder::new()
        .endpoint_url(&s3.endpoint)
        .credentials_provider(SharedCredentialsProvider::new(creds))
        .force_path_style(s3.use_path_style);
    // if we have a region set then add that to our config
    if let Some(region) = &s3.region {
        // set our region
        s3_config_builder =
            s3_config_builder.region(aws_types::region::Region::new(region.clone()));
    }
    // build our s3 config
    let s3_config = s3_config_builder.build();
    // build our s3 client from the s3 config
    let client = Client::from_conf(s3_config);
    // create all Thorium buckets
    create_bucket(s3, &client, &config.files.bucket).await?;
    create_bucket(s3, &client, &config.repos.bucket).await?;
    create_bucket(s3, &client, &config.attachments.bucket).await?;
    create_bucket(s3, &client, &config.results.bucket).await?;
    create_bucket(s3, &client, &config.ephemeral.bucket).await?;
    create_bucket(s3, &client, &config.graphics.bucket).await?;
    create_bucket(s3, &client, &config.reaction_cache.bucket).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serve one scripted response to every request on a local port and record each request
    ///
    /// Returns the endpoint to send requests to and the full requests (request line, headers,
    /// and body) seen so far.
    ///
    /// # Arguments
    ///
    /// * `status` - The status line of every response
    /// * `body` - The body of every response
    async fn mock_s3(
        status: &'static str,
        body: &'static str,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // listen on a free local port
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let record = seen.clone();
        tokio::spawn(async move {
            loop {
                // answer one request per connection
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = Vec::new();
                let mut chunk = [0_u8; 4096];
                // read the request head and any body it declares
                while let Ok(read) = stream.read(&mut chunk).await {
                    if read == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..read]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let length = text[..end]
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().ok())?
                            })
                            .unwrap_or(0);
                        if buf.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                // record the whole request
                record
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf).to_string());
                // send the scripted reply and close the connection
                let reply = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: application/xml\r\n\
                     Connection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(reply.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });
        (endpoint, seen)
    }

    /// Build the S3 config and client the operator would use for an endpoint and region
    ///
    /// # Arguments
    ///
    /// * `endpoint` - The S3 endpoint
    /// * `region` - The region setting, if one is set
    fn test_s3(endpoint: &str, region: Option<&str>) -> (S3, Client) {
        // build the config the operator reads
        let config: S3 = serde_json::from_value(serde_json::json!({
            "access_key": "key",
            "secret_token": "secret",
            "endpoint": endpoint,
            "region": region,
            "use_path_style": true,
        }))
        .unwrap();
        // build a client the way the operator does, signing for us-east-1
        let creds = Credentials::new("key", "secret", None, None, "Thorium");
        let s3_config = aws_sdk_s3::config::Builder::new()
            .endpoint_url(endpoint)
            .credentials_provider(SharedCredentialsProvider::new(creds))
            .force_path_style(true)
            .region(aws_types::region::Region::new("us-east-1"))
            .build();
        (config, Client::from_conf(s3_config))
    }

    /// Get the body of each `CreateBucket` request a mock S3 saw
    ///
    /// # Arguments
    ///
    /// * `seen` - The full requests the mock S3 recorded
    fn create_bucket_bodies(seen: &std::sync::Mutex<Vec<String>>) -> Vec<String> {
        // keep the body after the header block of every PUT
        seen.lock()
            .unwrap()
            .iter()
            .filter(|request| request.starts_with("PUT"))
            .map(|request| {
                request
                    .split_once("\r\n\r\n")
                    .map_or_else(String::new, |(_, body)| body.to_owned())
            })
            .collect()
    }

    /// A bucket created without a region, with an empty region, or in us-east-1 is sent with no
    /// `CreateBucketConfiguration`, and any other region sends its location constraint
    #[tokio::test]
    async fn create_bucket_sends_configuration_only_with_a_constraint() {
        for region in [None, Some(""), Some("us-east-1")] {
            // serve a bucket that is created
            let (endpoint, seen) = mock_s3("200 OK", "").await;
            let (config, client) = test_s3(&endpoint, region);
            create_bucket(&config, &client, "thorium-files")
                .await
                .expect("the bucket is created");
            // the create carries no body, which gateways like Ceph RGW reject when empty
            let bodies = create_bucket_bodies(&seen);
            assert_eq!(bodies.len(), 1, "region {region:?} sent {bodies:?}");
            assert!(
                bodies[0].is_empty(),
                "region {region:?} sent a body: {}",
                bodies[0]
            );
        }
        // any other region pins the bucket to it
        let (endpoint, seen) = mock_s3("200 OK", "").await;
        let (config, client) = test_s3(&endpoint, Some("us-west-2"));
        create_bucket(&config, &client, "thorium-files")
            .await
            .expect("the bucket is created");
        let bodies = create_bucket_bodies(&seen);
        assert_eq!(bodies.len(), 1, "sent {bodies:?}");
        assert!(
            bodies[0].contains("<CreateBucketConfiguration")
                && bodies[0].contains("<LocationConstraint>us-west-2</LocationConstraint>"),
            "no location constraint: {}",
            bodies[0]
        );
    }

    /// A rejected `CreateBucket` names the bucket, the endpoint, and the region setting, and
    /// suggests pre-creating the bucket
    #[tokio::test]
    async fn create_bucket_failure_names_bucket_endpoint_and_region() {
        // reject the create without a message, as Ceph RGW does
        let (endpoint, _) = mock_s3(
            "400 Bad Request",
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>InvalidArgument</Code></Error>",
        )
        .await;
        let (config, client) = test_s3(&endpoint, Some(""));
        let message = create_bucket(&config, &client, "thorium-yara-rulesets")
            .await
            .expect_err("a rejected create must not pass")
            .to_string();
        assert!(
            message.contains("thorium-yara-rulesets"),
            "no bucket: {message}"
        );
        assert!(message.contains(&endpoint), "no endpoint: {message}");
        assert!(
            message.contains("thorium.s3.region"),
            "no region: {message}"
        );
        assert!(message.contains("InvalidArgument"), "no cause: {message}");
        assert!(message.contains("Pre-create"), "no suggestion: {message}");
    }

    /// A failed create names the endpoint without the user info configured in it
    #[test]
    fn create_bucket_error_redacts_endpoint_credentials() {
        // configure an endpoint carrying a user and password
        let (config, _) = test_s3("http://svuser:SVs3Pw42@127.0.0.1:1", None);
        let message = create_bucket_error(&config, "thorium-files", "denied").to_string();
        assert!(!message.contains("SVs3Pw42"), "password leaked: {message}");
        assert!(!message.contains("svuser"), "user leaked: {message}");
        assert!(
            message.contains("http://127.0.0.1:1"),
            "no endpoint: {message}"
        );
        assert!(message.contains("thorium.s3.region: unset"), "{message}");
    }

    /// A failed create names a schemeless endpoint without the user info configured in it
    #[test]
    fn create_bucket_error_redacts_schemeless_endpoint_credentials() {
        // configure an endpoint without a scheme carrying a user and password
        let (config, _) = test_s3("svuser:SVs3Pw42@s3.example.test:9000", None);
        let message = create_bucket_error(&config, "thorium-files", "denied").to_string();
        assert!(!message.contains("SVs3Pw42"), "password leaked: {message}");
        assert!(!message.contains("svuser"), "user leaked: {message}");
        assert!(
            message.contains(" at s3.example.test:9000"),
            "no endpoint: {message}"
        );
        assert!(message.contains("Pre-create the buckets"), "{message}");
    }

    /// A create that fails, including one the SDK refuses because the endpoint isn't a valid URI,
    /// names the endpoint without the user info configured in it, in the error's cause as well
    #[tokio::test]
    async fn create_bucket_failure_redacts_endpoint_credentials() {
        // endpoints carrying user info in forms a URL parser reads differently, and the endpoint
        // each must be named as
        let cases = [
            ("svuser:SVs3Pw42@127.0.0.1:1", "127.0.0.1:1"),
            ("http://svuser:SVs3Pw42@127.0.0.1:1", "http://127.0.0.1:1"),
            ("svuser:12/SVs3Pw42@127.0.0.1:1", "127.0.0.1:1"),
            ("svuser:SVp?a#SVs3Pw42@127.0.0.1:1", "127.0.0.1:1"),
            ("svuser@127.0.0.1:1", "127.0.0.1:1"),
            ("svuser:SVs3Pw42@[::1]:1", "[::1]:1"),
            ("http://svuser:SVs3Pw42@[::1]:1", "http://[::1]:1"),
        ];
        for (endpoint, named) in cases {
            // fail to create a bucket at an endpoint nothing listens on
            let (config, client) = test_s3(endpoint, None);
            let message = create_bucket(&config, &client, "thorium-files")
                .await
                .expect_err("a create at a closed port must not pass")
                .to_string();
            // neither the user nor any part of the password is shown
            for secret in ["svuser", "SVs3Pw42", "SVp"] {
                assert!(
                    !message.contains(secret),
                    "{endpoint} leaked {secret}: {message}"
                );
            }
            assert!(
                message.contains(&format!(" at {named} ")),
                "{endpoint} not named as {named}: {message}"
            );
        }
    }
}
