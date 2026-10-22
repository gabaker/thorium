//! Wrappers for interacting with reactions within Thorium with different backends
//! Currently only Redis is supported

use base64::Engine as _;
use chrono::prelude::*;
use std::collections::HashMap;
use uuid::Uuid;

// only support tokio for file reads in tokio mode
#[cfg(feature = "tokio-models")]
use tokio::{fs::File, io::AsyncReadExt};

// import python bindings if needed
#[cfg(feature = "python")]
use pyo3::pyclass;

use super::{
    Buffer, Entity, EntityMetadata, GenericJobArgs, GenericJobArgsUpdate, JobHandleStatus,
    ModelApi, ModelAuthLevel, ModelConfigRef, OnDiskFile, RepoDependency, RepoDependencyRequest,
    ToolKind, ToolSpec, WorkloadEnvironment, WorkloadSpec,
};
use crate::{matches_adds, matches_removes, matches_vec, same};

cfg_if::cfg_if! {
    if #[cfg(feature = "api")] {
        use serde_json::Value;
        use std::collections::BTreeMap;

        use super::GenericJobOpts;
        use crate::{deserialize_value, bad, utils::ApiError};

        /// Generic job args containing non statically typed kwargs
        ///
        /// This is used to let users pass in either String or Vec<String> values to kwargs
        #[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
        pub struct RawGenericJobArgs {
            /// The positional arguments to overlay onto the original cmd
            #[serde(default)]
            pub positionals: Vec<String>,
            /// The keyword arguments to overlay onto the original cmd
            #[serde(default)]
            pub kwargs: HashMap<String, Value>,
            /// The switch arguments to overlay onto the original cmd
            #[serde(default)]
            pub switches: Vec<String>,
            /// The options to apply to this generic job
            #[serde(default = "GenericJobOpts::default")]
            pub opts: GenericJobOpts,
        }

        impl TryFrom<RawGenericJobArgs> for GenericJobArgs {
            type Error = ApiError;
            /// Converts [`RawGenericJobArgs`] to [`GenericJobArgs`]
            ///
            /// # Arguments
            ///
            /// * `raw` - The raw generic job args to convert
            fn try_from(raw: RawGenericJobArgs) -> Result<Self, ApiError> {
                // build a hashmap of the right size
                let mut kwargs = BTreeMap::default();
                // crawl over the raw kwarg values and convert them to vectors
                for (key, values) in raw.kwargs {
                    // if this value is a string then wrap it in a vector and insert it
                    if let Some(value) = values.as_str() {
                        kwargs.insert(key, vec!(value.to_owned()));
                        continue;
                    }
                    // if this value is a list then just insert it
                    if values.is_array() {
                        kwargs.insert(key, deserialize_value!(values));
                        continue
                    }
                    // the value was not a string or an array so throw an error
                    return bad!(format!("The kwarg value {:?} for {} is not a string or array", values, key));
                }
                // build our converted job args structure
                let converted = GenericJobArgs {
                    positionals: raw.positionals,
                    kwargs,
                    switches: raw.switches,
                    opts: raw.opts,
                };
                Ok(converted)
            }
        }

        // The arguments for all images in a reaction
        pub type RawReactionArgs = HashMap<String, RawGenericJobArgs>;

        /// A request to create a new reaction
        #[derive(Serialize, Deserialize, Debug, Clone)]
        pub struct RawReactionRequest {
            /// The group the reaction is in
            pub group: String,
            /// The pipeline this reaction is build around
            pub pipeline: String,
            /// The args to overlay ontop of the args for images in this reaction
            pub args: RawReactionArgs,
            /// The number of seconds we have to meet this reactions SLA.
            pub sla: Option<u64>,
            /// The tags this reaction can be listed under
            #[serde(default)]
            pub tags: Vec<String>,
            /// The parent reaction to set if this is a sub reaction
            pub parent: Option<Uuid>,
            /// A list of sample sha256s to download before executing this reaction's jobs
            #[serde(default)]
            pub samples: Vec<String>,
            /// A map of ephemeral buffers to download before executing this reaction's jobs
            #[serde(default)]
            pub buffers: HashMap<String, String>,
            /// Any repos to download before executing this reaction's jobs
            #[serde(default)]
            pub repos: Vec<RepoDependencyRequest>,
            /// This reactions depth in triggers if this reaction was caused by a trigger
            pub trigger_depth: Option<u8>,
            /// Any initial cache for this reaction
            #[serde(default)]
            pub cache: ReactionCache,
        }

        impl TryFrom<RawReactionRequest> for ReactionRequest {
            type Error = ApiError;
            /// Converts [`RawReactionRequest`] to [`ReactionRequest`]
            ///
            /// # Arguments
            ///
            /// * `raw` - The raw reaction request to convert
            fn try_from(raw: RawReactionRequest) -> Result<Self, Self::Error> {
                // build a hashmap to store our job args
                let mut args = HashMap::with_capacity(raw.args.len());
                // crawl over all job args and convert them
                for (stage, raw) in raw.args {
                    args.insert(stage, GenericJobArgs::try_from(raw)?);
                }
                // build the converted reaction request
                let converted = ReactionRequest {
                    group: raw.group,
                    pipeline: raw.pipeline,
                    args,
                    sla: raw.sla,
                    tags: raw.tags,
                    parent: raw.parent,
                    samples: raw.samples,
                    buffers: raw.buffers,
                    repos: raw.repos,
                    trigger_depth: raw.trigger_depth,
                    cache: raw.cache,
                };
                Ok(converted)
            }
        }
    }
}

/// A response containing the Reaction id
#[derive(Serialize)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ReactionIdResponse {
    /// The uuidv4 of a reaction
    pub id: Uuid,
}

/// The response from creating reactions in bulk
#[derive(Serialize, Deserialize, Debug, Default, Clone)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "python", thorium_derive::pyclass(get_all))]
pub struct BulkReactionResponse {
    /// Any errors that occured while creating reactions
    pub errors: HashMap<usize, String>,
    /// The successfully created reactions
    pub created: Vec<Uuid>,
}

impl BulkReactionResponse {
    /// Create a new reaction response with a starting capacity for created reactions
    ///
    /// # Arguments
    ///
    /// * `capacity` - The capacity to allocate
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        BulkReactionResponse {
            errors: HashMap::default(),
            created: Vec::with_capacity(capacity),
        }
    }
}

/// A response for handling the reaction command
#[derive(Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct HandleReactionResponse {
    /// The status of the executed command
    pub status: ReactionStatus,
}

/// The arguments for all images in a reaction
pub type ReactionArgs = HashMap<String, GenericJobArgs>;

/// A request to create a new reaction
#[derive(Serialize, Deserialize, Debug, Clone)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "python", pyclass(from_py_object))]
pub struct ReactionRequest {
    /// The group the reaction is in
    pub group: String,
    /// The pipeline this reaction is build around
    pub pipeline: String,
    /// The args to overlay ontop of the args for images in this reaction
    pub args: ReactionArgs,
    /// The number of seconds we have to meet this reactions SLA.
    pub sla: Option<u64>,
    /// The tags this reaction can be listed under
    #[serde(default)]
    pub tags: Vec<String>,
    /// The parent reaction to set if this is a sub reaction
    pub parent: Option<Uuid>,
    /// A list of sample sha256s to download before executing this reaction's job
    #[serde(default)]
    pub samples: Vec<String>,
    /// A map of ephemeral buffers to download before executing this reaction's jobs
    #[serde(default)]
    pub buffers: HashMap<String, String>,
    /// Any repos to download before executing this reaction's jobs
    #[serde(default)]
    pub repos: Vec<RepoDependencyRequest>,
    /// This reactions depth in triggers if this reaction was caused by a trigger
    pub trigger_depth: Option<u8>,
    /// Any initial cache for this reaction
    #[serde(default)]
    pub cache: ReactionCache,
}

impl ReactionRequest {
    /// Creates a [`ReactionRequest`] for a new reaction
    ///
    /// # Arguments
    ///
    /// * `group` - The group this reaction should be in
    /// * `pipeline` - The pipeline this reaction should be based on
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::models::{ReactionRequest, GenericJobArgs};
    ///
    /// // build args for all images/stages
    /// // plant corn
    /// let plant_args = GenericJobArgs::default()
    ///     .positionals(vec!("corn"));
    /// // use a combine to harvest
    /// let harvest_args = GenericJobArgs::default()
    ///     .switches(vec!("--combine"));
    /// // create a reaction with an SLA of 1 week
    /// let request = ReactionRequest::new("Corn", "harvest")
    ///     .sla(604800)
    ///     .args("plant", plant_args)
    ///     .args("harvest", harvest_args)
    ///     .tag("CornPlants")
    ///     .sample("63b0490d4736e740f26ea9483d55c254abe032845b70ba84ea463ca6582d106f");
    /// ```
    pub fn new<T: Into<String>>(group: T, pipeline: T) -> Self {
        ReactionRequest {
            group: group.into(),
            pipeline: pipeline.into(),
            sla: None,
            args: HashMap::default(),
            tags: Vec::default(),
            parent: None,
            samples: Vec::default(),
            buffers: HashMap::default(),
            repos: Vec::default(),
            trigger_depth: None,
            cache: ReactionCache::default(),
        }
    }

    /// Add args for an image/stage in the reaction
    ///
    /// # Arguments
    ///
    /// * `image` - The name of the image/stage these args are for
    /// * `args` - The args for this image/stage
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::models::{ReactionRequest, GenericJobArgs};
    ///
    /// // build args for all images/stages
    /// // plant corn
    /// let plant_args = GenericJobArgs::default()
    ///     .positionals(vec!("corn"));
    /// // use a combine to harvest
    /// let harvest_args = GenericJobArgs::default()
    ///     .switches(vec!("--combine"));
    /// // create a reaction
    /// let request = ReactionRequest::new("Corn", "harvest")
    ///     .args("plant", plant_args)
    ///     .args("harvest", harvest_args);
    /// ```
    #[must_use]
    pub fn args<T: Into<String>>(mut self, image: T, args: GenericJobArgs) -> Self {
        self.args.insert(image.into(), args);
        self
    }

