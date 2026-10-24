//! The data-shaping record: the partition sizes, `earliest` values, and the files password's
//! identity that the first `schema apply` records in `schema_migrations`, and the checks every
//! later apply makes against it
//!
//! A partition size decides which bucket a row is written to, so changing it while rows exist
//! leaves those rows where reads and deletes no longer look. Stored files are carted with the
//! files password, so changing it leaves them unreadable with the new one. The record lets
//! `schema apply` refuse such a change while its data exists, instead of letting it orphan rows.
//! An `earliest` value only hides older data from default listings and searches, so a changed
//! one is recorded on every apply, with a warning when it was raised.

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use chrono::{DateTime, NaiveDate, SecondsFormat, Utc};
use scylla::client::session::Session;
use scylla::serialize::row::SerializeRow;
use scylla::value::CqlValue;
use std::collections::BTreeMap;
use uuid::Uuid;

use super::queries::migrations::SchemaMigrationsQueries;
use super::scylla::{S3IdsTable, SchemaMigrationsTable};
use super::{
    ApplyError, ColumnRole, ConfigRecord, CqlDesc, CqlNative, MigrationRecordKind, ObjectDef,
    ScyllaObject,
};
use crate::Conf;
use crate::conf::DEFAULT_FILES_PASSWORD;
use crate::models::TagType;
use crate::utils::helpers::partition;

/// The record id of the files password's identity
pub const FILES_PASSWORD: &str = "files.password";

/// The data types whose partition size and `earliest` value are recorded, as they're keyed in the
/// record
pub const DATA_TYPES: [&str; 8] = [
    "files",
    "repos",
    "results",
    "entities",
    "associations",
    "tags.files",
    "tags.repos",
    "tags.entities",
];

/// What a partition size's record id starts with
const PARTITION_SIZE: &str = "partition_size.";

/// What an `earliest` value's record id starts with
const EARLIEST: &str = "earliest.";

/// Get one data type's partition size and `earliest` value from the config
///
/// # Arguments
///
/// * `conf` - The Thorium config
/// * `data_type` - The data type, as [`DATA_TYPES`] names it
fn settings_of(conf: &Conf, data_type: &str) -> Option<(u16, i64)> {
    // each data type reads its own section; tags have one per tag type
    let thorium = &conf.thorium;
    match data_type {
        "files" => Some((thorium.files.partition_size, thorium.files.earliest)),
        "repos" => Some((thorium.repos.partition_size, thorium.repos.earliest)),
        "results" => Some((thorium.results.partition_size, thorium.results.earliest)),
        "entities" => Some((thorium.entities.partition_size, thorium.entities.earliest)),
        "associations" => Some((
            thorium.associations.partition_size,
            thorium.associations.earliest,
        )),
        "tags.files" => Some((
            thorium.tags.files.partition_size,
            thorium.tags.files.earliest,
        )),
        "tags.repos" => Some((
            thorium.tags.repos.partition_size,
            thorium.tags.repos.earliest,
        )),
        "tags.entities" => Some((
            thorium.tags.entities.partition_size,
            thorium.tags.entities.earliest,
        )),
        _ => None,
    }
}

/// Get the config path of a recorded setting, as errors and the operator name it
///
/// `partition_size.files` is `thorium.files.partition_size`, `earliest.tags.repos` is
/// `thorium.tags.repos.earliest`, and `files.password` is `thorium.files.password`.
///
/// # Arguments
///
/// * `id` - The setting's record id
#[must_use]
pub fn setting_path(id: &str) -> String {
    // a partition size or an earliest value names its data type's section
    if let Some(data_type) = id.strip_prefix(PARTITION_SIZE) {
        return format!("thorium.{data_type}.partition_size");
    }
    if let Some(data_type) = id.strip_prefix(EARLIEST) {
        return format!("thorium.{data_type}.earliest");
    }
    // the files password and anything else keep their own path
    format!("thorium.{id}")
}

/// Get the record id of a setting from its config path, the inverse of [`setting_path`]
///
/// # Arguments
///
/// * `path` - The setting's config path, such as `thorium.files.earliest`
#[must_use]
pub fn setting_id(path: &str) -> String {
    // drop the section prefix, then move the setting's name to the front
    let rest = path.strip_prefix("thorium.").unwrap_or(path);
    if let Some(data_type) = rest.strip_suffix(".partition_size") {
        return format!("{PARTITION_SIZE}{data_type}");
    }
    if let Some(data_type) = rest.strip_suffix(".earliest") {
        return format!("{EARLIEST}{data_type}");
    }
    rest.to_owned()
}

/// Build the argon2id hasher with the fixed parameters every files-password identity uses
fn hasher() -> Argon2<'static> {
    // argon2's defaults (19 MiB, 2 passes, 1 lane), the same as Thorium's user passwords
    Argon2::new(Algorithm::Argon2id, Version::V0x13, Params::default())
}

