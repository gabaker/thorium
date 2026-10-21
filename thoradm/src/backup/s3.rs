//! Backup/Restore data in S3

use aws_credential_types::provider::SharedCredentialsProvider;
use aws_sdk_s3::Client;
use aws_sdk_s3::config::Credentials;
use aws_sdk_s3::primitives::{ByteStream, SdkBody};
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
use bytes::{Buf, BytesMut};
use data_encoding::HEXLOWER;
use futures::stream::{FuturesOrdered, FuturesUnordered};
use futures::{FutureExt, StreamExt};
use indicatif::ProgressBar;
use kanal::{AsyncReceiver, AsyncSender};
use num_format::{Locale, ToFormattedString};
use rkyv::Archive;
use rkyv::validation::validators::DefaultValidator;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use thorium::Conf;
use tokio::fs::File;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tokio_util::io::ReaderStream;

use crate::Error;
use crate::args::BackupComponents;
use crate::shared::retry::{RetryPolicy, Transient};
use crate::shared::workers::Orders;

use super::failures::{FailureLog, FailureRecord, Operation, OrderKey};
use super::manifest::{ObjectIndexLine, file_sha256, read_index};
use super::sink::{FileSink, PRIVATE_FILE_MODE, SinkFactory, create_private_dir};
use super::{ArchiveReader, MonitorUpdate, Utils};

/// How many characters of an object's key name the directory that spreads a backup's objects
pub const OBJECT_PREFIX_CHARS: usize = 6;

/// Returns where a backup keeps an object, relative to its component's `objects/` directory
///
/// The S3 ids keep each bucket role's objects apart (`files/`, `repos/`); every layout then puts
/// the object under the first characters of its key's first segment, and then under its key, whose
/// `/`s become directories. The spreading directory never holds a `/`, so a restore reads the
/// key back from what follows it.
///
/// # Arguments
///
/// * `component` - The S3 component that copies the object
/// * `role` - The bucket role the object came from
/// * `key` - The object's key
#[must_use]
pub fn object_relative_path(component: BackupComponents, role: &str, key: &str) -> PathBuf {
    // the S3 ids are split by bucket role first
    let mut path = PathBuf::new();
    if component == BackupComponents::S3IdsObjects {
        path.push(role);
    }
    // then by the first characters of the key's first segment, and then by the key itself
    let first = key.split('/').next().unwrap_or(key);
    let prefix = first
        .char_indices()
        .nth(OBJECT_PREFIX_CHARS)
        .map_or(first, |(end, _)| &first[..end]);
    path.push(prefix);
    path.push(key);
    path
}

/// Returns whether a backup can keep an object's key as a path under its `objects/` directory
/// and read the same key back
///
/// # Arguments
///
/// * `key` - The object's key
#[must_use]
pub fn keepable_key(key: &str) -> bool {
    // an empty segment (a leading `/`, `//`, or a trailing `/`) is lost on the way back, and `.`
    // and `..` would lead elsewhere
    key.split('/')
        .all(|segment| !matches!(segment, "" | "." | ".."))
}

/// The mode downloaded objects and object indexes are created with, before the umask
const OBJECT_MODE: u32 = PRIVATE_FILE_MODE;

/// How many directories objects landed in are collected before they are synced together
const DIR_SYNC_BATCH: usize = 1_024;

/// The different s3 monitor updates
pub enum S3MonitorUpdate {
    /// A single object was downloaded
    Update(usize),
    /// All operations have completed
    Finished,
}

/// The s3 backup monitor
pub struct S3Monitor {
    /// The total number of objects we have backed up
    total_objects: u64,
    /// The reciever to pull updates from
    receiver: AsyncReceiver<S3MonitorUpdate>,
    /// The progress bar to report updates on
    progress: ProgressBar,
}

impl S3Monitor {
    /// Create a new s3 monitor
    ///
    /// # Arguments
    ///
    /// * `receiver` - The channel to receive updates on
    /// * `progress` - The bar to report progress on
    pub fn new(receiver: AsyncReceiver<S3MonitorUpdate>, progress: ProgressBar) -> Self {
        S3Monitor {
            total_objects: 0,
            receiver,
            progress,
        }
    }

    /// Start monitoring and reporting updates
    pub async fn start(mut self) -> Result<(), Error> {
        // track the number of updates since our last message update
        let mut since_msg_update = 0;
        // loop forever until the finished update is received
        loop {
            // handle this update
            match self.receiver.recv().await? {
                // update our progress bar with this new info
                S3MonitorUpdate::Update(amt) => {
                    // update our progress bar
                    self.progress.inc(amt as u64);
                    // update our total objects count
                    self.total_objects += 1;
                }
                S3MonitorUpdate::Finished => {
                    // set our message
                    self.progress.set_message(self.total_objects.to_string());
                    // stop monitoring for updates
                    break;
                }
            }
            // update our message if its been a while since we did or we have more no more updates
            if since_msg_update >= 100 || self.receiver.is_empty() {
                // set our new message
                self.progress
                    .set_message(self.total_objects.to_formatted_string(&Locale::en));
                // reset our messages since message update counter
                since_msg_update = 0;
            } else {
                // increment our messages since update counter
                since_msg_update += 1;
            }
        }
        // shutdown our progress bar
        self.progress.finish();
        Ok(())
    }
}

/// What a download left at an object's final path
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Landed {
    /// The bucket the object came from
    pub bucket: String,
    /// The object's key
    pub key: String,
    /// The object's size in bytes
    pub size: u64,
    /// The `ETag` S3 returned for it, when it returned one
    pub etag: Option<String>,
    /// The sha256 of the bytes at the final path
    pub sha256: String,
    /// The object's final path in the backup
    pub path: PathBuf,
}

/// One S3 component's object index: written as objects land, and the index of the interrupted
/// run being resumed, which decides which files already on disk are kept
pub struct ComponentIndex {
    /// The index file being written
    file: tokio::sync::Mutex<Box<dyn FileSink>>,
    /// How many objects were indexed
    objects: AtomicU64,
    /// How many bytes the indexed objects hold
    bytes: AtomicU64,
    /// The interrupted run's index, by bucket and key: each object's size and sha256
    prior: HashMap<(String, String), (u64, String)>,
    /// The paths of the objects that landed, kept only when resuming an interrupted run, whose
    /// files of objects this run didn't land are removed at the end
    landed: Option<Mutex<HashSet<PathBuf>>>,
    /// The directories objects were moved into, or new directories were created in, since they
    /// were last synced
    unsynced: Mutex<HashSet<PathBuf>>,
}

impl ComponentIndex {
    /// Starts a component's index: reads the index an interrupted run left, then creates a new one
    ///
    /// # Arguments
    ///
    /// * `path` - The index file
    /// * `sinks` - The file-system seam to write through
    /// * `resume` - Whether the run resumes an interrupted backup, so the paths that land are
    ///   kept for [`ComponentIndex::prune`]
    pub async fn start(path: &Path, sinks: &dyn SinkFactory, resume: bool) -> Result<Self, Error> {
        // the earlier run's index says which files on disk are whole
        let prior = read_index(path).await?;
        let file = sinks.create(path, OBJECT_MODE).await?;
        Ok(ComponentIndex {
            file: tokio::sync::Mutex::new(file),
            objects: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            prior,
            landed: resume.then(Mutex::default),
            unsynced: Mutex::default(),
        })
    }

    /// Appends one landed object's line and hands it to the operating system
    ///
    /// # Arguments
    ///
    /// * `landed` - The object that landed
    pub async fn add(&self, landed: &Landed) -> Result<(), Error> {
        // one JSON object per line
        let line = ObjectIndexLine {
            bucket: landed.bucket.clone(),
            key: landed.key.clone(),
            size: landed.size,
            etag: landed.etag.clone().unwrap_or_default(),
            sha256: landed.sha256.clone(),
        };
        let mut bytes = serde_json::to_vec(&line)?;
        bytes.push(b'\n');
        // write under the lock so lines never interleave
        let mut file = self.file.lock().await;
        file.write_all(&bytes).await?;
        file.flush().await?;
        drop(file);
        // count the object for the manifest
        self.objects.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(landed.size, Ordering::Relaxed);
        // remember its path when the run will prune what an interrupted run left
        if let Some(paths) = &self.landed {
            paths
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(landed.path.clone());
        }
        Ok(())
    }