    /// Set the SLA for a reaction
    ///
    /// # Arguments
    ///
    /// * `sla` - The sla to set for an reaction
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::models::ReactionRequest;
    ///
    /// // create a reaction with an SLA of 1 week
    /// let request = ReactionRequest::new("Corn", "harvest").sla(604800);
    /// ```
    #[must_use]
    pub fn sla(mut self, sla: u64) -> Self {
        self.sla = Some(sla);
        self
    }

    /// Add a search tag
    ///
    /// # Arguments
    ///
    /// * `tag` - A tag to allow this job to be searched by
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::models::ReactionRequest;
    ///
    /// // create a reaction
    /// let request = ReactionRequest::new("Corn", "harvest").tag("CornPlants");
    /// ```
    #[must_use]
    pub fn tag<T: Into<String>>(mut self, tag: T) -> Self {
        self.tags.push(tag.into());
        self
    }

    /// Adds search tags
    ///
    /// # Arguments
    ///
    /// * `tags` - The tags to allow this job to be searched by
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::models::ReactionRequest;
    ///
    /// // create a reaction
    /// let request = ReactionRequest::new("Corn", "harvest")
    ///     .tags(vec!("CornPlants", "Plants"));
    /// ```
    #[must_use]
    pub fn tags<T: Into<String>>(mut self, tags: Vec<T>) -> Self {
        self.tags.extend(tags.into_iter().map(Into::into));
        self
    }

    /// Sets a parent reaction
    ///
    /// # Arguments
    ///
    /// * `parent` - The uuid of the reaction that is spawning this sub reaction
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::models::ReactionRequest;
    /// use uuid::Uuid;
    ///
    /// // get the uuid of the parent reaction
    /// let parent = Uuid::new_v4();
    /// // create a sub reaction with an SLA of 1 day
    /// let request = ReactionRequest::new("Combine", "fill_gas")
    ///     .parent(parent);
    /// ```
    #[must_use]
    pub fn parent<T: Into<Uuid>>(mut self, parent: T) -> Self {
        self.parent = Some(parent.into());
        self
    }

    /// Adds a sample to download when running this reactions jobs
    ///
    /// # Arguments
    ///
    /// * `sample` - The sha256 of the sample to download
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::models::ReactionRequest;
    ///
    /// // create a reaction
    /// let request = ReactionRequest::new("Corn", "harvest")
    ///     .sample("63b0490d4736e740f26ea9483d55c254abe032845b70ba84ea463ca6582d106f");
    /// ```
    #[must_use]
    pub fn sample<T: Into<String>>(mut self, sample: T) -> Self {
        self.samples.push(sample.into());
        self
    }

    /// Adds samples to download when running this reactions jobs
    ///
    /// # Arguments
    ///
    /// * `samples` - The sha256s of the samples to download
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::models::ReactionRequest;
    ///
    /// // create a reaction
    /// let request = ReactionRequest::new("Corn", "harvest")
    ///     .samples(vec!(
    ///         "63b0490d4736e740f26ea9483d55c254abe032845b70ba84ea463ca6582d106f",
    ///         "63b0490d4736e740f26ea9483d55c254abe032845b70ba84ea463ca6582d106f"));
    /// ```
    #[must_use]
    pub fn samples<T: Into<String>>(mut self, samples: Vec<T>) -> Self {
        self.samples.extend(samples.into_iter().map(Into::into));
        self
    }

    /// Adds a file to a reaction request
    ///
    /// # Arguments
    ///
    /// * `path` - The path to the file to pass in
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::models::ReactionRequest;
    ///
    /// # async fn exec() -> Result<(), std::io::ApiError> {
    /// // create a reaction with that depends on a file by path
    /// let request = ReactionRequest::new("Combine", "fill_gas")
    ///     .file("reactions.rs");
    /// # // allow test code to be compiled but don't unwrap as no API instance would be up
    /// # Ok(())
    /// # }
    /// # tokio_test::block_on(async {
    /// #    exec().await.unwrap()
    /// # });
    /// ```
    #[cfg(feature = "tokio-models")]
    pub async fn file<T: AsRef<Path> + Into<Box<dyn std::error::ApiError + Send + Sync>>>(
        mut self,
        path: T,
    ) -> Result<Self, std::io::ApiError> {
        // try to get the name of this file
        let name = match path.as_ref().file_name() {
            Some(name) => name,
            // if we can't get a file name then assume this is a directory and error
            None => return Err(std::io::ApiError::new(ApiErrorKind::IsADirectory, path)),
        };
        // cast our name to a string lossily
        let clean = name.to_string_lossy().into_owned();
        // open a handle to our file
        let mut file = File::open(&path).await?;
        // try to read in our file
        let mut buffer = vec![];
        file.read_to_end(&mut buffer).await?;
        // convert our buffer into a Vec<u8> and base64 it
        let encoded = base64::engine::general_purpose::STANDARD.encode(&buffer);
        self.buffers.insert(clean, encoded);
        Ok(self)
    }

    /// Adds a buffer to a reaction request
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the buffer to pass in
    /// * `buffer` - The buffer to download before running this reactions jobs
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::models::ReactionRequest;
    ///
    /// // create a reaction with that depends on a file by path
    /// let request = ReactionRequest::new("Combine", "fill_gas")
    ///     .buffer("buffer.txt", "I am a buffer");
    /// ```
    #[must_use]
    pub fn buffer<T: Into<String>, B: AsRef<[u8]>>(mut self, name: T, buffer: B) -> Self {
        // convert our buffer into a Vec<u8> and base64 it
        let encoded = base64::engine::general_purpose::STANDARD.encode(&buffer);
        self.buffers.insert(name.into(), encoded);
        self
    }

    /// Adds a repo to a reaction request
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the buffer to pass in
    /// * `repo` - The repo dependency request to add to this reaction
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::models::{ReactionRequest, RepoDependencyRequest};
    ///
    /// // create a reaction with that depends on a repository
    /// let request = ReactionRequest::new("Combine", "fill_gas")
    ///     .repo(RepoDependencyRequest::new("github.com/rustlang/rust").commitish("main"));
    /// ```
    #[must_use]
    pub fn repo(mut self, repo: RepoDependencyRequest) -> Self {
        self.repos.push(repo);
        self
    }

    /// Set a trigger depth for this reaction
    ///
    /// # Arguments
    ///
    /// * `trigger_depth` - The trigger depth to set
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::models::ReactionRequest;
    ///
    /// // create a reaction with that depends on a repository
    /// let request = ReactionRequest::new("Combine", "fill_gas").trigger_depth(3);
    /// ```
    #[must_use]
    pub fn trigger_depth(mut self, trigger_depth: u8) -> Self {
        self.trigger_depth = Some(trigger_depth);
        self
    }

    /// Set some initial reaction cache data
    ///
    /// # Arguments
    ///
    /// * `cache` - The initial cache data to set
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::models::{ReactionRequest, ReactionCache};
    ///
    /// // build some initial cache data to set
    /// let mut cache = ReactionCache::default();
    /// cache.generic.insert("CombineProblems".to_owned(), "Hydraulics".to_owned());
    /// // create a reaction with that depends on a repository
    /// let request = ReactionRequest::new("Combine", "fill_gas").cache(cache);
    /// ```
    #[must_use]
    pub fn cache(mut self, cache: ReactionCache) -> Self {
        self.cache = cache;
        self
    }
}

/// Helps serde default the reaction list limit to 50
fn default_list_limit() -> usize {
    50
}

/// The parameters for a reaction list request
#[derive(Serialize, Deserialize, Debug)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ReactionListParams {
    /// The cursor id to use if one exists
    #[serde(default)]
    pub cursor: usize,
    /// The max amount of reactions to return in on request
    #[serde(default = "default_list_limit")]
    pub limit: usize,
}

impl Default for ReactionListParams {
    fn default() -> Self {
        Self {
            cursor: usize::default(),
            limit: default_list_limit(),
        }
    }
}

impl ReactionListParams {
    /// Set the limit in a builder-like pattern
    ///
    /// # Arguments
    ///
    /// * `limit` - The limit to set
    #[must_use]
    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }
}

/// The parameters for listing reactions across groups with a redis cursor
#[derive(Deserialize, Debug)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ReactionCursorParams {
    /// The groups to list reactions from
    #[serde(default)]
    pub groups: Vec<String>,
    /// The cursor id to use if one exists
    pub cursor: Option<Uuid>,
    /// The max number of reactions to return in a single page
    #[serde(default = "default_list_limit")]
    pub limit: usize,
}

impl Default for ReactionCursorParams {
    /// Create a default reaction cursor params
    fn default() -> Self {
        ReactionCursorParams {
            groups: Vec::default(),
            cursor: None,
            limit: default_list_limit(),
        }
    }
}

impl ReactionCursorParams {
    /// Set the limit in a builder-like pattern
    ///
    /// # Arguments
    ///
    /// * `limit` - The limit to set
    #[must_use]
    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }
}

/// The options that you can set when listing reactions across groups
#[derive(Debug, Clone)]
pub struct ReactionCursorOpts {
    /// The cursor to use to continue this listing
    pub cursor: Option<Uuid>,
    /// The max number of objects to retrieve on a single page
    pub page_size: usize,
    /// The total number of objects to return with this cursor
    pub limit: Option<usize>,
    /// The groups to limit our listing to
    pub groups: Vec<String>,
}

impl Default for ReactionCursorOpts {
    /// Build a default reaction listing
    fn default() -> Self {
        ReactionCursorOpts {
            cursor: None,
            page_size: 50,
            limit: None,
            groups: Vec::default(),
        }
    }
}

