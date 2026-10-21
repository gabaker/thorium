//! Reads one token range of a table a page at a time, resuming after a failed page
//!
//! The reader keeps the paging state of the last page it received. When a page fails to arrive
//! (a reset connection, a stalled node, a timeout), the same page is requested again from that
//! state under the retry policy, so no row of the range is lost and none is read twice.

use scylla::client::session::Session;
use scylla::deserialize::DeserializationError;
use scylla::deserialize::row::DeserializeRow;
use scylla::response::{PagingState, PagingStateResponse};
use scylla::statement::prepared::PreparedStatement;
use std::ops::ControlFlow;

use crate::Error;
use crate::shared::retry::RetryPolicy;

/// One page of rows: each row decoded, or the error that kept it from decoding
pub type Page<R> = Vec<Result<R, DeserializationError>>;

/// A range whose next page still failed after every attempt
#[derive(Debug)]
pub struct RangeReadError {
    /// The last attempt's error
    pub error: Error,
    /// How many attempts were made for the failed page, including the first
    pub attempts: u32,
    /// How many rows of the range were read before the failed page
    pub rows: u64,
}

impl std::fmt::Display for RangeReadError {
    /// Describes the failed page and how far into the range it was
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // the error, then how much of the range was read before it
        write!(
            f,
            "{} (the rest of the range after {} rows wasn't read)",
            self.error, self.rows
        )
    }
}

/// Reads the rows of one token range a page at a time
pub struct RangeReader<'a> {
    /// The session to read with
    session: &'a Session,
    /// The statement selecting the range; it binds the range's first and last token
    prepared: &'a PreparedStatement,
    /// The first and last token of the range
    range: (i64, i64),
    /// Where the next page starts, or `None` once the last page was read
    paging: Option<PagingState>,
    /// How failed pages are retried
    policy: RetryPolicy,
    /// How many rows were read so far
    rows: u64,
}

impl<'a> RangeReader<'a> {
    /// Starts reading a range from its first page
    ///
    /// # Arguments
    ///
    /// * `session` - The session to read with
    /// * `prepared` - The statement selecting the range by its first and last token
    /// * `range` - The first and last token of the range
    /// * `policy` - How failed pages are retried
    #[must_use]
    pub fn new(
        session: &'a Session,
        prepared: &'a PreparedStatement,
        range: (i64, i64),
        policy: RetryPolicy,
    ) -> Self {
        // the first page starts at the beginning of the range
        RangeReader {
            session,
            prepared,
            range,
            paging: Some(PagingState::start()),
            policy,
            rows: 0,
        }
    }

    /// Returns how many rows of the range were read so far
    #[must_use]
    pub fn rows(&self) -> u64 {
        // counted as each page arrives
        self.rows
    }

    /// Reads the next page of the range, or returns `None` once every page was read
    ///
    /// A page that fails to arrive is requested again from the same paging state; a page that
    /// still fails after the policy's attempts ends the range with the last error.
    pub async fn next_page<R>(&mut self) -> Result<Option<Page<R>>, RangeReadError>
    where
        R: for<'frame, 'metadata> DeserializeRow<'frame, 'metadata>,
    {
        // a range whose last page was read has nothing more
        let Some(paging) = self.paging.clone() else {
            return Ok(None);
        };
        // fetch the page, asking again from the same state while it fails for a transient reason
        let (session, prepared, range) = (self.session, self.prepared, self.range);
        let retried = self
            .policy
            .run(|| fetch_page::<R>(session, prepared, range, paging.clone()))
            .await;
        match retried.result {
            Ok((page, response)) => {
                // move past this page, or finish when the server has no more
                self.paging = match response.into_paging_control_flow() {
                    ControlFlow::Continue(next) => Some(next),
                    ControlFlow::Break(()) => None,
                };
                self.rows += page.len() as u64;
                Ok(Some(page))
            }
            Err(error) => Err(RangeReadError {
                error,
                attempts: retried.attempts,
                rows: self.rows,
            }),
        }
    }
}

/// Fetches one page of a range and decodes its rows
///
/// # Arguments
///
/// * `session` - The session to read with
/// * `prepared` - The statement selecting the range
/// * `range` - The first and last token of the range
/// * `paging` - Where the page starts
async fn fetch_page<R>(
    session: &Session,
    prepared: &PreparedStatement,
    range: (i64, i64),
    paging: PagingState,
) -> Result<(Page<R>, PagingStateResponse), Error>
where
    R: for<'frame, 'metadata> DeserializeRow<'frame, 'metadata>,
{
    // request exactly one page from the given state
    let (result, response) = session
        .execute_single_page(prepared, &range, paging)
        .await?;
    // a range query always returns rows; anything else can't be read and won't change
    let rows = result
        .into_rows_result()
        .map_err(|error| Error::new(format!("the range returned no rows: {error}")))?;
    // a page whose columns don't suit the row type won't change either
    let typed = rows
        .rows::<R>()
        .map_err(|error| Error::new(format!("the range's rows can't be read: {error}")))?;
    Ok((typed.collect(), response))
}