    /// Removes every file under a resumed component's `objects/` directory that isn't an object
    /// this run landed, and returns how many it removed
    ///
    /// An interrupted run can leave files of objects no row names any more, which restore would
    /// otherwise upload as objects. A run that doesn't resume removes nothing.
    ///
    /// # Arguments
    ///
    /// * `objects` - The component's `objects/` directory
    /// * `sinks` - The file-system seam to remove files through
    pub async fn prune(&self, objects: &Path, sinks: &dyn SinkFactory) -> Result<u64, Error> {
        // only a resumed run kept the paths that landed
        let Some(paths) = &self.landed else {
            return Ok(0);
        };
        let landed = std::mem::take(
            &mut *paths
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        // walk depth first, removing each file that didn't land and syncing its directory
        let mut removed = 0;
        let mut pending = vec![objects.to_path_buf()];
        while let Some(dir) = pending.pop() {
            let mut listing = match tokio::fs::read_dir(&dir).await {
                Ok(listing) => listing,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            let mut changed = false;
            while let Some(entry) = listing.next_entry().await? {
                let path = entry.path();
                if entry.file_type().await?.is_dir() {
                    pending.push(path);
                } else if !landed.contains(&path) {
                    sinks.remove(&path).await?;
                    removed += 1;
                    changed = true;
                }
            }
            if changed {
                sinks.sync_dir(&dir).await?;
            }
        }
        Ok(removed)
    }

    /// Notes the directory a landed object was moved into, returning a batch of directories to
    /// sync once enough are waiting
    ///
    /// # Arguments
    ///
    /// * `landed` - The object that landed
    fn note_dir(&self, landed: &Landed) -> Option<Vec<PathBuf>> {
        // a file at the backup root has no directory of its own to note
        let dir = landed.path.parent()?;
        let mut unsynced = self
            .unsynced
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        unsynced.insert(dir.to_path_buf());
        // hand the whole batch over once it is full
        (unsynced.len() >= DIR_SYNC_BATCH).then(|| unsynced.drain().collect())
    }

    /// Notes directories that new directories were created in, to sync with the next batch
    ///
    /// # Arguments
    ///
    /// * `parents` - The directories that gained a new directory
    fn note_parents(&self, parents: impl IntoIterator<Item = PathBuf>) {
        // the next batch or the component's end syncs them with the objects' directories
        self.unsynced
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend(parents);
    }

    /// Takes every directory objects were moved into since it was last synced
    pub fn take_unsynced(&self) -> Vec<PathBuf> {
        // empty the set so each directory is synced once
        self.unsynced
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain()
            .collect()
    }

    /// Syncs the index to disk and returns how many objects and bytes it lists
    pub async fn finish(&self) -> Result<(u64, u64), Error> {
        // force every line to disk before the manifest pins the file by its sha256
        self.file.lock().await.sync().await?;
        Ok((
            self.objects.load(Ordering::Relaxed),
            self.bytes.load(Ordering::Relaxed),
        ))
    }

    /// Returns the size and sha256 the interrupted run's index lists for an object
    ///
    /// # Arguments
    ///
    /// * `bucket` - The object's bucket
    /// * `key` - The object's key
    fn listed(&self, bucket: &str, key: &str) -> Option<&(u64, String)> {
        // index lines are keyed by bucket and key
        self.prior.get(&(bucket.to_owned(), key.to_owned()))
    }
}

/// The directory, beside `objects/` in a component's directory, that downloads are written into
/// before they are complete; restore never reads it, and no object key can name a path in it
pub const STAGING_DIR: &str = "staging";

/// Returns the name an object's download is staged under: the lowercase hex sha256 of its bucket
/// and key, so two objects never share a staging file whatever their keys are
///
/// # Arguments
///
/// * `bucket` - The object's bucket
/// * `key` - The object's key
#[must_use]
pub fn staging_name(bucket: &str, key: &str) -> String {
    // the digest names the object, so it names its staging file too
    HEXLOWER.encode(&object_digest(bucket, key))
}

/// Returns the sha256 of an object's bucket and key
///
/// # Arguments
///
/// * `bucket` - The object's bucket
/// * `key` - The object's key
fn object_digest(bucket: &str, key: &str) -> [u8; 32] {
    // a separator no bucket name holds keeps bucket and key apart in the hash
    let mut hasher = Sha256::new();
    hasher.update(bucket.as_bytes());
    hasher.update([0]);
    hasher.update(key.as_bytes());
    hasher.finalize().into()
}

/// Empties a component's staging directory, creating it when missing
///
/// Anything in it was left by a download an interrupted run never finished.
///
/// # Arguments
///
/// * `staging` - The component's staging directory
pub async fn clear_staging(staging: &Path) -> Result<(), Error> {
    // a missing directory is already empty
    remove_staging(staging).await?;
    create_private_dir(staging).await?;
    Ok(())
}

/// Removes a component's staging directory and anything left in it
///
/// # Arguments
///
/// * `staging` - The component's staging directory
pub async fn remove_staging(staging: &Path) -> Result<(), Error> {
    // a missing directory is already gone
    match tokio::fs::remove_dir_all(staging).await {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(Error::from(error)),
        _ => Ok(()),
    }
}

/// The endpoint authorities thoradm's own tests route virtual-hosted requests through: a request
/// to `<bucket>.<authority>` connects to `<authority>` and keeps its `Host` header, since the test
/// host resolves no `<bucket>.localhost` names
#[cfg(feature = "test-support")]
static VIRTUAL_HOST_ROUTES: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Routes requests a client makes to virtual-hosted buckets of an endpoint through the endpoint
/// itself, so a test's S3 proxy sees each request's `Host` header; only thoradm's own tests can
/// call it
///
/// # Arguments
///
/// * `authority` - The endpoint's host and port (`localhost:39123`)
///
/// # Panics
///
/// Panics if another test panicked while registering a route.
#[cfg(feature = "test-support")]
#[doc(hidden)]
pub fn route_virtual_hosts_through(authority: &str) {
    // register each authority once
    let mut routes = VIRTUAL_HOST_ROUTES.lock().unwrap();
    if !routes.iter().any(|route| route == authority) {
        routes.push(authority.to_owned());
    }
}

/// Sends a virtual-hosted request to its endpoint's own authority, keeping the bucket's host name
/// in the `Host` header
#[cfg(feature = "test-support")]
#[derive(Debug)]
struct VirtualHostRoute {
    /// The endpoint's host and port
    authority: String,
}

#[cfg(feature = "test-support")]
impl aws_sdk_s3::config::Intercept for VirtualHostRoute {
    /// Names this interceptor in the SDK's errors
    fn name(&self) -> &'static str {
        "VirtualHostRoute"
    }

    /// Rewrites a request to `<bucket>.<authority>` to connect to `<authority>`
    ///
    /// # Arguments
    ///
    /// * `context` - The request about to be sent
    /// * `_components` - The client's runtime components
    /// * `_config` - The request's config
    fn modify_before_transmit(
        &self,
        context: &mut aws_sdk_s3::config::interceptors::BeforeTransmitInterceptorContextMut<'_>,
        _components: &aws_sdk_s3::config::RuntimeComponents,
        _config: &mut aws_sdk_s3::config::ConfigBag,
    ) -> Result<(), aws_sdk_s3::error::BoxError> {
        // only a request whose host is a subdomain of the endpoint is rewritten
        let request = context.request_mut();
        let uri = request.uri().to_owned();
        let Some((scheme, rest)) = uri.split_once("://") else {
            return Ok(());
        };
        // split the host from the path
        let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
        // send a virtual-hosted request to the endpoint, keeping the bucket in the Host header
        if host.ends_with(&format!(".{}", self.authority)) {
            let host = host.to_owned();
            request.set_uri(format!("{scheme}://{}/{path}", self.authority))?;
            request.headers_mut().insert("host", host);
        }
        Ok(())
    }
}

/// Builds the S3 client a backup or restore of a cluster uses
///
/// # Arguments
///
/// * `conf` - The config for this Thorium cluster
/// * `path_style` - Whether to address buckets in the path rather than the host name
#[must_use]
pub fn s3_client(conf: &Conf, path_style: bool) -> Client {
    // get our s3 conf
    let s3_conf = &conf.thorium.s3;
    // get our s3 credentials
    let creds = Credentials::new(
        &s3_conf.access_key,
        &s3_conf.secret_token,
        None,
        None,
        "Thorium",
    );
    // build our s3 s3_config
    let mut s3_config_builder = aws_sdk_s3::config::Builder::new()
        .endpoint_url(&s3_conf.endpoint)
        .credentials_provider(SharedCredentialsProvider::new(creds))
        .force_path_style(path_style);
    // if we have a region set then add that to our config
    if let Some(region) = &s3_conf.region {
        // set our region
        s3_config_builder =
            s3_config_builder.region(aws_types::region::Region::new(region.clone()));
    }
    // a test that registered this endpoint gets its virtual-hosted requests routed to it
    #[cfg(feature = "test-support")]
    {
        let authority = s3_conf
            .endpoint
            .split_once("://")
            .map_or(s3_conf.endpoint.as_str(), |(_, rest)| rest)
            .trim_end_matches('/')
            .to_owned();
        if VIRTUAL_HOST_ROUTES.lock().unwrap().contains(&authority) {
            s3_config_builder = s3_config_builder.interceptor(VirtualHostRoute { authority });
        }
    }
    // build our s3 client
    Client::from_conf(s3_config_builder.build())
}

/// Everything the downloads of one S3 backup worker share
#[derive(Clone)]
struct DownloadContext {
    /// The client to download with, shared so its connections are reused
    s3: Client,
    /// The file-system seam to write objects through
    sinks: Arc<dyn SinkFactory>,
    /// How a download that fails for a transient reason is retried
    retry: RetryPolicy,
    /// The component's object index, including the interrupted run's
    index: Arc<ComponentIndex>,
    /// The component's staging directory, where downloads are written until they are complete
    staging: PathBuf,
}

/// The object one download task copies, and what to record if it fails
struct Download {
    /// The bucket the object is in
    bucket: String,
    /// The key of the object to download
    key: String,
    /// Where to write the object
    path: PathBuf,
    /// The component the object belongs to
    component: String,
    /// The table whose rows named the object
    table: &'static str,
    /// The record that names the object, as `<table or collection>:<id>`
    referrer: String,
    /// Whether an object the bucket lacks is a broken link to record and keep, not a failure
    accept_broken_links: bool,
}

/// An object a backed-up record names, with where the backup keeps it
#[derive(Debug, Clone)]
pub struct Named {
    /// The bucket the object is in
    pub bucket: String,
    /// The object's key
    pub key: String,
    /// Where to write the object in the backup
    pub path: PathBuf,
    /// The record that names the object, as `<table or collection>:<id>`
    pub referrer: String,
}

/// A row whose objects couldn't be listed because a column of it doesn't hold what it should
#[derive(Debug, Clone)]
pub struct SkippedRow {
    /// The row, as `<table or collection>:<id>`
    pub referrer: String,
    /// Why the row's objects couldn't be listed
    pub error: String,
}

/// The objects one partition's rows name, and the rows whose objects couldn't be listed
#[derive(Debug, Clone, Default)]
pub struct Crawl {
    /// The objects to back up
    pub downloads: Vec<Named>,
    /// The rows skipped, each recorded without stopping the rest of the partition
    pub skipped: Vec<SkippedRow>,
}

impl From<Vec<Named>> for Crawl {
    /// Wraps the objects of a partition whose rows were all readable
    ///
    /// # Arguments
    ///
    /// * `downloads` - The objects the rows name
    fn from(downloads: Vec<Named>) -> Self {
        // no row was skipped
        Crawl {
            downloads,
            skipped: Vec::new(),
        }
    }
}

/// Streams an object's body into its staging file, syncs it, and checks its length
///
/// Returns the sha256 of the bytes written.
///
/// # Arguments
///
/// * `ctx` - What the worker's downloads share
/// * `body` - The object's body
/// * `partial` - The staging file to write
/// * `expected` - The length the response announced, when it announced one
async fn write_partial(
    ctx: &DownloadContext,
    body: &mut ByteStream,
    partial: &Path,
    expected: Option<u64>,
) -> Result<String, Error> {
    // write the body through the seam, hashing it as it streams
    let mut file = ctx.sinks.create(partial, OBJECT_MODE).await?;
    let mut hasher = Sha256::new();
    let mut received: u64 = 0;
    while let Some(buff) = body.try_next().await? {
        file.write_all(&buff).await?;
        hasher.update(&buff);
        received += buff.len() as u64;
    }
    // flush and sync, so the last write's error isn't lost and the bytes are on disk
    file.sync().await?;
    drop(file);
    // a body shorter than its announced length is cut short, which another attempt can fix
    if let Some(expected) = expected
        && expected != received
    {
        return Err(Error::ShortBody { expected, received });
    }
    // a file shorter than what was written lost bytes on the way to disk
    let on_disk = tokio::fs::metadata(partial).await?.len();
    if on_disk != received {
        return Err(Error::new(format!(
            "{} holds {on_disk} of the {received} bytes written to it",
            partial.display()
        )));
    }
    Ok(HEXLOWER.encode(&hasher.finalize()))
}

/// Returns what landed when a file an interrupted run left at an object's path is whole: its size
/// matches a HEAD of the object and the interrupted run's index, and its sha256 matches the index
///
/// # Arguments
///
/// * `ctx` - What the worker's downloads share
/// * `bucket` - The bucket the object is in
/// * `key` - The key of the object
/// * `path` - The object's final path, where the file was left
async fn keep_left_file(
    ctx: &DownloadContext,
    bucket: &str,
    key: &str,
    path: &Path,
) -> Result<Option<Landed>, Error> {
    // compare the file's size with the object's and with the index's
    let head = ctx.s3.head_object().bucket(bucket).key(key).send().await?;
    let size = head
        .content_length()
        .and_then(|len| u64::try_from(len).ok());
    let on_disk = tokio::fs::metadata(path).await?.len();
    let listed = ctx.index.listed(bucket, key);
    if size != Some(on_disk) || listed.is_none_or(|(len, _)| *len != on_disk) {
        return Ok(None);
    }
    // only then read the whole file to compare its sha256
    let sha256 = file_sha256(path).await?;
    if listed.is_none_or(|(_, listed_sha)| *listed_sha != sha256) {
        return Ok(None);
    }
    Ok(Some(Landed {
        bucket: bucket.to_owned(),
        key: key.to_owned(),
        size: on_disk,
        etag: head.e_tag().map(ToOwned::to_owned),
        sha256,
        path: path.to_path_buf(),
    }))
}

/// Makes one attempt at an object: keeps a matching file an earlier run left, or downloads the
/// object into its staging file and moves it into place
///
/// A left file that isn't kept, including one whose check fails, is removed before the object
/// is downloaded, so a download that then fails leaves nothing at the object's path.
///
/// # Arguments
///
/// * `ctx` - What the worker's downloads share
/// * `bucket` - The bucket the object is in
/// * `key` - The key of the object
/// * `path` - The object's final path
async fn download_attempt(
    ctx: &DownloadContext,
    bucket: &str,
    key: &str,
    path: &Path,
) -> Result<Landed, Error> {
    // a file an interrupted run left is kept only when it is whole, and removed otherwise
    if tokio::fs::try_exists(path).await? {
        match keep_left_file(ctx, bucket, key, path).await {
            Ok(Some(landed)) => return Ok(landed),
            rejected => {
                // remove it first, so no failure below leaves it in the backup
                ctx.sinks.remove(path).await?;
                // a check that failed, because the object is gone for example, fails the attempt
                rejected?;
            }
        }
    }
    // download the object into its staging file
    let mut object = ctx.s3.get_object().bucket(bucket).key(key).send().await?;
    let expected = object
        .content_length()
        .and_then(|len| u64::try_from(len).ok());
    let etag = object.e_tag().map(ToOwned::to_owned);
    let partial = ctx.staging.join(staging_name(bucket, key));
    let written = write_partial(ctx, &mut object.body, &partial, expected).await;
    // move a complete file into place, replacing any file an earlier run left; the directory is
    // synced later with others, before the index lists the object for the manifest
    let moved = match written {
        Ok(sha256) => match ctx.sinks.rename(&partial, path).await {
            Ok(()) => Ok(sha256),
            Err(error) => Err(Error::from(error)),
        },
        Err(error) => Err(error),
    };
    let sha256 = match moved {
        Ok(sha256) => sha256,
        Err(error) => {
            // never leave a staging file behind; a failed removal is noted with the error
            return match ctx.sinks.remove(&partial).await {
                Ok(()) => Err(error),
                Err(remove_error) => Err(Error::new(format!(
                    "{error}; removing {} also failed: {remove_error}",
                    partial.display()
                ))),
            };
        }
    };
    Ok(Landed {
        bucket: bucket.to_owned(),
        key: key.to_owned(),
        size: received_size(expected, path).await?,
        etag,
        sha256,
        path: path.to_path_buf(),
    })
}

/// Returns the size of a landed object: its announced length, or its file's length
///
/// # Arguments
///
/// * `expected` - The length the response announced, when it announced one
/// * `path` - The object's final path
async fn received_size(expected: Option<u64>, path: &Path) -> Result<u64, Error> {
    // a checked download's length equals its announced one
    match expected {
        Some(expected) => Ok(expected),
        None => Ok(tokio::fs::metadata(path).await?.len()),
    }
}

/// Downloads a single object to disk with retries, recording it if it still fails
///
/// Returns what landed, or `None` when the download failed and was recorded.
///
/// # Arguments
///
/// * `ctx` - What the worker's downloads share
/// * `progress` - The bar to report downloaded bytes on
/// * `download` - The object to download
/// * `log` - The run's failure log
async fn download_file(
    ctx: DownloadContext,
    progress: ProgressBar,
    download: Download,
    log: Arc<FailureLog>,
) -> Result<Option<Landed>, Error> {
    let Download {
        bucket,
        key,
        path,
        component,
        table,
        referrer,
        accept_broken_links,
    } = download;
    // make the object's directory, recording a disk that refuses it
    let prepared = match path.parent() {
        Some(parent) => create_object_dir(&ctx.index, parent).await,
        None => Ok(()),
    };
    // download the object, retrying transient failures; disk errors and missing objects are
    // permanent
    let (result, attempts) = match prepared {
        Ok(()) => {
            let retried = ctx
                .retry
                .run(|| download_attempt(&ctx, &bucket, &key, &path))
                .await;
            (retried.result, retried.attempts)
        }
        Err(error) => (Err(error), 1),
    };
    match result {
        Ok(landed) => {
            // count the bytes of a landed object once
            progress.inc(landed.size);
            Ok(Some(landed))
        }
        Err(error) => {
            // an object the bucket lacks is a broken link when the run accepts them, which it
            // records with the record that names it; any other error fails the object
            let record = if accept_broken_links && error.is_missing_object() {
                FailureRecord::broken_link(
                    component,
                    format!("the object is missing from the bucket: {error}"),
                )
                .referrer(referrer)
            } else {
                FailureRecord::failed(component, Operation::Download, error)
            };
            // record the object that wasn't backed up
            let record = record
                .table(table)
                .bucket(bucket)
                .key(key)
                .attempts(attempts);
            log.record(&record).await?;
            Ok(None)
        }
    }
}

/// Creates an object's directory and any missing parents, noting the parent of each directory
/// it creates so the new entries are synced before the index lists the objects under them
///
/// # Arguments
///
/// * `index` - The component's object index, which collects the directories to sync
/// * `dir` - The object's directory
async fn create_object_dir(index: &ComponentIndex, dir: &Path) -> Result<(), Error> {
    // find the directories on the path that don't exist yet, deepest first
    let mut missing = Vec::new();
    let mut next = Some(dir);
    while let Some(candidate) = next {
        if tokio::fs::try_exists(candidate).await? {
            break;
        }
        missing.push(candidate);
        next = candidate.parent();
    }
    // a directory that already exists needs neither creating nor syncing
    if missing.is_empty() {
        return Ok(());
    }
    create_private_dir(dir).await?;
    // each new directory's entry lives in its parent
    index.note_parents(
        missing
            .iter()
            .filter_map(|created| created.parent())
            .map(Path::to_path_buf),
    );
    Ok(())
}

/// Syncs directories objects were moved into, recording each one that can't be synced
///
/// # Arguments
///
/// * `sinks` - The file-system seam to sync through
/// * `dirs` - The directories to sync
/// * `component` - The component whose objects they hold
/// * `table` - The table whose rows named the objects
/// * `log` - The run's failure log
pub async fn sync_dirs(
    sinks: &dyn SinkFactory,
    dirs: Vec<PathBuf>,
    component: &str,
    table: &str,
    log: &FailureLog,
) -> Result<(), Error> {
    // a directory that can't be synced may lose the objects moved into it in a crash
    for dir in dirs {
        if let Err(error) = sinks.sync_dir(&dir).await {
            let record = FailureRecord::failed(
                component,
                Operation::WriteArchive,
                format!("{} couldn't be synced to disk: {error}", dir.display()),
            )
            .table(table)
            .key(dir.to_string_lossy());
            log.record(&record).await?;
        }
    }
    Ok(())
}

/// What an S3 backup worker is told to copy
#[derive(Debug, Clone)]
pub enum S3Order {
    /// The objects the rows of one archive name, by the archive's map
    Archive(PathBuf),
    /// Objects named by key, outside any archive's rows
    Keys {
        /// The bucket role the keys are in
        role: &'static str,
        /// The bucket the keys are in
        bucket: String,
        /// The keys to copy, each with the record that names it as `<collection>:<id>`
        keys: Vec<(String, String)>,
    },
}

impl OrderKey for S3Order {
    /// Names the archive map, or the first named key, a worker was copying objects from
    ///
    /// # Arguments
    ///
    /// * `record` - The record to name the order in
    fn name_in(&self, record: FailureRecord) -> FailureRecord {
        // a map order names its map, and a keys order its bucket and first key
        match self {
            S3Order::Archive(map) => map.name_in(record),
            S3Order::Keys { bucket, keys, .. } => {
                let record = record.bucket(bucket);
                match keys.first() {
                    Some((key, _)) => record.key(key),
                    None => record,
                }
            }
        }
    }
}

/// A worker that copies the S3 objects of the orders it receives to disk
pub struct S3BackupWorker<S: S3Backup> {
    /// The type we are backing up
    phantom: PhantomData<S>,
    /// The config for this Thorium cluster
    conf: Conf,
    /// The kanal channel workers should send backup updates over
    updates: AsyncSender<MonitorUpdate>,
    /// The progress bar to track progress with
    progress: ProgressBar,
    /// the path to our object directory
    object_path: PathBuf,
    /// What this worker's downloads share
    ctx: DownloadContext,
    /// The downloads in flight
    pub active: FuturesUnordered<JoinHandle<Result<Option<Landed>, Error>>>,
    /// What every worker of the run shares, including the bound on downloads in flight
    shared: S3BackupShared,
    /// Track the number of objects we have backed up
    pub backed_up: u64,
    /// The component this worker backs up
    kind: BackupComponents,
    /// The component's name, as records name it
    component: String,
    /// The run's failure log
    log: Arc<FailureLog>,
}

/// What every worker of every S3 component of one backup run shares
#[derive(Clone)]
pub struct S3BackupShared {
    /// The config of the cluster being backed up, which the client is built from
    conf: Arc<Conf>,
    /// The client to download with, built when an S3 component first needs it, so a backup of
    /// tables alone never builds one
    s3: Arc<OnceLock<Client>>,
    /// The file-system seam to write downloaded objects through
    pub sinks: Arc<dyn SinkFactory>,
    /// How a download that fails for a transient reason is retried
    pub retry: RetryPolicy,
    /// The run-wide bound on downloads in flight
    pub permits: Arc<Semaphore>,
    /// The objects the running S3 component has started downloading, by a 128-bit hash of
    /// bucket and key, so an object several rows name is downloaded once; emptied when the next
    /// component starts, since no two components read the same bucket
    pub claimed: Arc<Mutex<HashSet<u128>>>,
    /// Whether an object the bucket lacks is recorded as a broken link and the run goes on
    /// (`--accept-broken-links`), rather than failing the object; set before the first download
    accept_broken_links: Arc<AtomicBool>,
}

impl S3BackupShared {
    /// Builds what the S3 components of one backup run share
    ///
    /// # Arguments
    ///
    /// * `conf` - The config of the cluster being backed up
    /// * `sinks` - The file-system seam to write downloaded objects through
    /// * `retry` - How a download that fails for a transient reason is retried
    /// * `concurrency` - The most downloads in flight at once across the run
    #[must_use]
    pub fn new(
        conf: &Conf,
        sinks: Arc<dyn SinkFactory>,
        retry: RetryPolicy,
        concurrency: usize,
    ) -> Self {
        // no client is built and nothing is claimed yet
        S3BackupShared {
            conf: Arc::new(conf.clone()),
            s3: Arc::default(),
            sinks,
            retry,
            permits: Arc::new(Semaphore::new(concurrency)),
            claimed: Arc::default(),
            accept_broken_links: Arc::default(),
        }
    }