impl ReactionCursorOpts {
    /// Set the cursor to use when continuing this listing
    ///
    /// # Arguments
    ///
    /// * `cursor` - The cursor id to use for this listing
    #[must_use]
    pub fn cursor(mut self, cursor: Uuid) -> Self {
        // set the cursor for this listing
        self.cursor = Some(cursor);
        self
    }

    /// The max number of objects to retrieve in a single page
    ///
    /// # Arguments
    ///
    /// * `page_size` - The max number of objects to return in a single request
    #[must_use]
    pub fn page_size(mut self, page_size: usize) -> Self {
        // set the page size for this listing
        self.page_size = page_size;
        self
    }

    /// Limit how many reactions this listing can return at once
    ///
    /// # Arguments
    ///
    /// * `limit` - The max number of objects to return over the lifetime of this cursor
    #[must_use]
    pub fn limit(mut self, limit: usize) -> Self {
        // set the total limit for this listing
        self.limit = Some(limit);
        self
    }

    /// Add a single group to limit our listing to
    ///
    /// # Arguments
    ///
    /// * `group` - The group to restrict our listing to
    #[must_use]
    pub fn group<T: Into<String>>(mut self, group: T) -> Self {
        // add this group to our group restrictions
        self.groups.push(group.into());
        self
    }

    /// Limit what groups we list reactions from
    ///
    /// # Arguments
    ///
    /// * `groups` - The groups to restrict our listing to
    #[must_use]
    pub fn groups<T: Into<String>>(mut self, groups: Vec<T>) -> Self {
        // convert these groups to strings and add them
        self.groups
            .extend(groups.into_iter().map(|group| group.into()));
        self
    }
}

/// A list of reaction names with a cursor
#[derive(Serialize, Deserialize, Debug)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ReactionList {
    /// A cursor used to page through reaction names
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<usize>,
    /// A list of reaction names
    pub names: Vec<String>,
}

/// A list of reaction details with a cursor
#[derive(Serialize, Deserialize, Debug, Default)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ReactionDetailsList {
    /// A cursor used to page through reaction details
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<usize>,
    /// A list of reaction details
    pub details: Vec<Reaction>,
}

/// A timestamped log line
#[derive(Serialize, Deserialize, Debug, Clone)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct StageLogLine {
    /// The position of this line when multiple lines have the same timestamp
    pub index: u64,
    /// The line of log data for this timestamp/index
    pub line: String,
}

impl StageLogLine {
    /// Turn a vector of strings into a [`Vec<StageLog>`]
    ///
    /// # Arguments
    ///
    /// * `raw` - The raw strings to turn into stage log lines
    /// * `start` - The first line these logs should be inserted at (line 10 would be 10)
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::models::StageLogLine;
    ///
    /// // build stage log lines vector
    /// let (lines, end) = StageLogLine::new(vec!("line1", "line2", "line3"), 0);
    /// ```
    #[must_use]
    pub fn new<T: Into<String>>(raw: Vec<T>, start: u64) -> (Vec<Self>, u64) {
        // determine end index value
        let end = start + raw.len() as u64;
        // build the stage log line vector
        let lines = raw
            .into_iter()
            .enumerate()
            .map(|(i, line)| StageLogLine {
                index: i as u64 + start,
                line: line.into(),
            })
            .collect();
        (lines, end)
    }
}

/// A list of log lines to append to a stages logs
#[derive(Serialize, Deserialize, Debug, Default)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct StageLogsAdd {
    /// The current log index to use as a start for newly added logs
    #[serde(skip)]
    pub index: u64,
    /// The log lines to append to our logs in the backend
    #[serde(default)]
    pub logs: Vec<StageLogLine>,
    /// The return to code to set if one has been returned
    pub return_code: Option<i32>,
}

impl StageLogsAdd {
    /// Adds new logs to be saved to an existing `StageLogsAdd`
    ///
    /// # Arguments
    ///
    /// * `logs` - The logs to save for this job
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::models::StageLogsAdd;
    ///
    /// let mut logs = StageLogsAdd::default();
    /// let index = logs.add_logs(vec!("line1", "line2", "line3"));
    /// ```
    pub fn add<T: Into<String>>(&mut self, line: T) {
        // build a single stage log line
        let line = StageLogLine {
            index: self.index,
            line: line.into(),
        };
        // update our index
        self.index += 1;
        // add our logs
        self.logs.push(line);
    }

    /// Adds new logs to be saved to an existing [`StageLogsAdd`]
    ///
    /// # Arguments
    ///
    /// * `logs` - The logs to save for this job
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::models::StageLogsAdd;
    ///
    /// let mut logs = StageLogsAdd::default();
    /// let index = logs.add_logs(vec!("line1", "line2", "line3"));
    /// ```
    pub fn add_logs<T: Into<String>>(&mut self, logs: Vec<T>) {
        // convert our new logs to strings
        let raw: Vec<String> = logs.into_iter().map(Into::into).collect();
        // convert to a vec of stage log lines
        let (logs, end) = StageLogLine::new(raw, self.index);
        // update our index
        self.index = end;
        // add our logs
        self.logs.extend(logs);
    }

    /// Adds new logs to be saved
    ///
    /// # Arguments
    ///
    /// * `logs` - The logs to save for this job
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::models::StageLogsAdd;
    ///
    /// let logs = StageLogsAdd::default()
    ///     .logs(vec!("line1", "line2", "line3"));
    /// ```
    #[must_use]
    pub fn logs<T: Into<String>>(mut self, logs: Vec<T>) -> Self {
        self.add_logs(logs);
        self
    }

    /// Sets the return code to set
    ///
    /// # Arguments
    ///
    /// * `code` - The return code to set for this job
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::models::StageLogsAdd;
    ///
    /// let logs = StageLogsAdd::default().code(0);
    /// ```
    #[must_use]
    pub fn code(mut self, code: i32) -> Self {
        // set our return code
        self.return_code = Some(code);
        self
    }

    /// Sets the index to use when adding logs
    ///
    /// # Arguments
    ///
    /// * `index` - The index to start with when adding new logs
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::models::StageLogsAdd;
    ///
    /// let logs = StageLogsAdd::default().index(32);
    /// ```
    #[must_use]
    pub fn index(mut self, index: u64) -> Self {
        // set our index
        self.index = index;
        self
    }
}

/// The logs for a specific stage within a reaction
///
/// This does not have a cursor because the cursor is just the number of log lines to skip
#[derive(Serialize, Deserialize, Debug)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct StageLogs {
    /// The log lines for a specific stage within a reaction
    pub logs: Vec<String>,
}

/// The different possible statuses for a reaction
#[derive(
    Serialize,
    Deserialize,
    Debug,
    Clone,
    PartialEq,
    Eq,
    Hash,
    clap::ValueEnum,
    strum::EnumString,
    strum::Display,
)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "python", pyclass(from_py_object))]
pub enum ReactionStatus {
    /// This reaction is created, but is not yet running
    #[strum(serialize = "Created")]
    Created,
    /// At least one stage of this reaction has started
    #[strum(serialize = "Started")]
    Started,
    /// This reaction has completed
    #[strum(serialize = "Completed")]
    Completed,
    /// This reaction has failed due to an error
    #[strum(serialize = "Failed")]
    Failed,
}

impl From<JobHandleStatus> for ReactionStatus {
    /// Convert a [`JobHandleStatus`] into its correct `ReactionStatus`
    ///
    /// # Arguments
    ///
    /// * `job_status` - The [`JobHandleStatus`] to convert
    fn from(job_status: JobHandleStatus) -> Self {
        match job_status {
            JobHandleStatus::Waiting
            | JobHandleStatus::Proceeding
            | JobHandleStatus::Sleeping
            | JobHandleStatus::Checkpointed => ReactionStatus::Started,
            JobHandleStatus::Completed => ReactionStatus::Completed,
            JobHandleStatus::Errored => ReactionStatus::Failed,
        }
    }
}

/// What a reaction runs
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum ReactionSource {
    /// The stages of a pipeline, each backed by a tool
    Pipeline {
        /// The name of the pipeline
        name: String,
        /// The exact versions of the stage tools this reaction was created with
        #[serde(default)]
        provenance: RunProvenance,
    },
    /// A single tool instance run directly (a container tool call from a harness, or a user tool run)
    Tool {
        /// The tool instance to run
        tool_instance: Uuid,
        /// The exact versions this run was created with
        #[serde(default)]
        provenance: RunProvenance,
    },
    /// A workflow instance run by its entry agent's harness
    WorkflowRun(ReactionWorkflowRun),
}

/// The workflow instance a reaction is running
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ReactionWorkflowRun {
    /// The workflow instance being run
    pub workflow_instance: Uuid,
    /// The workflow instance's digest at run time
    pub digest: String,
    /// The entry agent that receives the run's inputs
    pub agent: Uuid,
    /// The harness tool instance running the agent
    pub harness: Uuid,
    /// The result tool name the run's results are stored under on each input sample and repo
    pub result_key: String,
    /// The model each agent of the run resolved to when the run was created, sorted by agent id
    #[serde(default)]
    pub models: Vec<ReactionModelBinding>,
    /// The exact versions of the agents and tools this run was created with
    #[serde(default)]
    pub provenance: RunProvenance,
}

/// The exact versions a reaction was created with, set once by the API
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct RunProvenance {
    /// Every agent of a workflow run, sorted by agent id (empty for a tool run)
    #[serde(default, serialize_with = "serialize_agents_sorted")]
    pub agents: Vec<AgentProvenance>,
    /// Every tool instance the run runs or may call, sorted by tool instance id
    #[serde(default, serialize_with = "serialize_tools_sorted")]
    pub tools: Vec<ToolProvenance>,
}

/// Serialize the agents of a provenance in agent id order
///
/// # Arguments
///
/// * `agents` - The agents to serialize
/// * `serializer` - The serializer to write them with
fn serialize_agents_sorted<S: serde::Serializer>(
    agents: &[AgentProvenance],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    // order references to the agents by id so nothing is cloned
    let mut sorted = agents.iter().collect::<Vec<&AgentProvenance>>();
    sorted.sort_by_key(|agent| agent.agent);
    serializer.collect_seq(sorted)
}

