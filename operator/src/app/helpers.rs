use aws_credential_types::provider::SharedCredentialsProvider;
use aws_sdk_s3::{
    Client,
    config::{Credentials, timeout::TimeoutConfig},
    types::{BucketLocationConstraint, CreateBucketConfiguration},
};
use std::time::Duration;
use thorium::client::ClientSettings;
use thorium::utils::helpers::redact_url;
use thorium::{Error, Thorium, conf::S3};

use aws_sdk_s3::config::http::HttpResponse;
use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::operation::create_bucket::CreateBucketError;
use aws_sdk_s3::operation::head_bucket::HeadBucketError;

use crate::k8s::clusters::ClusterMeta;

/// How long in seconds a single request to the Thorium API may take
pub const API_TIMEOUT_SECS: u64 = 30;

/// How long in seconds to wait to connect to the Thorium API
pub const API_CONNECT_TIMEOUT_SECS: u64 = 10;

/// How long a whole S3 operation, retries included, may take
const S3_OPERATION_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a single S3 request attempt may take
const S3_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(20);

/// How long to wait for S3 to send data on an open connection
const S3_READ_TIMEOUT: Duration = Duration::from_secs(20);

/// How long to wait to connect to S3
const S3_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Why a check against one of Thorium's backends didn't pass
#[derive(Debug)]
pub enum CheckError {
    /// The backend isn't reachable or accepting connections yet, so the cluster is still
    /// provisioning, with a description of what is being waited on
    Waiting(String),
    /// The check failed in a way that needs attention, such as rejected credentials
    Failed(Error),
}

impl From<Error> for CheckError {
    /// Treat any other error as a failure that needs attention
    ///
    /// # Arguments
    ///
    /// * `error` - The error the check failed with
    fn from(error: Error) -> Self {
        Self::Failed(error)
    }
}

impl std::fmt::Display for CheckError {
    /// Write the description of this check error
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // both kinds already describe themselves fully
        match self {
            Self::Waiting(details) => f.write_str(details),
            Self::Failed(error) => write!(f, "{error}"),
        }
    }
}

/// Check whether an S3 request failed because S3 couldn't be reached
///
/// A request S3 answered, even with an error, means S3 is up.
///
/// # Arguments
///
/// * `error` - The error the request failed with
fn s3_unreachable<E, R>(error: &SdkError<E, R>) -> bool {
    match error {
        // the request timed out before S3 answered
        SdkError::TimeoutError(_) => true,
        // the connection couldn't be opened or broke before a response arrived
        SdkError::DispatchFailure(failure) => failure.is_io() || failure.is_timeout(),
        // S3 answered or the request was never valid
        _ => false,
    }
}

/// Describe an S3 endpoint that isn't accepting connections yet
///
/// # Arguments
///
/// * `endpoint` - The S3 endpoint that couldn't be reached
/// * `description` - A description of the request that failed
/// * `chain` - The error and its causes
fn s3_waiting(endpoint: &str, description: &str, chain: &str) -> CheckError {
    // name the endpoint without any credentials in its URL
    let endpoint = redact_url(endpoint);
    CheckError::Waiting(format!(
        "Waiting for S3 at {endpoint} to accept connections: failed to {description}: {chain}"
    ))
}

/// Build the Thorium client settings the operator uses for one-off requests
pub fn client_settings() -> ClientSettings {
    // bound every request so an unresponsive API can't stall a reconcile
    ClientSettings {
        timeout: API_TIMEOUT_SECS,
        ..ClientSettings::default()
    }
}

/// Build a plain reqwest client with the operator's Thorium API timeouts
pub fn reqwest_client() -> Result<reqwest::Client, Error> {
    // bound connecting and every request so an unresponsive API can't stall a reconcile
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(API_CONNECT_TIMEOUT_SECS))
        .timeout(Duration::from_secs(API_TIMEOUT_SECS))
        .build()
        .map_err(Error::from)
}

/// Build a Thorium client authenticated with a token and the operator's timeouts
///
/// # Arguments
///
/// * `host` - The url of the Thorium API
/// * `token` - The token to authenticate with
pub async fn thorium_client(host: &str, token: &str) -> Result<Thorium, Error> {
    // start building a client with our token
    let mut builder = Thorium::build(host).token(token);
    // bound every request so an unresponsive API can't stall a reconcile
    builder.settings = client_settings();
    builder.build().await
}

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