    /// Makes the run record an object the bucket lacks as a broken link, and go on, rather than
    /// fail the object
    ///
    /// # Arguments
    ///
    /// * `accept` - Whether the run accepts broken links
    pub fn accept_broken_links(&self, accept: bool) {
        // every clone of the shared state sees the setting
        self.accept_broken_links.store(accept, Ordering::Relaxed);
    }

    /// Returns whether the run accepts broken links
    #[must_use]
    pub fn accepts_broken_links(&self) -> bool {
        // read the setting every worker shares
        self.accept_broken_links.load(Ordering::Relaxed)
    }

    /// Returns the run's client, building it the first time
    #[must_use]
    pub fn client(&self) -> Client {
        // every component shares the one client, so its connections are reused
        self.s3
            .get_or_init(|| s3_client(&self.conf, self.conf.thorium.s3.use_path_style))
            .clone()
    }

    /// Forgets every object claimed so far, so the set holds one component's objects at a time
    pub fn forget_claims(&self) {
        // release the set's memory along with its entries
        *self
            .claimed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = HashSet::new();
    }

    /// Claims an object for this run, returning false when the run already claimed it
    ///
    /// # Arguments
    ///
    /// * `bucket` - The object's bucket
    /// * `key` - The object's key
    #[must_use]
    pub fn claim(&self, bucket: &str, key: &str) -> bool {
        // the first 128 bits of the object's digest identify it
        let digest = object_digest(bucket, key);
        let mut id = [0_u8; 16];
        id.copy_from_slice(&digest[..16]);
        // the first claim of an object wins; later rows naming it are skipped
        self.claimed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(u128::from_le_bytes(id))
    }
}

impl<S: S3Backup> S3BackupWorker<S> {
    /// Create a new backup worker
    ///
    /// # Arguments
    ///
    /// * `conf` - The config for this Thorium cluster
    /// * `updates` - The channel to send partition archive updates on
    /// * `progress` - The progress bar for this worker
    /// * `object_path` - The path to our object directory; downloads are staged in the
    ///   `staging` directory beside it
    /// * `shared` - What every worker of the run shares
    /// * `index` - The component's object index
    /// * `kind` - The component this worker backs up
    /// * `log` - The run's failure log
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        conf: &Conf,
        updates: &AsyncSender<MonitorUpdate>,
        progress: ProgressBar,
        object_path: &Path,
        shared: &S3BackupShared,
        index: &Arc<ComponentIndex>,
        kind: BackupComponents,
        log: &Arc<FailureLog>,
    ) -> Self {
        // build our worker
        S3BackupWorker {
            phantom: PhantomData,
            conf: conf.clone(),
            updates: updates.clone(),
            progress,
            object_path: object_path.to_path_buf(),
            ctx: DownloadContext {
                s3: shared.client(),
                sinks: shared.sinks.clone(),
                retry: shared.retry,
                index: index.clone(),
                staging: object_path.with_file_name(STAGING_DIR),
            },
            active: FuturesUnordered::new(),
            shared: shared.clone(),
            backed_up: 0,
            kind,
            component: kind.to_string(),
            log: log.clone(),
        }
    }