/// Hash the files password into its identity: an argon2id PHC string with a random salt and
/// `Params::default()`
///
/// # Arguments
///
/// * `password` - The configured files password
pub fn hash_files_password(password: &str) -> Result<String, ApplyError> {
    // a fresh random salt, so two deployments with the same password record different hashes;
    // two v4 UUIDs come from the OS's random source, which builds without the api feature reach
    // without enabling another random number generator
    let mut bytes = Vec::with_capacity(32);
    bytes.extend_from_slice(Uuid::new_v4().as_bytes());
    bytes.extend_from_slice(Uuid::new_v4().as_bytes());
    let salt = SaltString::encode_b64(&bytes).map_err(|err| ApplyError::Query {
        statement: "salt the files password".to_owned(),
        error: err.to_string(),
    })?;
    // hash with the fixed parameters into a PHC string that carries the salt
    hasher()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|err| ApplyError::Query {
            statement: "hash the files password".to_owned(),
            error: err.to_string(),
        })
}

/// Check that a recorded files-password identity is argon2id with `Params::default()`, without
/// computing any hash
///
/// Verifying recomputes the hash with the costs written in the recorded string, so a crafted
/// record or backup naming huge costs would make every apply spin or allocate that much.
///
/// # Arguments
///
/// * `phc` - The recorded PHC string
pub fn check_files_password_hash(phc: &str) -> Result<PasswordHash<'_>, ApplyError> {
    // parse the string without computing anything
    let parsed = PasswordHash::new(phc).map_err(|_| ApplyError::PasswordHashParams)?;
    // only argon2id at version 19, the algorithm Thorium hashes with
    if parsed.algorithm != Algorithm::Argon2id.ident()
        || parsed.version != Some(Version::V0x13.into())
    {
        return Err(ApplyError::PasswordHashParams);
    }
    // only the default costs and output length, no key id or associated data, and a salt and a
    // hash present
    let params = Params::try_from(&parsed).map_err(|_| ApplyError::PasswordHashParams)?;
    let default = Params::default();
    let costs = (params.m_cost(), params.t_cost(), params.p_cost());
    let output = parsed.hash.map(|hash| hash.len());
    if costs != (default.m_cost(), default.t_cost(), default.p_cost())
        || output != Some(Params::DEFAULT_OUTPUT_LEN)
        || !params.keyid().is_empty()
        || !params.data().is_empty()
        || parsed.salt.is_none()
    {
        return Err(ApplyError::PasswordHashParams);
    }
    Ok(parsed)
}

/// Whether the configured files password matches a recorded identity, after refusing an identity
/// with other parameters before any hash is computed
///
/// # Arguments
///
/// * `phc` - The recorded PHC string
/// * `password` - The configured files password
pub fn verify_files_password(phc: &str, password: &str) -> Result<bool, ApplyError> {
    // refuse other algorithms and costs first
    let parsed = check_files_password_hash(phc)?;
    // re-derive the hash with the recorded salt and the fixed parameters
    match hasher().verify_password(password.as_bytes(), &parsed) {
        Ok(()) => Ok(true),
        Err(argon2::password_hash::Error::Password) => Ok(false),
        Err(_) => Err(ApplyError::PasswordHashParams),
    }
}

/// The warning `schema apply` and API startup give when the files password is the public default
///
/// # Arguments
///
/// * `conf` - The Thorium config
#[must_use]
pub fn default_password_warning(conf: &Conf) -> Option<String> {
    // only the public default is warned about
    (conf.thorium.files.password == DEFAULT_FILES_PASSWORD).then(|| {
        "thorium.files.password is the public default; stored files are carted with a password \
         anyone can look up, and it can't be changed once files are stored"
            .to_owned()
    })
}

impl ConfigRecord {
    /// Get the effective partition size of every data type from the config, keyed like the record
    ///
    /// # Arguments
    ///
    /// * `conf` - The Thorium config
    #[must_use]
    pub fn partition_sizes_of(conf: &Conf) -> BTreeMap<String, u16> {
        // every data type's size, tags per tag type
        DATA_TYPES
            .iter()
            .filter_map(|data_type| {
                settings_of(conf, data_type).map(|(size, _)| ((*data_type).to_owned(), size))
            })
            .collect()
    }

    /// Get every `earliest` value from the config, keyed like the record
    ///
    /// # Arguments
    ///
    /// * `conf` - The Thorium config
    #[must_use]
    pub fn earliest_of(conf: &Conf) -> BTreeMap<String, i64> {
        // every data type's earliest value, tags per tag type
        DATA_TYPES
            .iter()
            .filter_map(|data_type| {
                settings_of(conf, data_type)
                    .map(|(_, earliest)| ((*data_type).to_owned(), earliest))
            })
            .collect()
    }