/// Describe an error followed by each of its causes
///
/// A cause whose message the description already contains is skipped, since many errors
/// repeat their source's message in their own.
///
/// # Arguments
///
/// * `error` - The error to describe
pub fn error_chain(error: &dyn std::error::Error) -> String {
    // start with the error's own message
    let mut chain = error.to_string();
    // append each cause that adds something new
    let mut source = error.source();
    while let Some(cause) = source {
        let message = cause.to_string();
        if !chain.contains(&message) {
            chain.push_str(": ");
            chain.push_str(&message);
        }
        source = cause.source();
    }
    chain
}

/// How a `HeadBucket` request for a bucket failed
#[derive(Debug, PartialEq)]
enum HeadFailure {
    /// S3 answered that the bucket doesn't exist
    Missing,
    /// S3 couldn't be reached, with the error and its causes
    Unreachable(String),
    /// S3 answered with anything else, described with its status code
    Problem(String),
}

/// Describe a failed `HeadBucket` request with its status code, and the bucket's region on a
/// redirect
///
/// # Arguments
///
/// * `error` - The error from the `HeadBucket` request
fn describe_head_error(error: &SdkError<HeadBucketError, HttpResponse>) -> String {
    match error.raw_response() {
        // S3 answered, so name its status code and any region it redirected us to
        Some(raw) => {
            let status = raw.status().as_u16();
            let region = raw
                .headers()
                .get("x-amz-bucket-region")
                .map_or_else(String::new, |region| {
                    format!(" (the bucket is in region {region})")
                });
            format!(
                "HeadBucket returned HTTP {status}{region}: {}",
                error_chain(error)
            )
        }
        // the request never got an answer, such as when no region is set
        None => format!("HeadBucket failed: {}", error_chain(error)),
    }
}

/// Classify a failed `HeadBucket` request
///
/// Only a 404 means the bucket is missing; a 403, a redirect to another region, a bad
/// request, or a signature error means it can't be checked with this identity or config.
///
/// # Arguments
///
/// * `error` - The error from the `HeadBucket` request
fn head_failure(error: &SdkError<HeadBucketError, HttpResponse>) -> HeadFailure {
    // an S3 that can't be reached yet is waited on
    if s3_unreachable(error) {
        return HeadFailure::Unreachable(error_chain(error));
    }
    // a 404 is the only answer that means the bucket doesn't exist
    let status = error.raw_response().map(|raw| raw.status().as_u16());
    let not_found = matches!(error, SdkError::ServiceError(inner) if inner.err().is_not_found());
    if status == Some(404) || not_found {
        HeadFailure::Missing
    } else {
        HeadFailure::Problem(describe_head_error(error))
    }
}

/// Describe a bucket that exists but can't be checked by Thorium's S3 identity or config
///
/// # Arguments
///
/// * `bucket_name` - The bucket that couldn't be checked
/// * `description` - What went wrong
fn bucket_access_error(bucket_name: &str, description: &str) -> CheckError {
    // name the bucket, what went wrong, and the right Thorium's identity needs
    Error::new(format!(
        "Can't access S3 bucket {bucket_name}: {description}. Thorium's S3 identity must be \
         able to HeadBucket each bucket (s3:ListBucket on AWS) in the configured region"
    ))
    .into()
}