    /// Counts one finished download, recording a download task that panicked
    ///
    /// # Arguments
    ///
    /// * `join` - The finished download task
    async fn finish_download(
        &mut self,
        join: Result<Result<Option<Landed>, Error>, tokio::task::JoinError>,
    ) -> Result<(), Error> {
        // a failed download was already recorded; a panicked one is recorded here
        let landed = match join {
            Ok(result) => result?,
            Err(error) => {
                let record = FailureRecord::failed(
                    &self.component,
                    Operation::Download,
                    format!("download task panicked: {error}"),
                )
                .table(S::name());
                self.log.record(&record).await?;
                None
            }
        };
        // index a download that landed, recording one whose line can't be written
        let landed = match landed {
            Some(landed) => match self.ctx.index.add(&landed).await {
                Ok(()) => Some(landed),
                Err(error) => {
                    let record = FailureRecord::failed(
                        &self.component,
                        Operation::WriteArchive,
                        format!(
                            "the object landed but its index line couldn't be written: {error}"
                        ),
                    )
                    .table(S::name())
                    .bucket(&landed.bucket)
                    .key(&landed.key);
                    self.log.record(&record).await?;
                    None
                }
            },
            None => None,
        };
        // sync the directories objects landed in once enough of them are waiting
        if let Some(dirs) = landed
            .as_ref()
            .and_then(|landed| self.ctx.index.note_dir(landed))
        {
            sync_dirs(
                self.ctx.sinks.as_ref(),
                dirs,
                &self.component,
                S::name(),
                &self.log,
            )
            .await?;
        }
        // count a download that landed
        if let Some(landed) = landed {
            // update our backed up count
            self.backed_up += 1;
            self.log.handled(&self.component, 1);
            // set our new message
            self.progress.set_message(self.backed_up.to_string());
            // send an update to our monitor; a closed monitor only loses progress output
            let update = MonitorUpdate::Update {
                items: 1,
                bytes: landed.size,
            };
            let _ = self.updates.send(update).await;
        }
        Ok(())
    }