    /// Build the record of a config, hashing its files password with a fresh salt
    ///
    /// # Arguments
    ///
    /// * `conf` - The Thorium config
    pub fn from_conf(conf: &Conf) -> Result<Self, ApplyError> {
        // the sizes and earliest values as configured, and the password's identity
        Ok(ConfigRecord {
            partition_sizes: Self::partition_sizes_of(conf),
            earliest: Self::earliest_of(conf),
            files_password: hash_files_password(&conf.thorium.files.password)?,
        })
    }

    /// Get the record as `schema_migrations` config rows, as `(id, value)` pairs; an empty
    /// password identity has no row
    #[must_use]
    pub fn rows(&self) -> Vec<(String, String)> {
        // a row per partition size, then per earliest value, then the password's identity
        let sizes = self
            .partition_sizes
            .iter()
            .map(|(data_type, size)| (format!("{PARTITION_SIZE}{data_type}"), size.to_string()));
        let earliest = self
            .earliest
            .iter()
            .map(|(data_type, at)| (format!("{EARLIEST}{data_type}"), at.to_string()));
        let password = (!self.files_password.is_empty())
            .then(|| (FILES_PASSWORD.to_owned(), self.files_password.clone()));
        sizes.chain(earliest).chain(password).collect()
    }

    /// Read a record from `schema_migrations` config rows, which may hold only some settings; a
    /// missing password leaves the identity empty, and unknown ids are ignored
    ///
    /// # Arguments
    ///
    /// * `rows` - The config rows, as `(id, value)` pairs
    pub fn from_rows<I>(rows: I) -> Result<Self, ApplyError>
    where
        I: IntoIterator<Item = (String, String)>,
    {
        // start from an empty record and fill in each row's setting
        let mut record = ConfigRecord {
            partition_sizes: BTreeMap::new(),
            earliest: BTreeMap::new(),
            files_password: String::new(),
        };
        // a value that doesn't parse can't be compared, so it stops the apply
        let unreadable = |id: &str, value: &str| ApplyError::Query {
            statement: "read the data-shaping record".to_owned(),
            error: format!("{id} holds {value:?}, which isn't a number"),
        };
        for (id, value) in rows {
            if let Some(data_type) = id.strip_prefix(PARTITION_SIZE) {
                let size = value.parse().map_err(|_| unreadable(&id, &value))?;
                record.partition_sizes.insert(data_type.to_owned(), size);
            } else if let Some(data_type) = id.strip_prefix(EARLIEST) {
                let at = value.parse().map_err(|_| unreadable(&id, &value))?;
                record.earliest.insert(data_type.to_owned(), at);
            } else if id == FILES_PASSWORD {
                record.files_password = value;
            }
        }
        Ok(record)
    }
}

/// What the data-shaping check decided before the schema converges: the config rows to record
/// once it has, and the warnings to report
#[derive(Debug, Default)]
pub(super) struct ConfigPlan {
    /// The config rows to write after the schema converges, as `(id, value)` pairs
    pub rows: Vec<(String, String)>,
    /// The warnings for the run's report
    pub warnings: Vec<String>,
}

/// Read the data-shaping settings an earlier apply recorded, when `schema_migrations` exists
///
/// # Arguments
///
/// * `session` - The Scylla session to read with
/// * `conf` - The Thorium config, for the keyspace
async fn read_recorded(session: &Session, conf: &Conf) -> Result<ConfigRecord, ApplyError> {
    // prepare the record's statements in the existing keyspace
    let queries = SchemaMigrationsQueries::prepare(session, &conf.thorium.namespace)
        .await
        .map_err(|err| ApplyError::Query {
            statement: "prepare the schema_migrations statements".to_owned(),
            error: err.to_string(),
        })?;
    // read every config row
    let rows = queries
        .list(session, &MigrationRecordKind::Config)
        .await
        .map_err(|err| ApplyError::Query {
            statement: "read the data-shaping record".to_owned(),
            error: err.to_string(),
        })?;
    let mut settings = Vec::new();
    for row in rows {
        // a row that doesn't deserialize can't be compared
        let row = row.map_err(|err| ApplyError::Query {
            statement: "read the data-shaping record".to_owned(),
            error: err.to_string(),
        })?;
        settings.push((row.id, row.value.unwrap_or_default()));
    }
    ConfigRecord::from_rows(settings)
}

/// What a schema apply records instead of refusing
///
/// Only the first record's sample check can be passed this way: a changed partition size goes
/// through its rebucket step, and the files password can't change once files are stored.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigAccept {
    /// Record the configured partition sizes at the first record although sampled rows sit in
    /// buckets another size gives (the operator passes it once the step is approved)
    pub partition_sizes: bool,
}