/// Create an S3 bucket
///
/// Only a bucket that `HeadBucket` reports missing is created, so pre-created buckets need no
/// create rights, and any other `HeadBucket` failure is reported with its status code.
///
/// # Arguments
///
/// * `config` - The Thorium S3 configuration
/// * `client` - API client for S3 interface
/// * `bucket_name` - Name of bucket to create
pub async fn create_bucket(
    config: &S3,
    client: &Client,
    bucket_name: &str,
) -> Result<(), CheckError> {
    // skip creation when the bucket already exists and we can reach it, so identities
    // without create rights work against pre-created buckets
    match client.head_bucket().bucket(bucket_name).send().await {
        Ok(_) => {
            println!("Bucket already exists: {bucket_name}");
            return Ok(());
        }
        Err(error) => match head_failure(&error) {
            // only a missing bucket is created below
            HeadFailure::Missing => println!("Bucket {bucket_name} doesn't exist, creating it"),
            // an S3 that can't be reached yet is waited on
            HeadFailure::Unreachable(chain) => {
                return Err(s3_waiting(
                    &config.endpoint,
                    &format!("check bucket {bucket_name}"),
                    &chain,
                ));
            }
            // a bucket we can't check is reported rather than created
            HeadFailure::Problem(description) => {
                return Err(bucket_access_error(bucket_name, &description));
            }
        },
    }
    // build out the bucket creation config
    let mut bucket_config = CreateBucketConfiguration::builder();
    // if we have a region set then set the location con
    if let Some(region) = &config.region {
        // build our constraint
        let constraint = BucketLocationConstraint::from(region.as_str());
        // set our constraint
        bucket_config = bucket_config.location_constraint(constraint);
    }
    // build our bucket config
    let bucket_config = bucket_config.build();
    // attempt to create the bucket
    let response = client
        .create_bucket()
        .create_bucket_configuration(bucket_config)
        .bucket(bucket_name)
        .send()
        .await;
    match response {
        // bucket was created
        Ok(_) => {
            println!("Created S3 bucket {bucket_name}");
            Ok(())
        }
        // an S3 that can't be reached yet is waited on
        Err(error) if s3_unreachable(&error) => Err(s3_waiting(
            &config.endpoint,
            &format!("create bucket {bucket_name}"),
            &error_chain(&error),
        )),
        Err(error) => match error {
            SdkError::ServiceError(service_err) => match service_err.err() {
                // the bucket exists but HeadBucket couldn't see it, so this is an access problem
                CreateBucketError::BucketAlreadyExists(msg) => Err(bucket_access_error(
                    bucket_name,
                    &format!(
                        "HeadBucket returned HTTP 404 but CreateBucket says the bucket already \
                         exists ({msg})"
                    ),
                )),
                // Bucket already exists and we likely have permissions to write
                CreateBucketError::BucketAlreadyOwnedByYou(_msg) => {
                    println!("Bucket already exists: {bucket_name}");
                    Ok(())
                }
                _ => Err(Error::new(format!(
                    "Failed to create bucket {bucket_name}: {service_err:?}"
                ))
                .into()),
            },
            _ => Err(Error::new(format!(
                "Failed to create bucket {bucket_name}: {}",
                error_chain(&error)
            ))
            .into()),
        },
    }
}

/// Describe the buckets that failed a head check, if any did
///
/// Returns `None` when every bucket exists and is accessible, otherwise an error message
/// naming the endpoint and each missing or inaccessible bucket with the reason it failed.
///
/// # Arguments
///
/// * `endpoint` - The S3 endpoint the buckets were checked at
/// * `results` - Each bucket's name and the error from heading it, if any
fn missing_buckets_error(endpoint: &str, results: &[(&str, Option<String>)]) -> Option<String> {
    // describe every bucket that failed its head check
    let failed = results
        .iter()
        .filter_map(|(bucket, error)| error.as_ref().map(|error| format!("{bucket} ({error})")))
        .collect::<Vec<String>>();
    // every bucket being accessible means there is nothing to report
    if failed.is_empty() {
        return None;
    }
    // name the endpoint without any credentials in its URL
    let endpoint = redact_url(endpoint);
    Some(format!(
        "skip_bucket_auto_create is set but {} required S3 bucket(s) are missing or \
         inaccessible at {endpoint}: {}. Create them or grant Thorium's S3 identity access \
         to them",
        failed.len(),
        failed.join(", ")
    ))
}