    /// Counts every download that already finished, without waiting for the others
    async fn drain_finished(&mut self) -> Result<(), Error> {
        // take finished tasks until the next one is still running or none are left
        while let Some(Some(join)) = self.active.next().now_or_never() {
            self.finish_download(join).await?;
        }
        Ok(())
    }

    /// Wait for all active tasks to complete
    pub async fn flush_active(&mut self) -> Result<(), Error> {
        // wait for all active tasks to complete
        while let Some(join) = self.active.next().await {
            self.finish_download(join).await?;
        }
        Ok(())
    }

    /// Copy the objects of every order to disk: the objects an archive's rows name, or the
    /// objects an order names by key
    ///
    /// # Arguments
    ///
    /// * `orders` - The orders to handle
    pub async fn backup(mut self, orders: Orders<S3Order>) -> Result<Self, Error>
    where
        <S as Archive>::Archived:
            for<'a> bytecheck::CheckBytes<DefaultValidator<'a>> + std::fmt::Debug,
    {
        // handle messages in our channel until its closed or the run is stopped
        loop {
            // take no new archive once --abort-on-failure stopped the run
            if self.log.stopped() {
                break;
            }
            // get the next message in the queue
            let order = match orders.recv().await {
                Ok(order) => order,
                Err(kanal::ReceiveError::Closed | kanal::ReceiveError::SendClosed) => break,
            };
            match order {
                // back up the objects this archive names, recording an archive that can't be read
                S3Order::Archive(map_path) => {
                    if let Err(error) = self.backup_archive(&map_path).await {
                        let record =
                            FailureRecord::failed(&self.component, Operation::ReadArchive, error)
                                .table(S::name())
                                .key(map_path.to_string_lossy());
                        self.log.record(&record).await?;
                    }
                }
                // back up the objects the order names
                S3Order::Keys { role, bucket, keys } => {
                    for (key, referrer) in keys {
                        let path = self
                            .object_path
                            .join(object_relative_path(self.kind, role, &key));
                        let named = Named {
                            bucket: bucket.clone(),
                            key,
                            path,
                            referrer,
                        };
                        if !self.start_download(named).await? {
                            break;
                        }
                    }
                }
            }
        }
        // wait for all active downloads to finish
        self.flush_active().await?;
        Ok(self)
    }

    /// Starts downloading every object one archive's rows name
    ///
    /// Each download takes a permit of the run-wide bound before it starts, so no more objects
    /// are open at once than the bound allows, however many a partition names. An object the
    /// run already claimed, because another row names it too, isn't downloaded again.
    ///
    /// # Arguments
    ///
    /// * `map_path` - The map of the archive to read
    async fn backup_archive(&mut self, map_path: &Path) -> Result<(), Error>
    where
        <S as Archive>::Archived:
            for<'a> bytecheck::CheckBytes<DefaultValidator<'a>> + std::fmt::Debug,
    {
        // build our reader for this archive
        let mut reader = ArchiveReader::new(map_path.to_path_buf()).await?;
        // crawl over the partitions in this archive and back up its s3 data
        while let Some(backup_slice) = reader.next().await? {
            // backup this partitions s3 data to disk
            let crawl = S::paths(
                &self.conf,
                &self.object_path,
                OBJECT_PREFIX_CHARS,
                backup_slice,
            )
            .await?;
            // record each row whose objects couldn't be listed, naming the row
            for row in crawl.skipped {
                let record = FailureRecord::skipped(&self.component, row.error)
                    .table(S::name())
                    .referrer(row.referrer);
                self.log.record(&record).await?;
            }
            for named in crawl.downloads {
                // stop at the first object once --abort-on-failure stopped the run
                if !self.start_download(named).await? {
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// Starts downloading one object, unless the run already claimed it, returning false once
    /// `--abort-on-failure` stopped the run
    ///
    /// The download takes a permit of the run-wide bound before it starts.
    ///
    /// # Arguments
    ///
    /// * `named` - The object, where to write it, and the record that names it
    async fn start_download(&mut self, named: Named) -> Result<bool, Error> {
        // take the object apart
        let Named {
            bucket,
            key,
            path,
            referrer,
        } = named;
        // stop starting downloads once --abort-on-failure stopped the run
        if self.log.stopped() {
            return Ok(false);
        }
        // a key that can't be kept as a path under objects/ is recorded instead of written
        if !keepable_key(&key) {
            let record = FailureRecord::failed(
                &self.component,
                Operation::Download,
                "the key can't be kept in a backup: it is empty, starts or ends with `/`, or holds \
                 an empty, `.`, or `..` segment",
            )
            .table(S::name())
            .bucket(&bucket)
            .key(&key);
            self.log.record(&record).await?;
            return Ok(true);
        }
        // download each object once, however many rows name it
        if !self.shared.claim(&bucket, &key) {
            return Ok(true);
        }
        // wait for room under the run-wide bound before anything is opened
        let permit = self
            .shared
            .permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|error| Error::new(format!("the download bound closed: {error}")))?;
        let download = Download {
            bucket,
            key,
            path,
            component: self.component.clone(),
            table: S::name(),
            referrer,
            accept_broken_links: self.shared.accepts_broken_links(),
        };
        // start downloading this file, holding the permit until it is done
        let (ctx, progress, log) = (self.ctx.clone(), self.progress.clone(), self.log.clone());
        let handle = tokio::spawn(async move {
            let landed = download_file(ctx, progress, download, log).await;
            drop(permit);
            landed
        });
        self.active.push(handle);
        // count the downloads that already finished
        self.drain_finished().await?;
        Ok(true)
    }

    /// Shutdown this worker
    pub fn shutdown(self) {
        // shutdown our progress bar
        self.progress.finish();
    }
}

/// A component whose S3 objects thoradm copies to disk, found by crawling the rows of a table's
/// archives or named by keys outside any archive
#[async_trait::async_trait]
pub trait S3Backup: Utils + std::fmt::Debug + 'static + Send + Archive {
    /// Get the s3 urls and where to write them off to disk at
    ///
    /// # Arguments
    ///
    /// * `conf` - The config for this Thorium cluster
    /// * `root` - The root path to write objects too
    /// * `chars` - The number of characters to use in order to partition this data on disk
    /// * `buffer` - The slice of data to deserialize and crawl for s3 backup objects
    ///
    /// Returns each object with the record that names it, and the rows whose objects couldn't be
    /// listed.
    async fn paths<'a>(
        conf: &'a Conf,
        root: &PathBuf,
        chars: usize,
        buffer: &[u8],
    ) -> Result<Crawl, Error>;

    /// The directory of the table component whose archives this component reads, when it isn't
    /// the component's own
    fn archives_dir() -> Option<&'static str> {
        // most components read the archives of the table they are named for
        None
    }
}