/// Serialize the tools of a provenance in tool instance id order
///
/// # Arguments
///
/// * `tools` - The tools to serialize
/// * `serializer` - The serializer to write them with
fn serialize_tools_sorted<S: serde::Serializer>(
    tools: &[ToolProvenance],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    // order references to the tools by tool instance id so nothing is cloned
    let mut sorted = tools.iter().collect::<Vec<&ToolProvenance>>();
    sorted.sort_by_key(|tool| tool.tool_instance);
    serializer.collect_seq(sorted)
}

impl RunProvenance {
    /// Build a provenance from its agents and tools, sorted by id with each tool listed once
    ///
    /// # Arguments
    ///
    /// * `agents` - The agents of a workflow run
    /// * `tools` - The tool instances the run runs or may call
    #[must_use]
    pub fn new(mut agents: Vec<AgentProvenance>, mut tools: Vec<ToolProvenance>) -> Self {
        // sort the agents and tools by id and drop repeated tools
        agents.sort_by_key(|agent| agent.agent);
        agents.dedup_by_key(|agent| agent.agent);
        tools.sort_by_key(|tool| tool.tool_instance);
        tools.dedup_by_key(|tool| tool.tool_instance);
        RunProvenance { agents, tools }
    }

    /// Build the provenance of the tool instances among some entities, skipping anything else
    ///
    /// # Arguments
    ///
    /// * `entities` - The entities the run read
    #[must_use]
    pub fn from_tools<'a, I: IntoIterator<Item = &'a Entity>>(entities: I) -> Self {
        // describe each tool instance
        let tools = entities
            .into_iter()
            .filter_map(ToolProvenance::from_entity)
            .collect();
        RunProvenance::new(Vec::new(), tools)
    }

    /// Whether this provenance records nothing, as for a reaction created before it was recorded
    #[must_use]
    pub fn is_empty(&self) -> bool {
        // nothing was recorded if both lists are empty
        self.agents.is_empty() && self.tools.is_empty()
    }

    /// Get the provenance of one tool instance, if it's recorded
    ///
    /// # Arguments
    ///
    /// * `tool_instance` - The id of the tool instance
    #[must_use]
    pub fn tool(&self, tool_instance: Uuid) -> Option<&ToolProvenance> {
        // find the tool by its instance id
        self.tools
            .iter()
            .find(|tool| tool.tool_instance == tool_instance)
    }
}

/// One agent of a workflow run and the hashes of its configuration and tool surface
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct AgentProvenance {
    /// The agent's id
    pub agent: Uuid,
    /// The agent's name at instancing time
    pub name: String,
    /// The hash of the agent's configuration (`InstancedAgent.config_hash`)
    pub config_hash: String,
    /// The digest of the agent's effective tools (`InstancedAgent.toolset_digest`)
    pub toolset_digest: String,
}

/// One tool instance a run runs or may call
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ToolProvenance {
    /// The id of the tool (root) this is an instance of
    pub tool: Uuid,
    /// The id of the tool instance
    pub tool_instance: Uuid,
    /// The name of the tool
    pub name: String,
    /// The kind of tool this is
    pub kind: ToolKind,
    /// The version label of the tool instance
    pub version: String,
    /// The content hash of the tool instance
    pub spec_hash: String,
    /// The container image as written in the spec (pinned when it carries `@sha256:`), for tools
    /// whose workload runs a container image
    pub image: Option<String>,
}

impl ToolProvenance {
    /// Describe a tool instance entity, or `None` if the entity isn't a tool instance
    ///
    /// # Arguments
    ///
    /// * `entity` - The tool instance entity
    #[must_use]
    pub fn from_entity(entity: &Entity) -> Option<Self> {
        // only tool instances have a provenance
        let EntityMetadata::ToolInstance(instance) = &entity.metadata else {
            return None;
        };
        // only Image and Harness tool instances run a workload that may have an image
        let image = match &instance.spec {
            ToolSpec::Image(workload) => Self::workload_image(workload),
            ToolSpec::Harness(harness) => Self::workload_image(&harness.workload),
            ToolSpec::Agentic(_) => None,
        };
        Some(ToolProvenance {
            tool: instance.tool,
            tool_instance: entity.id,
            name: entity.name.clone(),
            kind: instance.spec.kind(),
            version: instance.version.to_string(),
            spec_hash: instance.spec_hash.clone(),
            image,
        })
    }

    /// Get the container image a workload runs as its spec writes it, if it runs one
    ///
    /// # Arguments
    ///
    /// * `workload` - The workload to read
    #[must_use]
    pub fn workload_image(workload: &WorkloadSpec) -> Option<String> {
        // only K8s and Windows workloads run a container image
        match &workload.environment {
            WorkloadEnvironment::K8s(k8s) => Some(k8s.image.clone()),
            WorkloadEnvironment::Windows(windows) => Some(windows.image.clone()),
            WorkloadEnvironment::BareMetal(_)
            | WorkloadEnvironment::Kvm(_)
            | WorkloadEnvironment::External => None,
        }
    }
}

/// The model, endpoint, and credential one agent of a workflow run resolved to
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ReactionModelBinding {
    /// The agent this binding is for
    pub agent: Uuid,
    /// The position in the agent's effective model list of the matcher that won (unset when the
    /// list was empty)
    pub priority: Option<u32>,
    /// The level whose auth was used
    pub level: ModelAuthLevel,
    /// The endpoint the model is called on
    pub endpoint: ModelConfigRef,
    /// The provider's exact model id
    pub model_id: String,
    /// The protocol the model is called with
    pub api: ModelApi,
    /// The profile whose parameters and limits apply, if one was used
    pub profile: Option<Uuid>,
    /// The credential used (unset for a keyless gateway)
    pub credential: Option<ModelConfigRef>,
}

/// A reaction's stages resolved when it was created
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ReactionPlan {
    /// The stages to run in order; the stages in each inner list run in parallel
    pub stages: Vec<Vec<PlannedStage>>,
}

impl ReactionPlan {
    /// Get the stages of one step of this plan, if the plan has that step
    ///
    /// # Arguments
    ///
    /// * `step` - The index of the step whose stages to get
    #[must_use]
    pub fn step(&self, step: usize) -> Option<&[PlannedStage]> {
        // each step is one inner list of parallel stages
        self.stages.get(step).map(Vec::as_slice)
    }

    /// Get the tool instance a stage of this plan runs
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the stage
    #[must_use]
    pub fn tool_instance(&self, name: &str) -> Option<Uuid> {
        // find the planned stage with this name
        self.stages
            .iter()
            .flatten()
            .find(|stage| stage.name == name)
            .map(|stage| stage.tool_instance)
    }

    /// Get every distinct tool instance this plan runs, sorted
    #[must_use]
    pub fn tool_instances(&self) -> Vec<Uuid> {
        // collect each tool instance once in sorted order
        let unique = self
            .stages
            .iter()
            .flatten()
            .map(|stage| stage.tool_instance)
            .collect::<std::collections::BTreeSet<Uuid>>();
        unique.into_iter().collect()
    }
}

/// A single stage of a reaction's plan
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct PlannedStage {
    /// The stage name that args, logs, and progress are keyed by
    pub name: String,
    /// The tool instance this stage's jobs run, queued and scheduled natively
    pub tool_instance: Uuid,
}

/// The inputs to a workflow or tool run (at least one file or repo)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct RunInputs {
    /// Specific files to run on, by SHA256
    #[serde(default)]
    pub samples: Vec<String>,
    /// Specific repos (and optional commitish) to run on
    #[serde(default)]
    pub repos: Vec<RepoDependencyRequest>,
}

/// A request to run a single tool instance on one or more inputs
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ToolRunRequest {
    /// The files and repos to run the tool on
    pub inputs: RunInputs,
    /// The group to run in, one of the tool instance's groups (picked automatically when it has one
    /// candidate; ignored for sub runs, which use their parent's group)
    #[serde(default)]
    pub group: Option<String>,
    /// The args for the tool's job
    #[serde(default)]
    pub args: GenericJobArgs,
    /// The reaction this run is a sub reaction of (set by harnesses for container tool calls)
    pub parent: Option<Uuid>,
    /// The tags to add to the run's reaction
    #[serde(default)]
    pub tags: Vec<String>,
    /// The SLA for this run in seconds
    pub sla: Option<u64>,
}

/// A request to run a workflow instance on one or more inputs
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct WorkflowRunRequest {
    /// The files and repos to run the workflow on
    pub inputs: RunInputs,
    /// The group to run in, one of the workflow instance's groups (picked automatically when it has
    /// one candidate)
    #[serde(default)]
    pub group: Option<String>,
    /// The tags to add to the run's reaction
    #[serde(default)]
    pub tags: Vec<String>,
    /// The SLA for this run in seconds
    pub sla: Option<u64>,
}

impl RunInputs {
    /// Add a file to run on by its SHA256
    ///
    /// # Arguments
    ///
    /// * `sample` - The SHA256 of the file
    #[must_use]
    pub fn sample<T: Into<String>>(mut self, sample: T) -> Self {
        // add this file to our inputs
        self.samples.push(sample.into());
        self
    }

    /// Add a repo to run on
    ///
    /// # Arguments
    ///
    /// * `repo` - The repo and optional commitish
    #[must_use]
    pub fn repo(mut self, repo: RepoDependencyRequest) -> Self {
        // add this repo to our inputs
        self.repos.push(repo);
        self
    }

    /// Make sure a run has at least one file or repo to run on
    pub fn validate(&self) -> Result<(), String> {
        // a run needs something to run on
        if self.samples.is_empty() && self.repos.is_empty() {
            return Err("a run needs at least one sample or repo".to_owned());
        }
        Ok(())
    }
}