/// Verify every required bucket exists and is accessible without creating any of them
///
/// An S3 that can't be reached at all is waited on rather than reported as missing buckets.
///
/// # Arguments
///
/// * `config` - The Thorium S3 configuration
/// * `client` - API client for S3 interface
/// * `buckets` - The names of the buckets to check
async fn verify_buckets(config: &S3, client: &Client, buckets: &[&str]) -> Result<(), CheckError> {
    // head each bucket and keep the reason for any failure
    let mut results = Vec::with_capacity(buckets.len());
    for bucket in buckets {
        // a successful head means the bucket exists and we can reach it
        let error = match client.head_bucket().bucket(*bucket).send().await {
            Ok(_) => None,
            Err(error) => match head_failure(&error) {
                // a missing bucket is named as missing
                HeadFailure::Missing => Some("missing: HeadBucket returned HTTP 404".to_owned()),
                // an S3 that can't be reached yet is waited on
                HeadFailure::Unreachable(chain) => {
                    return Err(s3_waiting(
                        &config.endpoint,
                        &format!("check bucket {bucket}"),
                        &chain,
                    ));
                }
                // any other failure is named with its status code
                HeadFailure::Problem(description) => Some(description),
            },
        };
        results.push((*bucket, error));
    }
    // fail with every bucket that isn't usable
    if let Some(message) = missing_buckets_error(&config.endpoint, &results) {
        return Err(Error::new(message).into());
    }
    println!("Every required S3 bucket exists: {}", buckets.join(", "));
    Ok(())
}