/// The warning an apply gives when it records a raised `earliest` value
///
/// # Arguments
///
/// * `data_type` - The data type, as [`DATA_TYPES`] names it
/// * `at` - The raised value, in seconds since the epoch
fn raised_earliest_warning(data_type: &str, at: i64) -> String {
    // name the date the value stands for, or the number when no date holds it
    let date = DateTime::from_timestamp(at, 0).map_or_else(
        || at.to_string(),
        |date| date.to_rfc3339_opts(SecondsFormat::Secs, true),
    );
    format!(
        "{EARLIEST}{data_type} raised: data older than {date} no longer appears in default \
         listings and searches. Nothing is deleted. Lower it again to see it."
    )
}

impl ApplyError {
    /// The config paths of the settings this error refuses because they changed while their data
    /// exists, such as `thorium.files.partition_size`, or, for a recorded password identity with
    /// other parameters, `thorium.files.password`; the operator plans a step for each
    #[must_use]
    pub fn refused_settings(&self) -> Vec<String> {
        match self {
            ApplyError::ConfigChanged { setting, .. } => vec![setting.clone()],
            ApplyError::PasswordHashParams => vec![setting_path(FILES_PASSWORD)],
            ApplyError::Refused(refusals) => refusals
                .iter()
                .flat_map(ApplyError::refused_settings)
                .collect(),
            _ => Vec::new(),
        }
    }

    /// The config paths of every setting this error stops an apply or a start over: the changed
    /// ones [`ApplyError::refused_settings`] names, and the partition sizes whose sampled rows sit
    /// in buckets the configured size doesn't give
    pub(super) fn stopped_settings(&self) -> Vec<String> {
        // a changed setting, then each data type whose size the sampled rows contradict
        let mut settings = self.refused_settings();
        settings.extend(
            self.sampled_data_types()
                .into_iter()
                .map(|data_type| setting_path(&format!("{PARTITION_SIZE}{data_type}"))),
        );
        settings
    }

    /// The data types whose sampled rows sit in buckets the configured partition size doesn't
    /// give; the operator blocks the step on an approval for these
    #[must_use]
    pub fn sampled_data_types(&self) -> Vec<String> {
        match self {
            ApplyError::SampledSizeMismatch { data_type, .. } => vec![data_type.clone()],
            ApplyError::Refused(refusals) => refusals
                .iter()
                .flat_map(ApplyError::sampled_data_types)
                .collect(),
            _ => Vec::new(),
        }
    }
}

/// The partition sizes the code defaults to and the chart renders, preferred when several sizes
/// explain sampled rows
const COMMON_SIZES: [u16; 5] = [10, 60, 180, 600, 3600];

/// How many token ranges each partitioned table is sampled in
const SAMPLE_RANGES: i128 = 16;

/// How many rows are read from each sampled token range
const SAMPLE_ROWS: u32 = 64;

/// The column of the tags tables that holds each row's tag type
const TAG_TYPE_COLUMN: &str = "type";

/// One time-bucketed table: its rows' buckets come from a timestamp column and a partition size
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Bucketed {
    /// The table's name
    pub table: &'static str,
    /// The data type whose partition size buckets it, or `None` when each row's tag type picks
    /// its `tags.<type>` size
    pub data_type: Option<String>,
    /// The partition key columns, in key order
    pub partition: Vec<&'static str>,
    /// The timestamp column the bucket comes from
    pub timestamp: &'static str,
}

/// Get the time-bucketed tables of a schema: each table whose bucket column names a partition
/// size, with the first clustering timestamp column its bucket comes from
///
/// # Arguments
///
/// * `objects` - The schema's Scylla objects
pub(super) fn bucketed(objects: &[ObjectDef]) -> Vec<Bucketed> {
    // keep the tables with a partition size and a clustering timestamp
    let mut tables = Vec::new();
    for object in objects {
        let ObjectDef::Table(table) = object else {
            continue;
        };
        let Some(setting) = table.partition_size else {
            continue;
        };
        // the tags tables pick their size by each row's type; the others name their section
        let data_type = if setting.starts_with("tags.") {
            None
        } else {
            Some(setting.trim_end_matches(".partition_size").to_owned())
        };
        // the bucket comes from the first clustering column that is a timestamp
        let timestamp = table
            .columns
            .iter()
            .filter_map(|column| match column.role {
                ColumnRole::Cluster(position, _)
                    if column.desc == CqlDesc::Native(CqlNative::Timestamp) =>
                {
                    Some((position, column.name))
                }
                _ => None,
            })
            .min()
            .map(|(_, name)| name);
        // a table without one has no bucket to check
        if let Some(timestamp) = timestamp {
            tables.push(Bucketed {
                table: table.name,
                data_type,
                partition: object.partition_key(),
                timestamp,
            });
        }
    }
    tables
}

/// The tag type a `tags.<type>` data type names, as the tags tables store it
///
/// # Arguments
///
/// * `data_type` - The data type, such as `tags.files`
fn tag_type(data_type: &str) -> Option<String> {
    // match the lowercase name against each tag type's stored text
    let name = data_type.strip_prefix("tags.")?;
    [TagType::Files, TagType::Repos, TagType::Entities]
        .iter()
        .map(|tag| tag.as_str().to_owned())
        .find(|stored| stored.eq_ignore_ascii_case(name))
}

