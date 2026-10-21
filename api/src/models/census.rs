//! The traits used for taking a census of data in Thorium

use scylla::client::session::Session;
use scylla::deserialize::row::DeserializeRow;
use scylla::errors::PrepareError;
use scylla::statement::prepared::PreparedStatement;
use std::fmt::Debug;

/// The census keys for both the count and stream key
#[derive(Debug, Clone)]
pub struct CensusKeys {
    /// The stream key
    pub stream: String,
    /// The bucket for these keys
    pub bucket: i32,
}

/// The trait used for taking a census of data
pub trait CensusSupport: 'static + Send {
    /// The type returned by our prepared statement
    type Row: 'static + Send + Debug + for<'frame, 'metadata> DeserializeRow<'frame, 'metadata>;

    /// The text of the census statement generated for this table, which counts the rows of each
    /// partition in a token range
    ///
    /// # Arguments
    ///
    /// * `ns` - The namespace the table is in
    fn census_cql(ns: &str) -> String;

    /// Prepare the census statement, which binds the first and last token of a range
    ///
    /// # Arguments
    ///
    /// * `scylla` - The session to prepare the statement with
    /// * `ns` - The namespace the table is in
    #[allow(async_fn_in_trait)]
    async fn scan_prepared_statement(
        scylla: &Session,
        ns: &str,
    ) -> Result<PreparedStatement, PrepareError> {
        // prepare the generated statement
        scylla.prepare(Self::census_cql(ns)).await
    }

    /// Get the count for this partition
    fn get_count(row: &Self::Row) -> i64;

    /// Get the bucket for this partition
    fn get_bucket(row: &Self::Row) -> i32;

    /// Build the sorted set key for this census operation from a census scan row
    fn stream_key_from_row(namespace: &str, row: &Self::Row) -> String;
}