/// Create the S3 buckets required for a `ThoriumCluster`
///
/// When `skip_bucket_auto_create` is set nothing is created and every bucket is only
/// checked for existence and access instead. An S3 that can't be reached yet is reported as
/// [`CheckError::Waiting`].
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
pub async fn create_all_buckets(meta: &ClusterMeta) -> Result<(), CheckError> {
    // get s3 portion of config
    let s3 = &meta.conf.thorium.s3;
    let config = &meta.conf.thorium;
    // get our s3 credentials
    let creds = Credentials::new(&s3.access_key, &s3.secret_token, None, None, "Thorium");
    // bound every S3 call so an unresponsive store can't stall a reconcile
    let timeouts = TimeoutConfig::builder()
        .operation_timeout(S3_OPERATION_TIMEOUT)
        .operation_attempt_timeout(S3_ATTEMPT_TIMEOUT)
        .read_timeout(S3_READ_TIMEOUT)
        .connect_timeout(S3_CONNECT_TIMEOUT)
        .build();
    // build our s3 config
    let mut s3_config_builder = aws_sdk_s3::config::Builder::new()
        .endpoint_url(&s3.endpoint)
        .credentials_provider(SharedCredentialsProvider::new(creds))
        .force_path_style(s3.use_path_style)
        .timeout_config(timeouts);
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
    // list every Thorium bucket once, since sites may share a bucket between uses
    let mut buckets: Vec<&str> = Vec::with_capacity(7);
    for bucket in [
        &config.files.bucket,
        &config.repos.bucket,
        &config.attachments.bucket,
        &config.results.bucket,
        &config.ephemeral.bucket,
        &config.graphics.bucket,
        &config.reaction_cache.bucket,
    ] {
        if !buckets.contains(&bucket.as_str()) {
            buckets.push(bucket);
        }
    }
    // only check pre-created buckets when bucket creation is disabled
    if s3.skip_bucket_auto_create {
        return verify_buckets(s3, &client, &buckets).await;
    }
    // create any Thorium bucket that doesn't exist yet
    for bucket in buckets {
        create_bucket(s3, &client, bucket).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An error with an optional cause, for building error chains
    #[derive(Debug)]
    struct Chained {
        /// This error's message
        message: &'static str,
        /// The error that caused this one
        source: Option<Box<Chained>>,
    }

    impl std::fmt::Display for Chained {
        /// Write this error's message
        ///
        /// # Arguments
        ///
        /// * `f` - The formatter to write to
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            // only this error's own message is written
            f.write_str(self.message)
        }
    }

    impl std::error::Error for Chained {
        /// Get the error that caused this one
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            // expose our cause as a plain error
            self.source
                .as_deref()
                .map(|source| source as &(dyn std::error::Error + 'static))
        }
    }

    /// Build a chain of errors from the outermost message inwards
    ///
    /// # Arguments
    ///
    /// * `messages` - Each error's message, outermost first
    fn chain(messages: &[&'static str]) -> Chained {
        // wrap each message around the ones after it
        let mut errors = messages.iter().rev();
        let innermost = Chained {
            message: errors.next().expect("at least one message"),
            source: None,
        };
        errors.fold(innermost, |source, message| Chained {
            message,
            source: Some(Box::new(source)),
        })
    }

    /// Every cause is appended unless the description already contains it
    #[test]
    fn error_chain_appends_causes() {
        // an error without a cause is just its message
        assert_eq!(error_chain(&chain(&["request failed"])), "request failed");
        // each cause is appended in order
        assert_eq!(
            error_chain(&chain(&[
                "error sending request",
                "client error (Connect)",
                "invalid peer certificate: certificate not valid for name"
            ])),
            "error sending request: client error (Connect): invalid peer certificate: certificate not valid for name"
        );
        // a cause the outer message already repeats is skipped
        assert_eq!(
            error_chain(&chain(&[
                "pool broken: connection refused",
                "connection refused"
            ])),
            "pool broken: connection refused"
        );
        // an empty cause adds nothing
        assert_eq!(error_chain(&chain(&["failed", ""])), "failed");
    }

    /// Only an S3 that couldn't be reached is waited on
    #[test]
    fn s3_unreachable_classified() {
        // a timed out request or a failed connection means S3 isn't up yet
        let timeout = SdkError::<(), ()>::timeout_error("timed out");
        assert!(s3_unreachable(&timeout));
        let refused = SdkError::<(), ()>::dispatch_failure(aws_sdk_s3::error::ConnectorError::io(
            "connection refused".into(),
        ));
        assert!(s3_unreachable(&refused));
        let slow = SdkError::<(), ()>::dispatch_failure(
            aws_sdk_s3::error::ConnectorError::timeout("connect timed out".into()),
        );
        assert!(s3_unreachable(&slow));
        // a request that was invalid or that S3 answered is not waited on
        let user = SdkError::<(), ()>::dispatch_failure(aws_sdk_s3::error::ConnectorError::user(
            "bad request".into(),
        ));
        assert!(!s3_unreachable(&user));
        let construction = SdkError::<(), ()>::construction_failure("invalid endpoint");
        assert!(!s3_unreachable(&construction));
        let service = SdkError::<(), ()>::service_error((), ());
        assert!(!s3_unreachable(&service));
        // the waiting message names the endpoint, the request, and the cause
        let CheckError::Waiting(details) = s3_waiting(
            "http://s3:9000",
            "create bucket files",
            "connection refused",
        ) else {
            panic!("an S3 that can't be reached should be waited on");
        };
        assert_eq!(
            details,
            "Waiting for S3 at http://s3:9000 to accept connections: failed to create bucket \
             files: connection refused"
        );
    }

    /// Other errors are failures and both kinds display their description
    #[test]
    fn check_error_from_and_display() {
        // a plain Thorium error needs attention
        let failed = CheckError::from(Error::new("rejected"));
        assert!(matches!(failed, CheckError::Failed(_)));
        assert_eq!(failed.to_string(), Error::new("rejected").to_string());
        // a waiting error displays just its description
        assert_eq!(
            CheckError::Waiting("waiting".to_owned()).to_string(),
            "waiting"
        );
    }

    /// Build the error the SDK returns for a `HeadBucket` answered with a status and no body
    ///
    /// # Arguments
    ///
    /// * `status` - The HTTP status S3 answered with
    /// * `region` - The region S3 named in `x-amz-bucket-region`, if any
    fn head_error(status: u16, region: Option<&str>) -> SdkError<HeadBucketError, HttpResponse> {
        // a HEAD answer has no body, so the SDK's error has no code or message
        let mut raw = HttpResponse::new(
            status.try_into().unwrap(),
            aws_sdk_s3::primitives::SdkBody::empty(),
        );
        if let Some(region) = region {
            raw.headers_mut()
                .insert("x-amz-bucket-region", region.to_owned());
        }
        // a 404 is the modeled NotFound error and anything else is generic
        let error = if status == 404 {
            HeadBucketError::NotFound(aws_sdk_s3::types::error::NotFound::builder().build())
        } else {
            HeadBucketError::generic(aws_sdk_s3::error::ErrorMetadata::builder().build())
        };
        SdkError::service_error(error, raw)
    }

    /// Only a 404 means a bucket is missing, and every other answer names its status code
    #[test]
    fn head_bucket_failures_classified() {
        // a 404 is a missing bucket
        assert_eq!(head_failure(&head_error(404, None)), HeadFailure::Missing);
        // a 403 names its status code
        let HeadFailure::Problem(forbidden) = head_failure(&head_error(403, None)) else {
            panic!("a 403 is not a missing bucket");
        };
        assert!(
            forbidden.starts_with("HeadBucket returned HTTP 403"),
            "{forbidden}"
        );
        // a redirect names the bucket's region
        let HeadFailure::Problem(moved) = head_failure(&head_error(301, Some("eu-west-1"))) else {
            panic!("a 301 is not a missing bucket");
        };
        assert!(
            moved.starts_with("HeadBucket returned HTTP 301 (the bucket is in region eu-west-1)"),
            "{moved}"
        );
        // a bad request names its status code
        let HeadFailure::Problem(bad) = head_failure(&head_error(400, None)) else {
            panic!("a 400 is not a missing bucket");
        };
        assert!(bad.starts_with("HeadBucket returned HTTP 400"), "{bad}");
    }

    /// Only buckets that failed their head check are reported, along with the endpoint
    #[test]
    fn missing_buckets_reported() {
        // every bucket being accessible reports nothing
        let ok = [("thorium-files", None), ("thorium-repos", None)];
        assert_eq!(missing_buckets_error("https://s3.example", &ok), None);
        // nothing to check reports nothing
        assert_eq!(missing_buckets_error("https://s3.example", &[]), None);
        // each failed bucket is named with its reason
        let forbidden = describe_head_error(&head_error(403, None));
        let some = [
            ("thorium-files", None),
            (
                "thorium-repos",
                Some("missing: HeadBucket returned HTTP 404".to_owned()),
            ),
            ("thorium-results", Some(forbidden.clone())),
        ];
        let message = missing_buckets_error("https://s3.example", &some).expect("an error");
        assert!(message.contains("https://s3.example"));
        assert!(message.contains("2 required S3 bucket(s)"));
        assert!(message.contains("thorium-repos (missing: HeadBucket returned HTTP 404)"));
        assert!(message.contains(&format!("thorium-results ({forbidden})")));
        assert!(!message.contains("thorium-files"));
    }

    /// A scripted S3 response: status line, extra headers, and body
    type MockResponse = (
        &'static str,
        Vec<(&'static str, &'static str)>,
        &'static str,
    );

    /// Serve scripted S3 responses on a local port and record each request's method and path
    ///
    /// Returns the endpoint to send requests to and the requests seen so far.
    ///
    /// # Arguments
    ///
    /// * `head` - The response to every HEAD request
    /// * `put` - The response to every PUT request
    async fn mock_s3(
        head: MockResponse,
        put: MockResponse,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use std::fmt::Write;
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
                // record the method and path
                let text = String::from_utf8_lossy(&buf).to_string();
                let line = text.lines().next().unwrap_or_default().to_owned();
                record.lock().unwrap().push(line.clone());
                // pick the scripted response for this method
                let (status, headers, body) = if line.starts_with("HEAD") {
                    &head
                } else {
                    &put
                };
                // write the status line and the length of the body, which HEAD replies don't send
                let mut reply = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
                    if line.starts_with("HEAD") {
                        0
                    } else {
                        body.len()
                    }
                );
                // add the scripted headers and end the header block
                for (name, value) in headers {
                    let _ = write!(reply, "{name}: {value}\r\n");
                }
                reply.push_str("\r\n");
                // add the body to replies other than HEAD
                if !line.starts_with("HEAD") {
                    reply.push_str(body);
                }
                // send the reply and close the connection
                let _ = stream.write_all(reply.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });
        (endpoint, seen)
    }

    /// Build the S3 config and client the operator would use for an endpoint and identity
    ///
    /// # Arguments
    ///
    /// * `endpoint` - The S3 endpoint
    /// * `key` - The access key of the identity
    /// * `secret` - The secret of the identity
    fn test_s3(endpoint: &str, key: &str, secret: &str) -> (S3, Client) {
        // build the config the operator reads
        let config: S3 = serde_json::from_value(serde_json::json!({
            "access_key": key,
            "secret_token": secret,
            "endpoint": endpoint,
            "region": "us-east-1",
            "use_path_style": true,
        }))
        .unwrap();
        // build a client the way the operator does
        let creds = Credentials::new(key, secret, None, None, "Thorium");
        let s3_config = aws_sdk_s3::config::Builder::new()
            .endpoint_url(endpoint)
            .credentials_provider(SharedCredentialsProvider::new(creds))
            .force_path_style(true)
            .region(aws_types::region::Region::new("us-east-1"))
            .build();
        (config, Client::from_conf(s3_config))
    }

    /// Messages that quote the S3 endpoint never carry the user info configured in it
    #[tokio::test]
    async fn s3_messages_redact_endpoint_credentials() {
        // an endpoint that refuses connections is waited on, named without its user info
        let (config, client) = test_s3("http://svuser:SVs3Pw42@127.0.0.1:1", "key", "secret");
        let create = create_bucket(&config, &client, "thorium-files")
            .await
            .expect_err("an unreachable S3 must not pass");
        let verify = verify_buckets(&config, &client, &["thorium-files"])
            .await
            .expect_err("an unreachable S3 must not pass");
        for error in [create, verify] {
            let message = error.to_string();
            assert!(!message.contains("SVs3Pw42"), "password leaked: {message}");
            assert!(!message.contains("svuser"), "user leaked: {message}");
            assert!(
                message.contains("http://127.0.0.1:1"),
                "no endpoint: {message}"
            );
        }
        // a reachable endpoint whose buckets are denied is named without its user info too
        let (endpoint, _) = mock_s3(("403 Forbidden", vec![], ""), ("200 OK", vec![], "")).await;
        let authed = endpoint.replacen("http://", "http://svuser:SVs3Pw42@", 1);
        let (config, client) = test_s3(&authed, "key", "secret");
        let message = verify_buckets(&config, &client, &["thorium-files"])
            .await
            .expect_err("a forbidden bucket must not pass")
            .to_string();
        assert!(!message.contains("SVs3Pw42"), "password leaked: {message}");
        assert!(message.contains(&endpoint), "no endpoint: {message}");
    }

    /// A `HeadBucket` redirect to another region is reported with that region, and no bucket is
    /// created
    #[tokio::test]
    async fn head_bucket_redirect_reports_region() {
        // serve a 301 naming the bucket's region for every HEAD
        let (endpoint, seen) = mock_s3(
            (
                "301 Moved Permanently",
                vec![("x-amz-bucket-region", "eu-west-1")],
                "",
            ),
            ("200 OK", vec![], ""),
        )
        .await;
        let (config, client) = test_s3(&endpoint, "key", "secret");
        // checking the bucket fails, naming the status and the region
        let error = create_bucket(&config, &client, "thorium-files")
            .await
            .expect_err("a redirected bucket must not pass");
        let message = error.to_string();
        assert!(message.contains("301"), "no status code: {message}");
        assert!(message.contains("eu-west-1"), "no region: {message}");
        assert!(message.contains("thorium-files"), "no bucket: {message}");
        // no bucket was created
        let seen = seen.lock().unwrap().clone();
        assert!(
            seen.iter().all(|line| !line.starts_with("PUT")),
            "sent {seen:?}"
        );
    }

    /// A forbidden `HeadBucket` is an access problem, even when a create then says the name
    /// already exists
    #[tokio::test]
    async fn head_bucket_forbidden_is_an_access_problem() {
        // serve a 403 for HEAD and BucketAlreadyExists for PUT
        let (endpoint, seen) = mock_s3(
            ("403 Forbidden", vec![], ""),
            (
                "409 Conflict",
                vec![("Content-Type", "application/xml")],
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>BucketAlreadyExists</Code>\
                 <Message>The requested bucket name is not available</Message></Error>",
            ),
        )
        .await;
        let (config, client) = test_s3(&endpoint, "key", "secret");
        // checking the bucket fails as an access problem naming HeadBucket's status
        let error = create_bucket(&config, &client, "thorium-files")
            .await
            .expect_err("a forbidden bucket must not pass");
        let message = error.to_string();
        assert!(message.contains("403"), "no status code: {message}");
        assert!(message.contains("HeadBucket"), "no HeadBucket: {message}");
        assert!(
            !message.contains("Failed to create bucket"),
            "reported as a create: {message}"
        );
        // no create was attempted for a bucket we can't see
        let seen = seen.lock().unwrap().clone();
        assert!(
            seen.iter().all(|line| !line.starts_with("PUT")),
            "sent {seen:?}"
        );
        // with bucket creation skipped the status code is reported too
        let error = verify_buckets(&config, &client, &["thorium-files"])
            .await
            .expect_err("a forbidden bucket must not pass");
        let message = error.to_string();
        assert!(message.contains("403"), "no status code: {message}");
    }

    /// A create answered with `BucketAlreadyExists` after `HeadBucket` reported the bucket missing
    /// is an access problem, not a name clash
    #[tokio::test]
    async fn bucket_exists_after_missing_head_is_an_access_problem() {
        // serve a 404 for HEAD and BucketAlreadyExists for PUT
        let (endpoint, seen) = mock_s3(
            ("404 Not Found", vec![], ""),
            (
                "409 Conflict",
                vec![("Content-Type", "application/xml")],
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>BucketAlreadyExists</Code>\
                 <Message>The requested bucket name is not available</Message></Error>",
            ),
        )
        .await;
        let (config, client) = test_s3(&endpoint, "key", "secret");
        // the create is attempted for the missing bucket and its conflict reported as access
        let message = create_bucket(&config, &client, "thorium-files")
            .await
            .expect_err("a bucket we can't see must not pass")
            .to_string();
        assert!(
            message.contains("Can't access S3 bucket thorium-files"),
            "{message}"
        );
        assert!(message.contains("already exists"), "{message}");
        let seen = seen.lock().unwrap().clone();
        assert!(
            seen.iter().any(|line| line.starts_with("PUT")),
            "sent {seen:?}"
        );
    }

    /// Against `SeaweedFS` with an existing bucket, an identity with read and write rights passes,
    /// a write-only identity fails naming `HeadBucket`'s 403, and a missing bucket is created by
    /// an admin identity
    ///
    /// Needs a `SeaweedFS` S3 at `THORIUM_TEST_SEAWEEDFS` with the identities `admin` (Admin),
    /// `readwrite` (Read, Write, List), and `writeonly` (Write), each using its name as both
    /// its access key and secret, and the bucket `thorium-existing` created.
    #[tokio::test]
    #[ignore = "needs a SeaweedFS S3 with test identities"]
    async fn seaweedfs_bucket_rights() {
        // get the SeaweedFS endpoint to test against
        let endpoint = std::env::var("THORIUM_TEST_SEAWEEDFS").expect("THORIUM_TEST_SEAWEEDFS");
        // read and write rights on an existing bucket pass without create rights
        let (config, client) = test_s3(&endpoint, "readwrite", "readwrite");
        create_bucket(&config, &client, "thorium-existing")
            .await
            .expect("read and write rights are enough for an existing bucket");
        // write-only rights fail naming HeadBucket's 403, not a create
        let (config, client) = test_s3(&endpoint, "writeonly", "writeonly");
        let message = create_bucket(&config, &client, "thorium-existing")
            .await
            .expect_err("write-only rights can't check a bucket")
            .to_string();
        assert!(message.contains("403"), "no status code: {message}");
        assert!(message.contains("thorium-existing"), "no bucket: {message}");
        assert!(
            !message.contains("Failed to create bucket"),
            "reported as a create: {message}"
        );
        // a missing bucket is created by an identity with create rights
        let (config, client) = test_s3(&endpoint, "admin", "admin");
        let bucket = format!("thorium-new-{}", std::process::id());
        create_bucket(&config, &client, &bucket)
            .await
            .expect("an admin creates a missing bucket");
        client
            .head_bucket()
            .bucket(&bucket)
            .send()
            .await
            .expect("the created bucket exists");
    }
}