/// A sub worker for this upload worker
pub struct UploadSubWorker<R: S3Restore> {
    /// The type we are restoring; held as a function pointer so the worker is `Sync`
    phantom: PhantomData<fn() -> R>,
    /// The Thorium config for the cluster we are restoring
    conf: Conf,
    /// The component's `objects/` directory in the backup, which object keys are relative to
    objects: PathBuf,
    /// How an upload that fails for a transient reason is retried
    retry: RetryPolicy,
    /// The s3 client to use
    s3: Client,
    /// The progress bar to track progress with
    progress: ProgressBar,
    /// The kanal channel workers should send backup updates over
    updates: AsyncSender<S3MonitorUpdate>,
    /// The buffer to store our chunks in
    buffer: BytesMut,
    /// Track the parts we have uploaded
    parts: Vec<CompletedPart>,
    /// The component this sub worker restores
    component: String,
    /// The run's failure log
    log: Arc<FailureLog>,
}

impl<R: S3Restore> UploadSubWorker<R> {
    /// Create a new upload sub worker
    ///
    /// # Arguments
    ///
    /// * `shared` - What every upload of the run shares
    /// * `objects` - The component's `objects/` directory in the backup
    /// * `progress` - The progress bar to track progress with
    /// * `updates` - The channel to send monitor updates over
    /// * `component` - The component this sub worker restores
    /// * `log` - The run's failure log
    pub fn new(
        shared: &S3RestoreShared,
        objects: &Path,
        progress: &ProgressBar,
        updates: &AsyncSender<S3MonitorUpdate>,
        component: &str,
        log: &Arc<FailureLog>,
    ) -> Self {
        // create our upload sub worker
        let s3 = shared.client();
        UploadSubWorker {
            phantom: PhantomData,
            conf: shared.conf.as_ref().clone(),
            objects: objects.to_path_buf(),
            retry: shared.retry,
            s3: s3.clone(),
            progress: progress.clone(),
            updates: updates.clone(),
            buffer: BytesMut::with_capacity(5_242_880),
            parts: Vec::with_capacity(10),
            component: component.to_owned(),
            log: log.clone(),
        }
    }

    /// Stream a single object to s3
    async fn stream_object(
        &mut self,
        path: &PathBuf,
        bucket: &str,
        key: &str,
        upload_id: &str,
    ) -> Result<(), Error> {
        // make sure we do not have any lingering parts from old uploads
        self.parts.clear();
        // open a file handle to this file
        let mut stream = ReaderStream::new(File::open(path).await?);
        // track the current part numbers
        let mut part_num = 1;
        // start reading this file into our buffer
        while let Some(data) = stream.next().await {
            // get the bytes we read
            let data = data?;
            // write this data to our buffer
            self.buffer.extend_from_slice(&data);
            // determine how much data we have to send to s3
            if self.buffer.remaining() >= 5_242_880 {
                // build our byte stream
                let byte_stream = ByteStream::from(SdkBody::from(&self.buffer[..]));
                // write this buffer to s3
                let part = self
                    .s3
                    .upload_part()
                    .bucket(bucket)
                    .key(key)
                    .upload_id(upload_id)
                    .body(byte_stream)
                    .part_number(part_num)
                    .send()
                    .await?;
                // add this chunk to our parts list
                self.parts.push(
                    CompletedPart::builder()
                        .e_tag(part.e_tag.unwrap_or_default())
                        .part_number(part_num)
                        .build(),
                );
                // increment our progress bar
                self.progress.inc(self.buffer.remaining() as u64);
                // build the update to send to our global monitor
                let update = S3MonitorUpdate::Update(self.buffer.remaining());
                // send an update to the global bar
                self.updates.send(update).await?;
                // reset our packable and writable number of bytes to 0
                self.buffer.clear();
                // increment our part number
                part_num += 1;
            }
        }
        // build our byte stream
        let byte_stream = ByteStream::from(SdkBody::from(&self.buffer[..]));
        // write this buffer to s3
        let part = self
            .s3
            .upload_part()
            .bucket(bucket)
            .key(key)
            .upload_id(upload_id)
            .body(byte_stream)
            .part_number(part_num)
            .send()
            .await?;
        // add this chunk to our parts list
        self.parts.push(
            CompletedPart::builder()
                .e_tag(part.e_tag.unwrap_or_default())
                .part_number(part_num)
                .build(),
        );
        // build our complete multipart upload object
        let completed_parts = CompletedMultipartUpload::builder()
            .set_parts(Some(self.parts.clone()))
            .build();
        // finish this multipart upload
        self.s3
            .complete_multipart_upload()
            .bucket(bucket)
            .key(key)
            .multipart_upload(completed_parts)
            .upload_id(upload_id)
            .send()
            .await?;
        // increment our progress bar
        self.progress.inc(self.buffer.remaining() as u64);
        // build the update to send to our global monitor
        let update = S3MonitorUpdate::Update(self.buffer.remaining());
        // send an update to the global bar
        self.updates.send(update).await?;
        // reset our packable and writable number of bytes to 0
        self.buffer.clear();
        Ok(())
    }

    /// Records one object that couldn't be restored
    ///
    /// # Arguments
    ///
    /// * `operation` - The operation that failed
    /// * `bucket` - The bucket the object belongs in, when known
    /// * `key` - The object's key, or the backup file's path when no key could be parsed
    /// * `error` - The error the operation returned
    /// * `attempts` - How many attempts were made
    async fn record(
        &self,
        operation: Operation,
        bucket: Option<&str>,
        key: &str,
        error: String,
        attempts: u32,
    ) -> Result<(), Error> {
        // name the table, bucket, and key of the object
        let mut record = FailureRecord::failed(&self.component, operation, error)
            .table(R::name())
            .key(key)
            .attempts(attempts);
        if let Some(bucket) = bucket {
            record = record.bucket(bucket);
        }
        self.log.record(&record).await
    }