impl ToolRunRequest {
    /// Build a request to run a tool instance on some inputs
    ///
    /// # Arguments
    ///
    /// * `inputs` - The files and repos to run the tool on
    #[must_use]
    pub fn new(inputs: RunInputs) -> Self {
        ToolRunRequest {
            inputs,
            group: None,
            args: GenericJobArgs::default(),
            parent: None,
            tags: Vec::new(),
            sla: None,
        }
    }

    /// Set the group to run in
    ///
    /// # Arguments
    ///
    /// * `group` - One of the tool instance's groups
    #[must_use]
    pub fn group<T: Into<String>>(mut self, group: T) -> Self {
        // set the run group
        self.group = Some(group.into());
        self
    }

    /// Set the args for the tool's job
    ///
    /// # Arguments
    ///
    /// * `args` - The args to pass to the tool
    #[must_use]
    pub fn args(mut self, args: GenericJobArgs) -> Self {
        // set the job's args
        self.args = args;
        self
    }

    /// Set the reaction this run is a sub reaction of
    ///
    /// # Arguments
    ///
    /// * `parent` - The parent reaction
    #[must_use]
    pub fn parent(mut self, parent: Uuid) -> Self {
        // set the parent reaction
        self.parent = Some(parent);
        self
    }

    /// Add a tag to the run's reaction
    ///
    /// # Arguments
    ///
    /// * `tag` - The tag to add
    #[must_use]
    pub fn tag<T: Into<String>>(mut self, tag: T) -> Self {
        // add this tag
        self.tags.push(tag.into());
        self
    }

    /// Set the SLA for this run in seconds
    ///
    /// # Arguments
    ///
    /// * `sla` - The SLA in seconds
    #[must_use]
    pub fn sla(mut self, sla: u64) -> Self {
        // set the SLA
        self.sla = Some(sla);
        self
    }
}

impl WorkflowRunRequest {
    /// Build a request to run a workflow instance on some inputs
    ///
    /// # Arguments
    ///
    /// * `inputs` - The files and repos to run the workflow on
    #[must_use]
    pub fn new(inputs: RunInputs) -> Self {
        WorkflowRunRequest {
            inputs,
            group: None,
            tags: Vec::new(),
            sla: None,
        }
    }

    /// Set the group to run in
    ///
    /// # Arguments
    ///
    /// * `group` - One of the workflow instance's groups
    #[must_use]
    pub fn group<T: Into<String>>(mut self, group: T) -> Self {
        // set the run group
        self.group = Some(group.into());
        self
    }

    /// Add a tag to the run's reaction
    ///
    /// # Arguments
    ///
    /// * `tag` - The tag to add
    #[must_use]
    pub fn tag<T: Into<String>>(mut self, tag: T) -> Self {
        // add this tag
        self.tags.push(tag.into());
        self
    }

    /// Set the SLA for this run in seconds
    ///
    /// # Arguments
    ///
    /// * `sla` - The SLA in seconds
    #[must_use]
    pub fn sla(mut self, sla: u64) -> Self {
        // set the SLA
        self.sla = Some(sla);
        self
    }
}

impl ReactionSource {
    /// Get the exact versions this reaction was created with
    #[must_use]
    pub fn provenance(&self) -> &RunProvenance {
        // every source carries its provenance
        match self {
            ReactionSource::Pipeline { provenance, .. }
            | ReactionSource::Tool { provenance, .. } => provenance,
            ReactionSource::WorkflowRun(run) => &run.provenance,
        }
    }

    /// Get the key a workflow run stores its results under, if this is a workflow run
    #[must_use]
    pub fn result_key(&self) -> Option<&str> {
        // only workflow runs have a result key
        match self {
            ReactionSource::WorkflowRun(run) => Some(&run.result_key),
            ReactionSource::Pipeline { .. } | ReactionSource::Tool { .. } => None,
        }
    }
}

/// A reaction built around a pipeline
///
/// This is used to track jobs across a single run of a pipeline
#[derive(Serialize, Deserialize, Debug, Clone)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "python", thorium_derive::pyclass(get_except(source, plan)))]
pub struct Reaction {
    /// The uuidv4 that identifies this reaction
    pub id: Uuid,
    /// The group this reaction is in
    pub group: String,
    /// The creator of this reaction
    pub creator: String,
    /// The pipeline this reaction is built around (a display label for tool and workflow runs)
    pub pipeline: String,
    /// The current status of this reaction
    pub status: ReactionStatus,
    /// The current stage of this reaction
    pub current_stage: u64,
    /// The current stages progress
    pub current_stage_progress: u64,
    /// The current stages length,
    pub current_stage_length: u64,
    /// The args for this reaction (passed to all jobs)
    pub args: ReactionArgs,
    /// The timestamp this reactions SLA expires at
    pub sla: DateTime<Utc>,
    /// The uuidv4's of the jobs in this pipeline
    pub jobs: Vec<Uuid>,
    /// The tags this reaction can be listed under
    pub tags: Vec<String>,
    /// The parent reaction for this reaction if its a sub reaction
    pub parent: Option<Uuid>,
    /// The number of subreactions for this reaction
    pub sub_reactions: u64,
    /// The number of completed subreactions for this reaction
    pub completed_sub_reactions: u64,
    /// The job id for any generators this reaction currently has active
    pub generators: Vec<Uuid>,
    /// A list of sample sha256s to download before executing this reactions jobs
    pub samples: Vec<String>,
    /// A list of ephemeral files to download
    pub ephemeral: Vec<String>,
    /// A list of ephemeral files from any parent reactions and what parent reaction its tied to
    pub parent_ephemeral: HashMap<String, Uuid>,
    /// A list of repos to download before executing this reactions jobs
    pub repos: Vec<RepoDependency>,
    /// This reactions depth in triggers if this reaction was caused by a trigger
    pub trigger_depth: Option<u8>,
    /// Whether this reaction has any cache data set
    pub has_cache: bool,
    /// What this reaction runs
    pub source: ReactionSource,
    /// The stages resolved when this reaction was created, each running a tool instance
    pub plan: ReactionPlan,
    /// The name of the run-scoped token this reaction's workers use, owned by `creator` (set on
    /// workflow runs and tool runs only; a harness's tool call names its parent run's token)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_token: Option<String>,
    /// The run whose token and per-run workers this reaction's jobs use: this reaction for a
    /// workflow run or direct tool run, the parent workflow run for a harness's tool call
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<Uuid>,
}

impl Reaction {
    /// Get the tool name a tool run or workflow run stores its results under
    ///
    /// A workflow run stores them under its result key and a tool run under the tool of its one
    /// stage; a pipeline reaction has no single key.
    #[must_use]
    pub fn result_key(&self) -> Option<&str> {
        // a workflow run records its key and a tool run stores results under its stage's tool
        match &self.source {
            ReactionSource::WorkflowRun(run) => Some(&run.result_key),
            ReactionSource::Tool { .. } => self
                .plan
                .stages
                .first()
                .and_then(|step| step.first())
                .map(|stage| stage.name.as_str()),
            ReactionSource::Pipeline { .. } => None,
        }
    }
}

/// The first wait of the tool run polling backoff in seconds
pub const TOOL_RUN_POLL_START_SECS: u64 = 1;

/// The longest wait of the tool run polling backoff in seconds
pub const TOOL_RUN_POLL_MAX_SECS: u64 = 60;

/// Get how long to wait before polling a tool run again
///
/// The hint follows the CLI wrapper's polling backoff from the run's start: polls 1 second
/// apart at first, doubling up to 1 minute apart, so the hint is the time left until the
/// backoff's next poll. A finished run has no hint.
///
/// # Arguments
///
/// * `status` - The tool run's current status
/// * `elapsed` - The seconds since the tool run started
#[must_use]
pub fn tool_run_retry_after(status: &ReactionStatus, elapsed: u64) -> Option<u64> {
    // a finished run is never polled again
    if matches!(status, ReactionStatus::Completed | ReactionStatus::Failed) {
        return None;
    }
    // walk the backoff's polls until the first one after now
    let mut wait = TOOL_RUN_POLL_START_SECS;
    let mut next = wait;
    while next <= elapsed {
        // double the wait up to its cap
        wait = (wait * 2).min(TOOL_RUN_POLL_MAX_SECS);
        // once the wait is capped the remaining polls are evenly spaced
        if wait == TOOL_RUN_POLL_MAX_SECS {
            let behind = elapsed - next;
            next += (behind / wait + 1) * wait;
            break;
        }
        next += wait;
    }
    Some(next - elapsed)
}

impl PartialEq<ReactionRequest> for Reaction {
    /// Check if a [`ReactionRequest`] and a [`Reaction`] are equal
    ///
    /// # Arguments
    ///
    /// * `request` - The `ReactionRequest` to compare against
    fn eq(&self, request: &ReactionRequest) -> bool {
        // build a corrected list of tags for this reaction
        let mut corrected = request.tags.clone();
        corrected.append(&mut request.samples.clone());
        corrected.push(self.creator.clone());
        // make sure all fields are the same
        same!(self.group, request.group);
        same!(self.pipeline, request.pipeline);
        matches_vec!(self.tags, corrected);
        same!(self.args, request.args);
        matches_vec!(self.samples, request.samples);
        // make sure we have the same number of ephemeral files
        same!(self.ephemeral.len(), request.buffers.len());
        // make sure our reaction depth is the same
        same!(self.trigger_depth, request.trigger_depth);
        true
    }
}

/// The response given when creating a reaction
#[derive(Clone, Serialize, Deserialize, Debug)]
#[cfg_attr(feature = "python", thorium_derive::pyclass(get_all))]
pub struct ReactionCreation {
    /// The uuidv4 of the created reaction
    pub id: Uuid,
}

/// A Reaction expiration object
#[derive(Serialize, Deserialize, Debug)]
pub struct ReactionExpire {
    /// The cmd to use to expire this data
    pub cmd: String,
    /// The list to remove this id from
    pub list: String,
    // The id to remove
    pub id: String,
}