/// The bounds of the partition sizes that would put sampled rows in their buckets, and how many
/// rows the configured size doesn't
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Sample {
    /// How many sampled rows sit in a bucket the configured size doesn't give
    pub mismatched: u64,
    /// The smallest size that explains every mismatched row
    low: i64,
    /// The largest size that explains every mismatched row
    high: i64,
}

impl Default for Sample {
    /// A sample that has seen no mismatch, which any size explains
    fn default() -> Self {
        Sample {
            mismatched: 0,
            low: 1,
            high: i64::from(u16::MAX),
        }
    }
}

impl Sample {
    /// Note a row whose bucket the configured size doesn't give, narrowing the sizes that would
    ///
    /// # Arguments
    ///
    /// * `seconds` - How many seconds into its year the row's timestamp is, as buckets count them
    /// * `bucket` - The row's stored bucket
    pub(super) fn mismatch(&mut self, seconds: i64, bucket: i32) {
        // count the row, then keep only the sizes that put it in its bucket
        self.mismatched += 1;
        let bucket = i64::from(bucket);
        let (low, high) = match (seconds, bucket) {
            // no size gives a negative bucket from a time after the year's start, or the reverse
            (seconds, bucket) if seconds < 0 || bucket < 0 => (1, 0),
            // the first bucket holds every time below the size
            (seconds, 0) => (seconds + 1, i64::from(u16::MAX)),
            // a later bucket holds the times from bucket * size up to the next bucket's start
            (seconds, bucket) => (seconds / (bucket + 1) + 1, seconds / bucket),
        };
        self.low = self.low.max(low);
        self.high = self.high.min(high);
    }

    /// The partition size the mismatched rows suggest, when one size explains them all: the one
    /// common size among those that do, or the only size that does
    #[must_use]
    pub(super) fn suggested(&self) -> Option<u16> {
        // no size explains rows whose bounds crossed
        if self.mismatched == 0 || self.low > self.high {
            return None;
        }
        // the common sizes inside the bounds, preferred over any other size that fits
        let common = COMMON_SIZES
            .iter()
            .copied()
            .filter(|size| (self.low..=self.high).contains(&i64::from(*size)))
            .collect::<Vec<_>>();
        // one common size, else the only size the bounds leave, else nothing
        match common.as_slice() {
            [size] => Some(*size),
            _ if self.low == self.high => u16::try_from(self.low).ok(),
            _ => None,
        }
    }
}

/// How many seconds into its year a timestamp is, counted the way buckets are
///
/// # Arguments
///
/// * `timestamp` - The row's timestamp
/// * `year` - The row's stored year
pub(super) fn seconds_into(timestamp: DateTime<Utc>, year: i32) -> Option<i64> {
    // buckets count from one second past the start of the year
    let start = NaiveDate::from_ymd_opt(year, 1, 1)?.and_hms_opt(0, 0, 1)?;
    Some((timestamp.naive_utc() - start).num_seconds())
}

/// Reads the data-shaping checks need from the keyspace's time-bucketed tables
struct DataReader<'a> {
    /// The Scylla session to read with
    session: &'a Session,
    /// The keyspace
    keyspace: &'a str,
    /// The time-bucketed tables the keyspace holds
    tables: Vec<Bucketed>,
    /// Whether the keyspace holds `s3_ids`, which maps every carted object stored in S3
    s3_ids: bool,
    /// Whether each data type has data, as found so far
    exists: BTreeMap<String, bool>,
}