    /// Makes one attempt to upload a file to s3 as a multipart upload, aborting the upload by
    /// bucket, key, and upload id when it fails
    ///
    /// # Arguments
    ///
    /// * `path` - The backed up file to upload
    /// * `bucket` - The bucket to upload it to
    /// * `key` - Its key
    async fn attempt_upload(
        &mut self,
        path: &PathBuf,
        bucket: &str,
        key: &str,
    ) -> Result<(), Error> {
        // initiate a multipart upload to s3
        let init = self
            .s3
            .create_multipart_upload()
            .bucket(bucket)
            .key(key)
            .content_type(R::content_type(key))
            .send()
            .await?;
        // get our upload id
        let Some(upload_id) = init.upload_id() else {
            return Err(Error::new("no upload id was returned"));
        };
        // upload this file to s3
        let Err(error) = self.stream_object(path, bucket, key, upload_id).await else {
            return Ok(());
        };
        // clear our buffer, and abort the upload so no parts are left behind
        self.buffer.clear();
        if let Err(abort) = self
            .s3
            .abort_multipart_upload()
            .bucket(bucket)
            .key(key)
            .upload_id(upload_id)
            .send()
            .await
        {
            // an upload that can't be aborted keeps its parts in the bucket, so it is recorded
            let abort = format!("failed to abort upload {upload_id}: {}", Error::from(abort));
            self.record(Operation::AbortUpload, Some(bucket), key, abort, 1)
                .await?;
        }
        Err(error)
    }

    /// Upload a file to s3, retrying transient failures and recording it if it still fails
    ///
    /// Returns whether the object was restored.
    ///
    /// # Arguments
    ///
    /// * `path` - The backed up file to upload
    pub async fn upload_helper(&mut self, path: &PathBuf) -> Result<bool, Error> {
        // get the bucket and key to write to, recording a file that names no object
        let (bucket, key) = match R::parse(path, &self.objects, &self.conf) {
            Ok(parsed) => parsed,
            Err(error) => {
                self.record(
                    Operation::Upload,
                    None,
                    &path.to_string_lossy(),
                    error.to_string(),
                    1,
                )
                .await?;
                return Ok(false);
            }
        };
        // upload it, retrying transient failures after growing delays
        let mut attempt = 1;
        loop {
            match self.attempt_upload(path, &bucket, &key).await {
                Ok(()) => return Ok(true),
                Err(error) if error.is_transient() && attempt < self.retry.attempts => {
                    tokio::time::sleep(self.retry.delay(attempt)).await;
                    attempt += 1;
                }
                Err(error) => {
                    // record the object that wasn't restored
                    self.record(
                        Operation::Upload,
                        Some(&bucket),
                        &key,
                        error.to_string(),
                        attempt,
                    )
                    .await?;
                    return Ok(false);
                }
            }
        }
    }

    /// Uploads a file to s3, returning this sub worker and whether the object was restored
    ///
    /// # Arguments
    ///
    /// * `path` - The backed up file to upload
    pub async fn upload(mut self, path: PathBuf) -> (Self, Result<bool, Error>) {
        // try to upload this file; failures are recorded inside
        let restored = self.upload_helper(&path).await;
        (self, restored)
    }
}

/// An upload sub worker returned from its task, with whether its object was restored
type FinishedUpload<R> = (UploadSubWorker<R>, Result<bool, Error>);

/// What every worker of every S3 component of one restore run shares
#[derive(Clone)]
pub struct S3RestoreShared {
    /// The config of the cluster being restored, which the client is built from
    conf: Arc<Conf>,
    /// The client to upload with, built when an S3 component first needs it, so a restore of
    /// tables alone never builds one
    s3: Arc<OnceLock<Client>>,
    /// How an upload that fails for a transient reason is retried
    pub retry: RetryPolicy,
    /// The run-wide bound on uploads in flight
    pub permits: Arc<Semaphore>,
}

impl S3RestoreShared {
    /// Builds what the S3 components of one restore run share
    ///
    /// # Arguments
    ///
    /// * `conf` - The config of the cluster being restored
    /// * `retry` - How an upload that fails for a transient reason is retried
    /// * `concurrency` - The most uploads in flight at once across the run
    #[must_use]
    pub fn new(conf: &Conf, retry: RetryPolicy, concurrency: usize) -> Self {
        // no client is built yet
        S3RestoreShared {
            conf: Arc::new(conf.clone()),
            s3: Arc::default(),
            retry,
            permits: Arc::new(Semaphore::new(concurrency)),
        }
    }

    /// Returns the run's client, built the first time and addressing buckets the way the config
    /// says, as a backup's does
    #[must_use]
    pub fn client(&self) -> Client {
        // every component shares the one client, so its connections are reused
        self.s3
            .get_or_init(|| s3_client(&self.conf, self.conf.thorium.s3.use_path_style))
            .clone()
    }
}

/// A worker that uploads the backed up objects of the orders it receives to S3
pub struct S3RestoreWorker<R: S3Restore> {
    /// The type we are backing up
    phantom: PhantomData<R>,
    /// What every upload of the run shares
    shared: S3RestoreShared,
    /// The component's `objects/` directory in the backup
    objects: PathBuf,
    /// The kanal channel workers should send backup updates over
    updates: AsyncSender<S3MonitorUpdate>,
    /// The progress bar to track progress with
    progress: ProgressBar,
    /// The workers waiting for active tasks
    workers: Vec<UploadSubWorker<R>>,
    /// The currently active downloads to monitor
    active: FuturesOrdered<JoinHandle<FinishedUpload<R>>>,
    /// Track the number of objects we have restored
    restored: u64,
    /// The component this worker restores
    component: String,
    /// The run's failure log
    log: Arc<FailureLog>,
}

impl<R: S3Restore> S3RestoreWorker<R> {
    /// Create a new restore worker
    ///
    /// # Arguments
    ///
    /// * `shared` - What every upload of the run shares
    /// * `objects` - The component's `objects/` directory in the backup
    /// * `updates` - The channel to send partition archive updates on
    /// * `progress` - The progress bar for this worker
    /// * `component` - The component this worker restores
    /// * `log` - The run's failure log
    pub fn new(
        shared: &S3RestoreShared,
        objects: &Path,
        updates: &AsyncSender<S3MonitorUpdate>,
        progress: ProgressBar,
        component: &str,
        log: &Arc<FailureLog>,
    ) -> Self {
        // create the sub workers that upload objects
        let workers = (0..10)
            .map(|_| UploadSubWorker::new(shared, objects, &progress, updates, component, log))
            .collect();
        // build our worker
        S3RestoreWorker {
            phantom: PhantomData,
            shared: shared.clone(),
            objects: objects.to_path_buf(),
            updates: updates.clone(),
            progress,
            workers,
            active: FuturesOrdered::new(),
            restored: 0,
            component: component.to_owned(),
            log: log.clone(),
        }
    }

    /// Builds a fresh upload sub worker
    fn sub_worker(&self) -> UploadSubWorker<R> {
        // share this worker's client, bar, and failure log
        UploadSubWorker::new(
            &self.shared,
            &self.objects,
            &self.progress,
            &self.updates,
            &self.component,
            &self.log,
        )
    }

    /// Counts one finished upload and returns its sub worker for reuse
    ///
    /// # Arguments
    ///
    /// * `join` - The finished upload task
    async fn finish_upload(
        &mut self,
        join: Result<FinishedUpload<R>, tokio::task::JoinError>,
    ) -> Result<UploadSubWorker<R>, Error> {
        // a panicked upload is recorded and its sub worker replaced
        let (sub_worker, restored) = match join {
            Ok(finished) => finished,
            Err(error) => {
                let record = FailureRecord::failed(
                    &self.component,
                    Operation::Upload,
                    format!("upload task panicked: {error}"),
                )
                .table(R::name());
                self.log.record(&record).await?;
                return Ok(self.sub_worker());
            }
        };
        // only an upload that landed counts as restored; failed ones were recorded
        if restored? {
            self.restored += 1;
            self.log.handled(&self.component, 1);
            self.progress.set_message(self.restored.to_string());
        }
        Ok(sub_worker)
    }

    /// Check on our pending futures and wait if we have enough in flight
    pub async fn check_active(&mut self) -> Result<UploadSubWorker<R>, Error> {
        // reuse an idle sub worker when there is one
        if let Some(worker) = self.workers.pop() {
            return Ok(worker);
        }
        // otherwise wait for an upload to finish and reuse its sub worker
        match self.active.next().await {
            Some(join) => self.finish_upload(join).await,
            None => Ok(self.sub_worker()),
        }
    }

    /// Wait for all active tasks to complete
    pub async fn flush_active(&mut self) -> Result<(), Error> {
        // wait for all active tasks to complete
        while let Some(join) = self.active.next().await {
            let sub_worker = self.finish_upload(join).await?;
            self.workers.push(sub_worker);
        }
        Ok(())
    }