impl ReactionExpire {
    pub fn new<T: Into<String>, U: Into<String>>(cmd: T, list: U, id: &Uuid) -> Self {
        ReactionExpire {
            cmd: cmd.into(),
            list: list.into(),
            id: id.to_string(),
        }
    }
}

// The arguments for all images in a reaction
pub type ReactionArgsUpdate = HashMap<String, GenericJobArgsUpdate>;

/// A Reaction arg update request that allows stages to change later stages inputs
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ReactionUpdate {
    /// A hashmap of updated args to overlay
    #[serde(default)]
    pub args: ReactionArgsUpdate,
    /// The new tags this reaction should be listed under
    #[serde(default)]
    pub add_tags: Vec<String>,
    /// The tags this reaction should no longer be listed under
    #[serde(default)]
    pub remove_tags: Vec<String>,
    /// The ephemeral files to add to this reaction
    #[serde(default)]
    pub ephemeral: HashMap<String, String>,
}

impl ReactionUpdate {
    /// Adds a stage args update to this reaction update
    ///
    /// # Arguments
    ///
    /// * `stage` - The name of the stage these updated args are for
    /// * `update` - The update to apply to this stages args
    ///
    ///  # Example
    ///
    ///  ```
    ///  use thorium::models::{ReactionUpdate, GenericJobArgsUpdate};
    ///
    ///  // build an update for a specific stage
    ///  let harvester_update = GenericJobArgsUpdate::default()
    ///     .positionals(vec!("soy"))
    ///     .kwarg("--field", vec!("west-4"));
    ///  // build a reaction update with this stage update
    ///  let update = ReactionUpdate::default()
    ///     .arg("harvest", harvester_update);
    ///  ```
    #[must_use]
    pub fn arg<T: Into<String>>(mut self, stage: T, update: GenericJobArgsUpdate) -> Self {
        // insert new stage args update
        self.args.insert(stage.into(), update);
        self
    }

    /// Adds a new tag to list/find this reaction with
    ///
    /// # Arguments
    ///
    /// * `tag` - The new tag for this reaction
    ///
    ///  # Example
    ///
    ///  ```
    ///  use thorium::models::{ReactionUpdate, GenericJobArgsUpdate};
    ///
    ///  // build a reaction update with a new tag
    ///  let update = ReactionUpdate::default()
    ///     .tag("SoyHarvester");
    ///  ```
    #[must_use]
    pub fn tag<T: Into<String>>(mut self, tag: T) -> Self {
        // add new tag
        self.add_tags.push(tag.into());
        self
    }

    /// Removes a tag from this reaction
    ///
    /// # Arguments
    ///
    /// * `tag` - The tag to remove from this reaction
    ///
    ///  # Example
    ///
    ///  ```
    ///  use thorium::models::{ReactionUpdate, GenericJobArgsUpdate};
    ///
    ///  // build a reaction update to remove a tag
    ///  let update = ReactionUpdate::default()
    ///     .remove_tag("BananaHarvester");
    ///  ```
    #[must_use]
    pub fn remove_tag<T: Into<String>>(mut self, tag: T) -> Self {
        // add tag to be removed
        self.remove_tags.push(tag.into());
        self
    }
}

impl PartialEq<ReactionUpdate> for Reaction {
    /// Check if a [`ReactionRequest`] and a [`Reaction`] are equal
    ///
    /// # Arguments
    ///
    /// * `request` - The [`ReactionRequest`] to compare against
    fn eq(&self, request: &ReactionUpdate) -> bool {
        // make sure all fields are the same
        matches_adds!(self.tags, request.add_tags);
        matches_removes!(self.tags, request.remove_tags);
        let args_check = request
            .args
            .iter()
            .all(|(stage, arg)| &self.args[stage] == arg);
        same!(args_check, true);
        true
    }
}

/// An ephemeral cache of information across this entire reaction
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[cfg_attr(feature = "python", pyclass(from_py_object))]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ReactionCache {
    /// A generic key/value cache of info across this reaction
    pub generic: HashMap<String, String>,
    /// Files in this reaction cache
    pub files: Vec<String>,
}

impl ReactionCache {
    /// Check if this cache is empty
    #[must_use]
    pub fn is_empty(&self) -> bool {
        // check if our cache is empty
        self.generic.is_empty() && self.files.is_empty()
    }
}

/// A non file update to a reaction cache
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ReactionCacheUpdate {
    /// A generic key/value cache of info across this reaction
    #[serde(default)]
    pub generic: HashMap<String, String>,
    /// The cache keys to remove
    #[serde(default)]
    pub remove_generic: Vec<String>,
}

impl ReactionCacheUpdate {
    /// Add a generic key to this update
    ///
    /// # Arguments
    ///
    /// * `key` - The key to add to our cache
    /// * `value` - The value to add to our generic cache
    #[must_use]
    pub fn generic<K: Into<String>, V: Into<String>>(mut self, key: K, value: V) -> Self {
        self.generic.insert(key.into(), value.into());
        self
    }

    /// A key to remove from our cache
    ///
    /// # Arguments
    ///
    /// * `key` - The key to remove from our cache
    #[must_use]
    pub fn remove<K: Into<String>>(mut self, key: K) -> Self {
        self.remove_generic.push(key.into());
        self
    }
}

/// An update to files in a reactions cache
#[derive(Debug, Clone, Default)]
pub struct ReactionCacheFileUpdate {
    /// Any files tied to this result
    pub files_on_disk: Vec<OnDiskFile>,
    /// Any buffers to upload as result files
    pub files_buffers: Vec<Buffer>,
    /// The files to remove from our cache
    pub remove_files: Vec<String>,
}

impl ReactionCacheFileUpdate {
    /// Adds a path to a file to upload to this reactions cache
    ///
    /// # Arguments
    ///
    /// * `file` - The on disk file to add
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::models::{OnDiskFile, ReactionCacheFileUpdate};
    ///
    /// let update = ReactionCacheFileUpdate::default()
    ///     .file(OnDiskFile::new("/corn/bushel1"));
    /// ```
    #[must_use]
    pub fn file(mut self, file: OnDiskFile) -> Self {
        // convert our path and add it
        self.files_on_disk.push(file);
        self
    }

    /// Adds paths to files to upload to this reactions cache
    ///
    /// # Arguments
    ///
    /// * `files` - The on disk files to add
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::models::{OnDiskFile, ReactionCacheFileUpdate};
    ///
    /// let update = ReactionCacheFileUpdate::default()
    ///     .files(vec!(OnDiskFile::new("/corn/bushel1"), OnDiskFile::new("/corn/bushel2")));
    /// ```
    #[must_use]
    pub fn files(mut self, files: Vec<OnDiskFile>) -> Self {
        // convert our paths and add them
        self.files_on_disk.extend(files);
        self
    }

    /// Adds a buffer to upload to this reactions cache
    ///
    /// # Arguments
    ///
    /// * `buffer` - The buffer to add
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::models::{Buffer, ReactionCacheFileUpdate};
    ///
    /// let update = ReactionCacheFileUpdate::default()
    ///     .buffer(Buffer::new("buffer").name("buffer.txt"));
    /// ```
    #[must_use]
    pub fn buffer(mut self, buffer: Buffer) -> Self {
        // add our buffer to our list of buffers to upload
        self.files_buffers.push(buffer);
        self
    }

    /// Adds multiple buffers to upload to this reactions cache
    ///
    /// # Arguments
    ///
    /// * `buffers` - The buffers to add
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::models::{Buffer, ReactionCacheFileUpdate};
    ///
    /// let update = ReactionCacheFileUpdate::default()
    ///     .buffers(vec!(Buffer::new("buffer0"), Buffer::new("buffer1")));
    /// ```
    #[must_use]
    pub fn buffers(mut self, mut buffers: Vec<Buffer>) -> Self {
        // append our new buffers on to our list of buffers to upload
        self.files_buffers.append(&mut buffers);
        self
    }

