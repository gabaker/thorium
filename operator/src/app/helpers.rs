use aws_credential_types::provider::SharedCredentialsProvider;
use aws_sdk_s3::{
    Client,
    config::{Credentials, timeout::TimeoutConfig},
    types::{BucketLocationConstraint, CreateBucketConfiguration},
};
use std::time::Duration;
use thorium::client::ClientSettings;
use thorium::{Error, Thorium, conf::S3};

use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::operation::create_bucket::CreateBucketError;

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
/// Get the thorium host from operator args or the target `ThoriumCluster` instance being
/// configured. The url argument is only set outside of k8s (for development), so in a pod the
/// API's in-cluster service url is used.
///
/// # Arguments
///
/// * `meta` - Thorium cluster client and metadata
/// * `url` - The Thorium API URL passed to the operator as an argument
pub fn get_thorium_host(meta: &ClusterMeta, url: Option<&String>) -> String {
    match url {
        // grab url if passed to the operator as an arg, mostly for development
        Some(url) => url.to_owned(),
        // use internal k8s networking by default, leaving the cluster domain to the pod's DNS
        // search path so clusters with a domain other than cluster.local work
        None => format!("http://thorium-api.{}.svc:80", meta.namespace),
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

/// Get the location constraint to create a bucket in a region with
///
/// AWS rejects a `us-east-1` constraint since that is where buckets go without one, so no
/// constraint is sent for it or when no region is set.
///
/// # Arguments
///
/// * `region` - The S3 region from the config, if one is set
fn location_constraint(region: Option<&str>) -> Option<BucketLocationConstraint> {
    region
        .filter(|region| !region.is_empty() && *region != "us-east-1")
        .map(BucketLocationConstraint::from)
}

/// Create an S3 bucket
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
        // any failure (missing, forbidden, or unreachable) falls through to the create below
        Err(error) => println!(
            "Bucket {bucket_name} not accessible, creating it: {}",
            error_chain(&error)
        ),
    }
    // build out the bucket creation config, pinning the bucket to our region when needed
    let bucket_config = CreateBucketConfiguration::builder()
        .set_location_constraint(location_constraint(config.region.as_deref()))
        .build();
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
                // the bucket name is already taken by another account
                CreateBucketError::BucketAlreadyExists(msg) => {
                    Err(Error::new(format!("Failed to create bucket {bucket_name}: {msg}")).into())
                }
                // Bucket already exists and we likely have permissions to write
                CreateBucketError::BucketAlreadyOwnedByYou(_msg) => {
                    println!("Bucket already exists: {bucket_name}");
                    Ok(())
                }
                other => Err(Error::new(format!(
                    "Failed to create bucket {bucket_name}: {}",
                    error_chain(other)
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
            // an S3 that can't be reached yet is waited on
            Err(error) if s3_unreachable(&error) => {
                return Err(s3_waiting(
                    &config.endpoint,
                    &format!("check bucket {bucket}"),
                    &error_chain(&error),
                ));
            }
            Err(error) => Some(error_chain(&error)),
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

    /// Only buckets that failed their head check are reported, along with the endpoint
    #[test]
    fn missing_buckets_reported() {
        // every bucket being accessible reports nothing
        let ok = [("thorium-files", None), ("thorium-repos", None)];
        assert_eq!(missing_buckets_error("https://s3.example", &ok), None);
        // nothing to check reports nothing
        assert_eq!(missing_buckets_error("https://s3.example", &[]), None);
        // each failed bucket is named with its reason
        let some = [
            ("thorium-files", None),
            ("thorium-repos", Some("service error: NotFound".to_owned())),
            (
                "thorium-results",
                Some("service error: Forbidden".to_owned()),
            ),
        ];
        let message = missing_buckets_error("https://s3.example", &some).expect("an error");
        assert!(message.contains("https://s3.example"));
        assert!(message.contains("2 required S3 bucket(s)"));
        assert!(message.contains("thorium-repos (service error: NotFound)"));
        assert!(message.contains("thorium-results (service error: Forbidden)"));
        assert!(!message.contains("thorium-files"));
    }

    /// The operator reaches the API through its in-cluster service unless given a url
    #[tokio::test]
    async fn thorium_host_default() {
        // build cluster metadata in the thorium namespace
        let fake = crate::k8s::clusters::tests::FakeKube::default();
        let meta = crate::k8s::clusters::tests::meta_for(
            crate::k8s::clusters::tests::namespaced_cluster(
                crate::k8s::clusters::tests::full_spec(),
            ),
            &fake.client(),
        );
        // the in-cluster service is the default
        assert_eq!(
            get_thorium_host(&meta, None),
            "http://thorium-api.thorium.svc:80"
        );
        // a url argument wins
        let url = "http://localhost:8080".to_owned();
        assert_eq!(get_thorium_host(&meta, Some(&url)), url);
    }

    /// Buckets in us-east-1 or without a region get no location constraint
    #[test]
    fn us_east_1_has_no_location_constraint() {
        // AWS rejects a us-east-1 constraint and an unset region needs none
        assert_eq!(location_constraint(Some("us-east-1")), None);
        assert_eq!(location_constraint(Some("")), None);
        assert_eq!(location_constraint(None), None);
        // any other region pins the bucket to it
        assert_eq!(
            location_constraint(Some("us-west-2")),
            Some(BucketLocationConstraint::UsWest2)
        );
        assert_eq!(
            location_constraint(Some("custom-1")),
            Some(BucketLocationConstraint::from("custom-1"))
        );
    }
}