impl DataReader<'_> {
    /// Run a read, failing with the statement on error
    ///
    /// # Arguments
    ///
    /// * `cql` - The statement
    /// * `binds` - The values to bind
    async fn rows<V: SerializeRow>(
        &self,
        cql: &str,
        binds: V,
    ) -> Result<Vec<scylla::value::Row>, ApplyError> {
        // run the read and take every row
        let failed = |err: &dyn std::fmt::Display| ApplyError::Query {
            statement: cql.to_owned(),
            error: err.to_string(),
        };
        let result = self
            .session
            .query_unpaged(cql, binds)
            .await
            .map_err(|err| failed(&err))?
            .into_rows_result()
            .map_err(|err| failed(&err))?;
        let mut rows = Vec::with_capacity(result.rows_num());
        for row in result
            .rows::<scylla::value::Row>()
            .map_err(|err| failed(&err))?
        {
            rows.push(row.map_err(|err| failed(&err))?);
        }
        Ok(rows)
    }

    /// Whether at least one row of a data type exists
    ///
    /// # Arguments
    ///
    /// * `data_type` - The data type, such as `files` or `tags.repos`
    async fn exists(&mut self, data_type: &str) -> Result<bool, ApplyError> {
        // answer from what an earlier check found
        if let Some(found) = self.exists.get(data_type) {
            return Ok(*found);
        }
        // read one row from each table of the data type until one has a row
        let mut found = false;
        for table in &self.tables {
            let rows = match (&table.data_type, tag_type(data_type)) {
                // a table of the data type holds data when it holds any row
                (Some(owner), _) if owner == data_type => {
                    let cql = format!(
                        "SELECT {} FROM {}.{} LIMIT 1",
                        table.partition[0], self.keyspace, table.table
                    );
                    self.rows(&cql, ()).await?
                }
                // a tags table holds a tag type's data when a row has that type; checked only
                // when a tags setting changed, since it filters the whole table
                (None, Some(stored)) => {
                    let cql = format!(
                        "SELECT {TAG_TYPE_COLUMN} FROM {}.{} WHERE {TAG_TYPE_COLUMN} = ? LIMIT 1 \
                         ALLOW FILTERING",
                        self.keyspace, table.table
                    );
                    self.rows(&cql, (stored,)).await?
                }
                _ => continue,
            };
            // one row is enough
            if !rows.is_empty() {
                found = true;
                break;
            }
        }
        // remember the answer for the other checks of this run
        self.exists.insert(data_type.to_owned(), found);
        Ok(found)
    }

    /// Whether `s3_ids` maps at least one stored object, carted with the files password whether
    /// or not a listing row still names it
    async fn objects_mapped(&self) -> Result<bool, ApplyError> {
        // a keyspace without the table maps nothing
        if !self.s3_ids {
            return Ok(false);
        }
        // one row is enough
        let cql = format!(
            "SELECT sha256 FROM {}.{} LIMIT 1",
            self.keyspace,
            <S3IdsTable as ScyllaObject>::NAME
        );
        Ok(!self.rows(&cql, ()).await?.is_empty())
    }

    /// Sample rows of the tables whose data types are listed, spread over the token ring, and
    /// note each row whose bucket the configured size doesn't give
    ///
    /// # Arguments
    ///
    /// * `sizes` - The configured partition size of each data type
    /// * `data_types` - The data types to sample
    async fn sample(
        &self,
        sizes: &BTreeMap<String, u16>,
        data_types: &[String],
    ) -> Result<BTreeMap<String, Sample>, ApplyError> {
        // sample only the tables of a listed data type, or the tags tables for any tag type
        let mut samples: BTreeMap<String, Sample> = BTreeMap::new();
        let wanted = |table: &Bucketed| match &table.data_type {
            Some(data_type) => data_types.contains(data_type),
            None => data_types
                .iter()
                .any(|data_type| tag_type(data_type).is_some()),
        };
        for table in self.tables.iter().filter(|table| wanted(table)) {
            // read the stored year, bucket, timestamp, and tag type of rows in each token range
            let tag = if table.data_type.is_none() {
                format!(", {TAG_TYPE_COLUMN}")
            } else {
                String::new()
            };
            let cql = format!(
                "SELECT year, bucket, {}{tag} FROM {}.{} WHERE token({key}) >= ? AND \
                 token({key}) <= ? LIMIT {SAMPLE_ROWS}",
                table.timestamp,
                self.keyspace,
                table.table,
                key = table.partition.join(", ")
            );
            // split the token ring into equal ranges, the last one ending at the ring's end
            let step = (i128::from(i64::MAX) - i128::from(i64::MIN) + 1) / SAMPLE_RANGES;
            for range in 0..SAMPLE_RANGES {
                let start = i128::from(i64::MIN) + range * step;
                let end = if range + 1 == SAMPLE_RANGES {
                    i128::from(i64::MAX)
                } else {
                    start + step - 1
                };
                let bounds = (
                    i64::try_from(start).unwrap_or(i64::MIN),
                    i64::try_from(end).unwrap_or(i64::MAX),
                );
                for row in self.rows(&cql, bounds).await? {
                    // a row the read can't place is left out of the sample
                    let Some((data_type, year, bucket, timestamp)) =
                        sampled_row(table, row.columns)
                    else {
                        continue;
                    };
                    let (Some(size), true) =
                        (sizes.get(&data_type), data_types.contains(&data_type))
                    else {
                        continue;
                    };
                    // compare the stored bucket with the one the configured size gives
                    if partition(timestamp, year, *size) != bucket {
                        let seconds = seconds_into(timestamp, year).unwrap_or(-1);
                        samples
                            .entry(data_type)
                            .or_default()
                            .mismatch(seconds, bucket);
                    }
                }
            }
        }
        Ok(samples)
    }
}