    /// Create a multipart form from this ``ReactionCacheFileUpdate``
    #[cfg(feature = "client")]
    pub async fn to_form(mut self) -> Result<reqwest::multipart::Form, crate::client::Error> {
        // build the form we are going to send
        // disable percent encoding, as the API natively supports UTF-8
        let mut form = reqwest::multipart::Form::new().percent_encode_noop();
        // add the cache files to remove
        form = crate::multipart_list!(form, "remove_files", self.remove_files);
        // add any files that were added by path
        for on_disk in self.files_on_disk {
            // a path was set so read in that file and add it to the form
            form = crate::multipart_file!(form, "files", on_disk.path, on_disk.trim_prefix);
        }
        // add any buffers that were added directly
        for buff in self.files_buffers {
            form = form.part("files", buff.to_part()?);
        }
        Ok(form)
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use serde_json::{Value, json};
    use std::collections::HashMap;
    use uuid::Uuid;

    use super::{AgentProvenance, ToolProvenance};
    use super::{
        PlannedStage, Reaction, ReactionModelBinding, ReactionPlan, ReactionSource, ReactionStatus,
        ReactionWorkflowRun, RunInputs, RunProvenance, ToolRunRequest, WorkflowRunRequest,
        tool_run_retry_after,
    };
    use crate::models::{
        AgenticTool, BareMetalEnvironment, Entity, EntityKinds, EntityMetadata, GenericJobArgs,
        HarnessSpec, K8sEnvironment, KvmEnvironment, ModelApi, ModelAuthLevel, ModelConfigRef,
        RepoDependencyRequest, ToolInstanceEntity, ToolKind, ToolSpec, ToolVersion,
        WindowsEnvironment, WorkloadEnvironment, WorkloadSpec,
    };

    /// Build a plan with two stages in its first step and one in its second
    fn plan() -> ReactionPlan {
        // build three stages that each run their own tool instance
        let stage = |name: &str| PlannedStage {
            name: name.to_owned(),
            tool_instance: Uuid::new_v4(),
        };
        ReactionPlan {
            stages: vec![vec![stage("plant"), stage("water")], vec![stage("harvest")]],
        }
    }

    /// Build a model binding for an agent
    fn binding() -> ReactionModelBinding {
        // bind an agent to a group endpoint and credential
        ReactionModelBinding {
            agent: Uuid::new_v4(),
            priority: Some(1),
            level: ModelAuthLevel::Group,
            endpoint: ModelConfigRef::Group(Uuid::new_v4()),
            model_id: "claude-haiku".to_owned(),
            api: ModelApi::AnthropicMessages,
            profile: None,
            credential: Some(ModelConfigRef::User(Uuid::new_v4())),
        }
    }

    /// Build a workflow run source
    fn workflow_run() -> ReactionWorkflowRun {
        // a workflow run with one model binding
        ReactionWorkflowRun {
            workflow_instance: Uuid::new_v4(),
            digest: "sha256:abc".to_owned(),
            agent: Uuid::new_v4(),
            harness: Uuid::new_v4(),
            result_key: "triage".to_owned(),
            models: vec![binding()],
            provenance: RunProvenance::default(),
        }
    }

    /// Build a pipeline reaction with a plan
    ///
    /// # Arguments
    ///
    /// * `source` - What the reaction runs
    fn reaction(source: ReactionSource) -> Reaction {
        // a reaction on its first stage with no jobs yet
        Reaction {
            id: Uuid::new_v4(),
            group: "jupiter".to_owned(),
            creator: "europa".to_owned(),
            pipeline: "orbit".to_owned(),
            status: ReactionStatus::Created,
            current_stage: 0,
            current_stage_progress: 0,
            current_stage_length: 2,
            args: HashMap::default(),
            sla: Utc::now(),
            jobs: Vec::new(),
            tags: Vec::new(),
            parent: None,
            sub_reactions: 0,
            completed_sub_reactions: 0,
            generators: Vec::new(),
            samples: Vec::new(),
            ephemeral: Vec::new(),
            parent_ephemeral: HashMap::default(),
            repos: Vec::new(),
            trigger_depth: None,
            has_cache: false,
            source,
            plan: plan(),
            run_token: None,
            run: None,
        }
    }

    /// Serialize a value to JSON and read it back
    ///
    /// # Arguments
    ///
    /// * `value` - The value to round trip
    fn round_trip<T: serde::Serialize + serde::de::DeserializeOwned>(value: &T) -> T {
        // serialize and deserialize the value
        let raw = serde_json::to_string(value).expect("serialize");
        serde_json::from_str(&raw).expect("deserialize")
    }

    /// Build a tool instance entity with a spec
    ///
    /// # Arguments
    ///
    /// * `spec` - The tool instance's spec
    fn instance(spec: ToolSpec) -> Entity {
        // an instance named after its tool and version
        Entity {
            id: Uuid::new_v4(),
            name: "sundial@2.1.0".to_owned(),
            kind: EntityKinds::ToolInstance,
            metadata: EntityMetadata::ToolInstance(ToolInstanceEntity {
                tool: Uuid::new_v4(),
                version: ToolVersion::from("2.1.0"),
                previous: None,
                spec_hash: "sha256:spec".to_owned(),
                deprecated: false,
                spec,
                portability: None,
                provides: Vec::new(),
            }),
            description: None,
            submitter: "europa".to_owned(),
            groups: vec!["jupiter".to_owned()],
            tags: HashMap::default(),
            image: None,
            created: Utc::now(),
        }
    }

    /// Build an Image tool spec whose workload runs in an environment
    ///
    /// # Arguments
    ///
    /// * `environment` - Where the workload runs
    fn image_spec(environment: WorkloadEnvironment) -> ToolSpec {
        // a workload with default execution and data settings
        ToolSpec::Image(WorkloadSpec {
            environment,
            ..WorkloadSpec::default()
        })
    }

    /// Build a K8s environment running an image
    ///
    /// # Arguments
    ///
    /// * `image` - The image the environment runs
    fn k8s(image: &str) -> WorkloadEnvironment {
        // a K8s environment with default scheduling
        WorkloadEnvironment::K8s(K8sEnvironment {
            image: image.to_owned(),
            ..K8sEnvironment::default()
        })
    }

    /// Build a tool provenance entry with an id
    ///
    /// # Arguments
    ///
    /// * `tool_instance` - The id of the tool instance
    fn tool_provenance(tool_instance: Uuid) -> ToolProvenance {
        // an Image tool instance running a pinned image
        ToolProvenance {
            tool: Uuid::new_v4(),
            tool_instance,
            name: "sundial@2.1.0".to_owned(),
            kind: ToolKind::Image,
            version: "2.1.0".to_owned(),
            spec_hash: "sha256:spec".to_owned(),
            image: Some("registry.local/sundial@sha256:abc".to_owned()),
        }
    }

    /// Build an agent provenance entry with an id
    ///
    /// # Arguments
    ///
    /// * `agent` - The id of the agent
    fn agent_provenance(agent: Uuid) -> AgentProvenance {
        // an agent with its hashes
        AgentProvenance {
            agent,
            name: "lead".to_owned(),
            config_hash: "sha256:config".to_owned(),
            toolset_digest: "sha256:tools".to_owned(),
        }
    }

    /// Run provenance round trips, every source without one reads with an empty one, and its
    /// agents and tools serialize sorted by id
    #[test]
    fn run_provenance_round_trips() {
        // three ids in a known order
        let mut ids = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
        ids.sort();
        // a provenance built out of order round trips
        let provenance = RunProvenance {
            agents: vec![agent_provenance(ids[2]), agent_provenance(ids[0])],
            tools: vec![
                tool_provenance(ids[1]),
                tool_provenance(ids[2]),
                tool_provenance(ids[0]),
            ],
        };
        let read = round_trip(&provenance);
        assert_eq!(read.agents.len(), 2);
        assert_eq!(read.tools.len(), 3);
        assert_eq!(round_trip(&provenance.tools[0]), provenance.tools[0]);
        assert_eq!(round_trip(&provenance.agents[0]), provenance.agents[0]);
        // its agents and tools serialize sorted by id
        let raw = serde_json::to_value(&provenance).expect("serialize");
        let agents = raw["agents"]
            .as_array()
            .expect("agents")
            .iter()
            .map(|agent| agent["agent"].as_str().unwrap_or_default().to_owned())
            .collect::<Vec<String>>();
        assert_eq!(agents, vec![ids[0].to_string(), ids[2].to_string()]);
        let tools = raw["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .map(|tool| {
                tool["tool_instance"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect::<Vec<String>>();
        let sorted = ids.iter().map(ToString::to_string).collect::<Vec<String>>();
        assert_eq!(tools, sorted);
        // building one sorts it and lists each tool once
        let built = RunProvenance::new(
            vec![agent_provenance(ids[1]), agent_provenance(ids[0])],
            vec![
                tool_provenance(ids[2]),
                tool_provenance(ids[0]),
                tool_provenance(ids[2]),
            ],
        );
        assert_eq!(
            built
                .agents
                .iter()
                .map(|agent| agent.agent)
                .collect::<Vec<Uuid>>(),
            vec![ids[0], ids[1]]
        );
        assert_eq!(
            built
                .tools
                .iter()
                .map(|tool| tool.tool_instance)
                .collect::<Vec<Uuid>>(),
            vec![ids[0], ids[2]]
        );
        // every source carries its provenance through a round trip
        let mut run = workflow_run();
        run.provenance = provenance.clone();
        let sources = [
            ReactionSource::Pipeline {
                name: "orbit".to_owned(),
                provenance: provenance.clone(),
            },
            ReactionSource::Tool {
                tool_instance: ids[0],
                provenance: provenance.clone(),
            },
            ReactionSource::WorkflowRun(run),
        ];
        for source in &sources {
            let read = round_trip(source);
            assert_eq!(read.provenance().tools.len(), 3);
            assert_eq!(read.provenance().agents.len(), 2);
        }
        // sources stored without a provenance read with an empty one
        let mut stored = serde_json::to_value(workflow_run()).expect("serialize");
        stored
            .as_object_mut()
            .expect("an object")
            .remove("provenance");
        let old = [
            json!({"Pipeline": {"name": "orbit"}}),
            json!({"Tool": {"tool_instance": ids[0]}}),
            json!({ "WorkflowRun": stored }),
        ];
        for raw in old {
            let read: ReactionSource = serde_json::from_value(raw).expect("an older source");
            assert!(read.provenance().is_empty());
            assert_eq!(read.provenance(), &RunProvenance::default());
        }
    }

    /// A tool instance's provenance carries its image as its spec writes it for K8s and Windows
    /// Image tools and a harness's K8s workload, and no image for other environments or for
    /// agentic tools
    #[test]
    fn tool_provenance_images() {
        // a tagged K8s image stays a tag and a pinned one stays pinned
        for image in [
            "registry.local/sundial:3",
            "registry.local/sundial@sha256:0123456789abcdef",
        ] {
            let entity = instance(image_spec(k8s(image)));
            let found = ToolProvenance::from_entity(&entity).expect("a tool instance");
            assert_eq!(found.image.as_deref(), Some(image));
            assert_eq!(found.tool_instance, entity.id);
            assert_eq!(found.kind, ToolKind::Image);
            assert_eq!(found.version, "2.1.0");
            assert_eq!(found.spec_hash, "sha256:spec");
            assert_eq!(found.name, "sundial@2.1.0");
            let EntityMetadata::ToolInstance(meta) = &entity.metadata else {
                unreachable!("a tool instance entity")
            };
            assert_eq!(found.tool, meta.tool);
        }
        // a Windows image is recorded as written
        let windows = WorkloadEnvironment::Windows(WindowsEnvironment {
            image: "registry.local/sundial-win:1".to_owned(),
            ..WindowsEnvironment::default()
        });
        let found = ToolProvenance::from_entity(&instance(image_spec(windows))).expect("windows");
        assert_eq!(found.image.as_deref(), Some("registry.local/sundial-win:1"));
        // a harness records its K8s workload's image
        let harness = ToolSpec::Harness(HarnessSpec {
            workload: WorkloadSpec {
                environment: k8s("registry.local/harness@sha256:fedcba"),
                ..WorkloadSpec::default()
            },
            ..HarnessSpec::default()
        });
        let found = ToolProvenance::from_entity(&instance(harness)).expect("harness");
        assert_eq!(found.kind, ToolKind::Harness);
        assert_eq!(
            found.image.as_deref(),
            Some("registry.local/harness@sha256:fedcba")
        );
        // BareMetal, Kvm, and External workloads and agentic tools have no image
        for environment in [
            WorkloadEnvironment::BareMetal(BareMetalEnvironment::default()),
            WorkloadEnvironment::Kvm(KvmEnvironment::default()),
            WorkloadEnvironment::External,
        ] {
            let found =
                ToolProvenance::from_entity(&instance(image_spec(environment))).expect("image");
            assert_eq!(found.image, None);
        }
        let agentic =
            ToolProvenance::from_entity(&instance(ToolSpec::Agentic(AgenticTool::default())))
                .expect("agentic");
        assert_eq!(agentic.kind, ToolKind::Agentic);
        assert_eq!(agentic.image, None);
        // an entity that isn't a tool instance has no tool provenance
        let mut tool = instance(image_spec(k8s("registry.local/sundial:3")));
        tool.metadata = EntityMetadata::Other;
        assert!(ToolProvenance::from_entity(&tool).is_none());
    }

    /// The polling hint follows the 1 second doubling backoff capped at 1 minute from the tool
    /// run's start while it runs, and is unset once it has finished
    #[test]
    fn tool_run_retry_after_follows_backoff() {
        // the backoff polls at 1, 3, 7, 15, 31, and 63 seconds, then every minute
        let expected = [
            (0, 1),
            (1, 2),
            (2, 1),
            (3, 4),
            (6, 1),
            (7, 8),
            (15, 16),
            (31, 32),
            (62, 1),
            (63, 60),
            (64, 59),
            (123, 60),
            (1000, 23),
        ];
        for status in [ReactionStatus::Created, ReactionStatus::Started] {
            for (elapsed, hint) in expected {
                assert_eq!(
                    tool_run_retry_after(&status, elapsed),
                    Some(hint),
                    "{status:?} after {elapsed} seconds"
                );
            }
        }
        // a finished run has no hint
        for status in [ReactionStatus::Completed, ReactionStatus::Failed] {
            for elapsed in [0, 5, 1000] {
                assert_eq!(tool_run_retry_after(&status, elapsed), None);
            }
        }
    }

    /// A tool run stores its results under its one stage's tool, a workflow run under its
    /// result key, and a pipeline reaction has no single result key
    #[test]
    fn reaction_result_keys() {
        // a tool run uses its stage's tool name
        let mut tool_run = reaction(ReactionSource::Tool {
            tool_instance: Uuid::new_v4(),
            provenance: RunProvenance::default(),
        });
        tool_run.plan = ReactionPlan {
            stages: vec![vec![PlannedStage {
                name: "sundial".to_owned(),
                tool_instance: Uuid::new_v4(),
            }]],
        };
        assert_eq!(tool_run.result_key(), Some("sundial"));
        // a workflow run uses its result key
        let run = reaction(ReactionSource::WorkflowRun(workflow_run()));
        assert_eq!(run.result_key(), Some("triage"));
        // a pipeline reaction has none
        let pipeline = reaction(ReactionSource::Pipeline {
            name: "orbit".to_owned(),
            provenance: RunProvenance::default(),
        });
        assert_eq!(pipeline.result_key(), None);
    }

    /// A run needs at least one sample or repo, and samples only, repos only, or both pass
    #[test]
    fn run_inputs_validate() {
        // no samples and no repos is refused
        let error = RunInputs::default().validate().unwrap_err();
        assert_eq!(error, "a run needs at least one sample or repo");
        // samples only, repos only, and both are accepted
        let repo = RepoDependencyRequest::new("github.com/moons/io");
        let samples = RunInputs::default().sample("a".repeat(64));
        let repos = RunInputs::default().repo(repo.clone());
        let both = RunInputs::default().sample("b".repeat(64)).repo(repo);
        for inputs in [samples, repos, both] {
            assert_eq!(inputs.validate(), Ok(()));
        }
        // unknown fields such as planned input queries are refused
        let queries = serde_json::from_value::<RunInputs>(json!({"samples": ["a"], "queries": []}));
        assert!(queries.is_err());
    }

    /// Reaction sources, plans, model bindings, run inputs, and run requests round trip
    #[test]
    fn run_types_round_trip() {
        // each source round trips
        let sources = [
            ReactionSource::Pipeline {
                name: "orbit".to_owned(),
                provenance: RunProvenance::default(),
            },
            ReactionSource::Tool {
                tool_instance: Uuid::new_v4(),
                provenance: RunProvenance::default(),
            },
            ReactionSource::WorkflowRun(workflow_run()),
        ];
        for source in &sources {
            assert_eq!(&round_trip(source), source);
        }
        // a workflow run, binding, plan, and stage round trip
        let run = workflow_run();
        assert_eq!(round_trip(&run), run);
        let bound = binding();
        assert_eq!(round_trip(&bound), bound);
        let planned = plan();
        assert_eq!(round_trip(&planned), planned);
        assert_eq!(round_trip(&planned.stages[1][0]), planned.stages[1][0]);
        // run inputs carry samples and repos
        let inputs = RunInputs {
            samples: vec!["a".repeat(64)],
            repos: vec![RepoDependencyRequest::new("github.com/moons/io").commitish("main")],
        };
        let read = round_trip(&inputs);
        assert_eq!(read.samples, inputs.samples);
        assert_eq!(read.repos[0].url, "github.com/moons/io");
        assert_eq!(read.repos[0].commitish.as_deref(), Some("main"));
        // a tool run request round trips with its fields
        let tool_run = ToolRunRequest {
            inputs: inputs.clone(),
            group: Some("jupiter".to_owned()),
            args: GenericJobArgs::default().positionals(vec!["--fast"]),
            parent: Some(Uuid::new_v4()),
            tags: vec!["moon".to_owned()],
            sla: Some(600),
        };
        let read = round_trip(&tool_run);
        assert_eq!(read.group, tool_run.group);
        assert_eq!(read.args, tool_run.args);
        assert_eq!(read.parent, tool_run.parent);
        assert_eq!(read.tags, tool_run.tags);
        assert_eq!(read.sla, Some(600));
        // a workflow run request round trips with its fields
        let workflow = WorkflowRunRequest {
            inputs,
            group: Some("jupiter".to_owned()),
            tags: vec!["moon".to_owned()],
            sla: None,
        };
        let read = round_trip(&workflow);
        assert_eq!(read.group, workflow.group);
        assert_eq!(read.tags, workflow.tags);
        // run requests without a group deserialize with none
        let tool_run: ToolRunRequest = serde_json::from_value(
            json!({"inputs": {"samples": ["a"]}, "parent": null, "sla": null}),
        )
        .expect("tool run without a group");
        assert_eq!(tool_run.group, None);
        let workflow: WorkflowRunRequest =
            serde_json::from_value(json!({"inputs": {"repos": []}, "sla": null}))
                .expect("workflow run without a group");
        assert_eq!(workflow.group, None);
    }

    /// A reaction round trips with its source, plan, run token, and run
    #[test]
    fn reaction_round_trips_run_fields() {
        // a workflow run reaction carries every run field
        let mut workflow = reaction(ReactionSource::WorkflowRun(workflow_run()));
        workflow.run_token = Some(format!("run-{}", workflow.id));
        workflow.run = Some(workflow.id);
        let read = round_trip(&workflow);
        assert_eq!(read.source, workflow.source);
        assert_eq!(read.plan, workflow.plan);
        assert_eq!(read.run_token, workflow.run_token);
        assert_eq!(read.run, workflow.run);
        // a reaction without a run token or run reads with neither
        let mut raw = serde_json::to_value(&workflow).expect("serialize");
        let map = raw.as_object_mut().expect("an object");
        map.remove("run_token");
        map.remove("run");
        let read: Reaction = serde_json::from_value(raw).expect("deserialize");
        assert_eq!(read.run_token, None);
        assert_eq!(read.run, None);
    }

    /// A reaction always writes its source and plan and writes its run fields only when set
    #[test]
    fn reaction_json_run_fields() {
        // a pipeline reaction writes no run fields
        let pipeline = reaction(ReactionSource::Pipeline {
            name: "orbit".to_owned(),
            provenance: RunProvenance::default(),
        });
        let raw = serde_json::to_value(&pipeline).expect("serialize");
        let map = raw.as_object().expect("an object");
        assert!(!map.contains_key("run_token"));
        assert!(!map.contains_key("run"));
        assert!(map.contains_key("source"));
        assert!(map.contains_key("plan"));
        // a workflow run reaction writes both run fields
        let mut workflow = reaction(ReactionSource::WorkflowRun(workflow_run()));
        workflow.run_token = Some("run-token".to_owned());
        workflow.run = Some(workflow.id);
        let raw = serde_json::to_value(&workflow).expect("serialize");
        assert!(raw.get("run_token").is_some());
        assert!(raw.get("run").is_some());
        // a reaction missing its source or plan doesn't deserialize
        for field in ["source", "plan"] {
            let mut raw: Value = serde_json::to_value(&pipeline).expect("serialize");
            raw.as_object_mut().expect("an object").remove(field);
            assert!(
                serde_json::from_value::<Reaction>(raw).is_err(),
                "a reaction without {field} deserialized"
            );
        }
    }
}