    /// Start restoring objects
    ///
    /// # Arguments
    ///
    /// * `orders` - The s3 objects to restore
    pub async fn restore(mut self, orders: Orders<PathBuf>) -> Result<Self, Error> {
        // handle messages in our channel until its closed or the run is stopped
        loop {
            // take no new object once --abort-on-failure stopped the run
            if self.log.stopped() {
                break;
            }
            // get the next message in the queue
            let path = match orders.recv().await {
                Ok(path) => path,
                Err(kanal::ReceiveError::Closed | kanal::ReceiveError::SendClosed) => break,
            };
            // wait for a sub worker and for room under the run-wide bound on uploads
            let worker = self.check_active().await?;
            let permit = self
                .shared
                .permits
                .clone()
                .acquire_owned()
                .await
                .map_err(|error| Error::new(format!("the upload bound closed: {error}")))?;
            // an upload that finished while this one waited may have stopped the run
            if self.log.stopped() {
                self.workers.push(worker);
                break;
            }
            // start uploading this file, holding the permit until it ends
            let handle = tokio::spawn(async move {
                let finished = worker.upload(path).await;
                drop(permit);
                finished
            });
            // add this to our futures
            self.active.push_back(handle);
        }
        // wait for all active downloads to finish
        self.flush_active().await?;
        // close our progress bar
        self.progress.finish();
        Ok(self)
    }
}

/// The trait for restoring s3 objects
pub trait S3Restore: Utils + std::fmt::Debug + 'static + Send {
    /// Get the bucket and key of a backed-up object from where the backup keeps it
    ///
    /// # Arguments
    ///
    /// * `path` - The path to the file we are restoring
    /// * `objects` - The component's `objects/` directory, which `path` is under
    /// * `conf` - The Thorium for this cluster
    fn parse(path: &Path, objects: &Path, conf: &Conf) -> Result<(String, String), Error>;

    /// Returns the content type a restored object is uploaded with; the backup keeps only an
    /// object's bytes, so this is plain bytes unless the component knows better
    ///
    /// # Arguments
    ///
    /// * `_key` - The object's key
    #[must_use]
    fn content_type(_key: &str) -> &'static str {
        // a type the backup doesn't record is restored as plain bytes
        "application/octet-stream"
    }
}

/// Splits where a backup keeps an object into its bucket role, for a layout that keeps roles
/// apart, and its key, the inverse of [`object_relative_path`]
///
/// The path is read relative to the component's `objects/` directory, so it parses the same from
/// any backup directory: relative, `./`, nested, or absolute.
///
/// # Arguments
///
/// * `path` - The path to the file
/// * `objects` - The component's `objects/` directory, which `path` is under
/// * `with_role` - Whether the layout starts with the bucket role (the S3 ids)
pub fn object_key(
    path: &Path,
    objects: &Path,
    with_role: bool,
) -> Result<(Option<String>, String), Error> {
    // read the path under objects/, segment by segment
    let relative = path.strip_prefix(objects)?;
    let mut segments = relative.components().map(|component| {
        component
            .as_os_str()
            .to_str()
            .map(ToOwned::to_owned)
            .ok_or_else(|| Error::new(format!("{} isn't valid UTF-8", path.display())))
    });
    // the bucket role comes first where the layout keeps roles apart
    let role = if with_role {
        let role = segments
            .next()
            .ok_or_else(|| Error::new(format!("{} names no bucket role", path.display())))??;
        Some(role)
    } else {
        None
    };
    // then the directory spreading objects, and then the key, whose segments are its directories
    segments
        .next()
        .ok_or_else(|| Error::new(format!("{} names no object", path.display())))??;
    let key = segments.collect::<Result<Vec<String>, Error>>()?.join("/");
    if key.is_empty() {
        return Err(Error::new(format!("{} names no object", path.display())));
    }
    Ok((role, key))
}

#[cfg(test)]
mod tests {
    use super::{S3Restore, keepable_key, object_relative_path};
    use crate::args::BackupComponents;
    use crate::backup::graphics::Graphic;
    use crate::backup::tables::{Comment, Output, S3Id};
    use std::path::{Path, PathBuf};
    use thorium::Conf;

    /// The backup directories a restore may be given: relative, `./`, nested, and absolute
    const ROOTS: [&str; 4] = [
        "ThoriumBackups",
        "./ThoriumBackups",
        "a/b",
        "/tmp/thoradm-tests/ThoriumBackups",
    ];

    /// Loads the test config, which names a bucket for every role
    fn conf() -> Conf {
        // the template the integration tests start from
        Conf::new("../api/tests/thorium-testing.yml").expect("failed to load the test config")
    }

    /// Returns where a backup under a root keeps an object of a table
    ///
    /// # Arguments
    ///
    /// * `root` - The backup directory
    /// * `table` - The table directory holding the objects
    /// * `rest` - The object's path under `objects/`
    fn object_path(root: &str, table: &str, rest: &str) -> PathBuf {
        // the object layout is the same under any root
        Path::new(root).join(table).join("objects").join(rest)
    }

    /// Returns a table's `objects/` directory in a backup under a root
    ///
    /// # Arguments
    ///
    /// * `root` - The backup directory
    /// * `table` - The table directory holding the objects
    fn objects(root: &str, table: &str) -> PathBuf {
        // every component keeps its objects there
        Path::new(root).join(table).join("objects")
    }

    /// Every S3 parser gives the same bucket and key for an object whatever the backup directory
    #[test]
    fn objects_parse_to_the_same_key_from_any_backup_path() {
        // a config naming every bucket, and the ids and hashes the paths hold
        let conf = conf();
        let id = "4f2a1b3c-5d6e-4f70-8192-a3b4c5d6e7f8";
        let sha = "deb3406511c00c1abcfd0a65199b299735e31dc825e3137fb69e2ca7d4ccdffc";
        let comment = "0b1c2d3e-4f50-4617-8293-a4b5c6d7e8f9";
        let attachment = "9f8e7d6c-5b4a-4392-8180-f7e6d5c4b3a2";
        for root in ROOTS {
            // a sample and a repo of the s3 ids
            let path = object_path(root, "s3_ids", &format!("files/4f2a1b/{id}"));
            assert_eq!(
                S3Id::parse(&path, &objects(root, "s3_ids"), &conf).unwrap(),
                (conf.thorium.files.bucket.clone(), id.to_owned()),
                "{root}"
            );
            let path = object_path(root, "s3_ids", &format!("repos/4f2a1b/{id}"));
            assert_eq!(
                S3Id::parse(&path, &objects(root, "s3_ids"), &conf).unwrap(),
                (conf.thorium.repos.bucket.clone(), id.to_owned()),
                "{root}"
            );
            // a comment attachment
            let path = object_path(
                root,
                "comments",
                &format!("deb340/{sha}/{comment}/{attachment}"),
            );
            assert_eq!(
                Comment::parse(&path, &objects(root, "comments"), &conf).unwrap(),
                (
                    conf.thorium.attachments.bucket.clone(),
                    format!("{sha}/{comment}/{attachment}")
                ),
                "{root}"
            );
            // a result file nested in directories
            let path = object_path(root, "results", &format!("4f2a1b/{id}/nested/out.txt"));
            assert_eq!(
                Output::parse(&path, &objects(root, "results"), &conf).unwrap(),
                (
                    conf.thorium.results.bucket.clone(),
                    format!("{id}/nested/out.txt")
                ),
                "{root}"
            );
        }
    }

    /// A graphic is kept where its key parses back from, whatever its key's first segment: a
    /// user's icon, an entity's image, and keys whose first segment is shorter than the
    /// directory that spreads objects
    #[test]
    fn graphics_parse_back_to_their_keys() {
        // keys like the API's and keys with short first segments
        let conf = conf();
        let keys = [
            "users/thorium/icon.png",
            "4f2a1b3c-5d6e-4f70-8192-a3b4c5d6e7f8/device.png",
            "a/b.png",
            "ab/cd/ef.png",
            "plain.png",
        ];
        for root in ROOTS {
            for key in keys {
                let kept = object_relative_path(BackupComponents::Graphics, "graphics", key);
                let path = objects(root, "graphics").join(kept);
                assert_eq!(
                    Graphic::parse(&path, &objects(root, "graphics"), &conf).unwrap(),
                    (conf.thorium.graphics.bucket.clone(), key.to_owned()),
                    "{root} {key}"
                );
            }
        }
    }

    /// A key that is absolute or holds an empty, `.`, or `..` segment can't be kept under a
    /// backup's `objects/` directory and read back unchanged, and every other key can
    #[test]
    fn only_keys_that_stay_inside_the_objects_directory_are_kept() {
        // keys the API writes, and names with dots inside them
        for key in ["users/thorium/icon.png", "a/b.png", "x..y/.hidden/v1.2.png"] {
            assert!(keepable_key(key), "{key}");
        }
        // keys that would leave the directory or change on the way back
        for key in [
            "",
            "/etc/x.png",
            "a/../../x.png",
            "../x.png",
            "a/./b.png",
            "a//b.png",
            "a/",
        ] {
            assert!(!keepable_key(key), "{key:?}");
        }
    }
}