/// Read one sampled row's data type, year, bucket, and timestamp, or nothing when the row can't
/// be placed, such as a stored year no date can hold
///
/// # Arguments
///
/// * `table` - The table the row came from
/// * `columns` - The row's columns: year, bucket, timestamp, and, for a tags table, the type
fn sampled_row(
    table: &Bucketed,
    columns: Vec<Option<CqlValue>>,
) -> Option<(String, i32, i32, DateTime<Utc>)> {
    // take the columns in the order the read selected them
    let mut columns = columns.into_iter();
    let Some(CqlValue::Int(year)) = columns.next()? else {
        return None;
    };
    // a year no date can hold has no bucket to compare
    NaiveDate::from_ymd_opt(year, 1, 1)?;
    let Some(CqlValue::Int(bucket)) = columns.next()? else {
        return None;
    };
    let Some(CqlValue::Timestamp(at)) = columns.next()? else {
        return None;
    };
    let timestamp = DateTime::from_timestamp_millis(at.0)?;
    // a tags row's data type comes from its stored tag type
    let data_type = match &table.data_type {
        Some(data_type) => data_type.clone(),
        None => match columns.next()? {
            Some(CqlValue::Text(stored)) => format!("tags.{}", stored.to_lowercase()),
            _ => return None,
        },
    };
    Some((data_type, year, bucket, timestamp))
}

/// One data-shaping check in progress: what's recorded, what's found, and what it decided
struct Check<'a> {
    /// Reads the keyspace's time-bucketed tables
    reader: DataReader<'a>,
    /// What an earlier apply recorded
    recorded: ConfigRecord,
    /// What to record instead of refusing
    accept: &'a ConfigAccept,
    /// Whether the apply only plans, so nothing it decides is written
    dry_run: bool,
    /// Receives a line for each setting recorded with a new value
    progress: &'a (dyn Fn(&str) + Sync),
    /// The rows to record and the warnings to report
    plan: ConfigPlan,
    /// Every setting refused so far
    refusals: Vec<ApplyError>,
}

impl Check<'_> {
    /// What a progress line says is done with a new value: it is recorded, or a dry run would
    fn recording(&self) -> &'static str {
        // a dry run writes nothing
        if self.dry_run {
            "would record"
        } else {
            "recording"
        }
    }

    /// Check every data type's partition size: an unrecorded one is recorded once its sampled rows
    /// match it, a changed one is refused while its data exists
    ///
    /// # Arguments
    ///
    /// * `sizes` - The configured partition size of each data type
    async fn sizes(&mut self, sizes: &BTreeMap<String, u16>) -> Result<(), ApplyError> {
        // sample the rows of every data type whose size isn't recorded yet
        let unrecorded = sizes
            .keys()
            .filter(|data_type| !self.recorded.partition_sizes.contains_key(*data_type))
            .cloned()
            .collect::<Vec<_>>();
        let samples = self.reader.sample(sizes, &unrecorded).await?;
        for (data_type, size) in sizes {
            let id = format!("{PARTITION_SIZE}{data_type}");
            match self.recorded.partition_sizes.get(data_type).copied() {
                // an unchanged size needs nothing
                Some(was) if was == *size => (),
                // a changed size is refused while its data exists, and recorded otherwise
                Some(was) => {
                    if self.reader.exists(data_type).await? {
                        self.refusals.push(ApplyError::ConfigChanged {
                            setting: setting_path(&id),
                            recorded: Some(was.to_string()),
                            configured: Some(size.to_string()),
                        });
                    } else {
                        (self.progress)(&format!(
                            "{} changed from {was} to {size} before any data was written; {} \
                             the new size",
                            setting_path(&id),
                            self.recording()
                        ));
                        self.plan.rows.push((id, size.to_string()));
                    }
                }
                // a size recorded for the first time must match the sampled rows
                None => match samples.get(data_type) {
                    Some(sample) if !self.accept.partition_sizes => {
                        self.refusals.push(ApplyError::SampledSizeMismatch {
                            data_type: data_type.clone(),
                            mismatched: sample.mismatched,
                            suggested: sample.suggested(),
                        });
                    }
                    Some(sample) => {
                        self.plan.warnings.push(format!(
                            "recorded {} as {size} although {} sampled rows sit in buckets it \
                             doesn't give",
                            setting_path(&id),
                            sample.mismatched
                        ));
                        self.plan.rows.push((id, size.to_string()));
                    }
                    None => self.plan.rows.push((id, size.to_string())),
                },
            }
        }
        Ok(())
    }

    /// Check every `earliest` value: an unrecorded or changed one is recorded, never refused,
    /// since it only hides older data from default listings; a raised one adds a warning
    ///
    /// # Arguments
    ///
    /// * `earliest` - The configured `earliest` value of each data type
    fn earliest(&mut self, earliest: BTreeMap<String, i64>) {
        // compare each value with the recorded one
        for (data_type, at) in earliest {
            let id = format!("{EARLIEST}{data_type}");
            match self.recorded.earliest.get(&data_type).copied() {
                // an unchanged value needs nothing
                Some(was) if was == at => (),
                // a changed value is recorded, and a raised one warns what it hides
                Some(was) => {
                    (self.progress)(&format!(
                        "{} changed from {was} to {at}; {} the new value",
                        setting_path(&id),
                        self.recording()
                    ));
                    if at > was {
                        self.plan
                            .warnings
                            .push(raised_earliest_warning(&data_type, at));
                    }
                    self.plan.rows.push((id, at.to_string()));
                }
                // a value recorded for the first time is recorded as configured
                None => self.plan.rows.push((id, at.to_string())),
            }
        }
    }

    /// Check the files password: with no identity recorded its identity is recorded, and with one
    /// recorded it must verify while any file or repo is stored or any object is mapped in
    /// `s3_ids`; no option accepts a change
    ///
    /// # Arguments
    ///
    /// * `password` - The configured files password
    async fn password(&mut self, password: &str) -> Result<(), ApplyError> {
        // record the identity of a password that has none yet
        if self.recorded.files_password.is_empty() {
            let identity = hash_files_password(password)?;
            self.plan.rows.push((FILES_PASSWORD.to_owned(), identity));
            return Ok(());
        }
        // verify against the recorded identity, refusing one with other parameters first
        match verify_files_password(&self.recorded.files_password, password) {
            // the same password needs nothing
            Ok(true) => (),
            // another password is refused while anything carted with the recorded one exists
            Ok(false) => {
                // stored files and repos, and objects only `s3_ids` still maps, were carted with
                // the recorded password
                if self.reader.exists("files").await?
                    || self.reader.exists("repos").await?
                    || self.reader.objects_mapped().await?
                {
                    self.refusals.push(ApplyError::ConfigChanged {
                        setting: setting_path(FILES_PASSWORD),
                        recorded: None,
                        configured: None,
                    });
                } else {
                    (self.progress)(
                        "thorium.files.password changed before any file or repo was stored",
                    );
                    let identity = hash_files_password(password)?;
                    self.plan.rows.push((FILES_PASSWORD.to_owned(), identity));
                }
            }
            // an identity with other parameters is refused without computing it
            Err(err) => self.refusals.push(err),
        }
        Ok(())
    }
}

/// Check the configured data-shaping settings against the record before the schema converges,
/// and decide what to record once it has
///
/// A setting the record lacks is recorded; a partition size only after sampling its rows finds
/// them in the buckets the configured size gives, unless `accept.partition_sizes`. A recorded
/// partition size or password that differs is refused while its data exists (for the password,
/// any stored file or repo, or object mapped in `s3_ids`) and recorded otherwise. A changed `earliest` value is always
/// recorded, with a warning when it was raised. Every refusal is returned together, before
/// anything is changed.
///
/// # Arguments
///
/// * `session` - The Scylla session to read with
/// * `conf` - The Thorium config
/// * `objects` - The Scylla objects the apply converges to, in creation order
/// * `has_table` - Whether the keyspace holds a table of this name
/// * `accept` - What to record instead of refusing
/// * `dry_run` - Whether the apply only plans, which the progress lines say
/// * `progress` - Receives a line for each setting recorded with a new value
pub(super) async fn plan(
    session: &Session,
    conf: &Conf,
    objects: &[ObjectDef],
    has_table: &(dyn Fn(&str) -> bool + Sync),
    accept: &ConfigAccept,
    dry_run: bool,
    progress: &(dyn Fn(&str) + Sync),
) -> Result<ConfigPlan, ApplyError> {
    // read what an earlier apply recorded, if the record table exists
    let recorded = if has_table(<SchemaMigrationsTable as ScyllaObject>::NAME) {
        read_recorded(session, conf).await?
    } else {
        ConfigRecord::from_rows(Vec::new())?
    };
    // read only the time-bucketed tables the keyspace already holds, and warn about the public
    // default password whatever is recorded
    let reader = DataReader {
        session,
        keyspace: &conf.thorium.namespace,
        tables: bucketed(objects)
            .into_iter()
            .filter(|table| has_table(table.table))
            .collect(),
        s3_ids: has_table(<S3IdsTable as ScyllaObject>::NAME),
        exists: BTreeMap::new(),
    };
    let mut check = Check {
        reader,
        recorded,
        accept,
        dry_run,
        progress,
        plan: ConfigPlan {
            rows: Vec::new(),
            warnings: default_password_warning(conf).into_iter().collect(),
        },
        refusals: Vec::new(),
    };
    // check the sizes, then the earliest values, then the password
    check.sizes(&ConfigRecord::partition_sizes_of(conf)).await?;
    check.earliest(ConfigRecord::earliest_of(conf));
    check.password(&conf.thorium.files.password).await?;
    // refuse every changed setting together, or none
    let mut refusals = check.refusals;
    match refusals.len() {
        0 => Ok(check.plan),
        1 => Err(refusals.remove(0)),
        _ => Err(ApplyError::Refused(refusals)),
    }
}
