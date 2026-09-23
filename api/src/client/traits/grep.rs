//! Traits defining shared behavior for grep based searches in the Thorium client

use super::GenericClient;
use crate::client::Error;
use crate::models::{GrepMatches, GrepOpts, GrepPage, GrepRequest};
use crate::send_build;
use crate::{add_date, add_query, add_query_bool, add_query_list};

/// Build the query params for a grep search from its options
///
/// Everything except the regex pattern rides in the query string; anything
/// unset is skipped so the server's defaults apply.
///
/// # Arguments
///
/// * `opts` - The grep options to build query params from
fn build_grep_query(opts: &GrepOpts) -> Vec<(String, String)> {
    // build our query params
    let mut query = Vec::with_capacity(8);
    add_query_bool!(query, "case_insensitive".to_owned(), opts.case_insensitive);
    add_query_bool!(query, "multi_line".to_owned(), opts.multi_line);
    add_query_list!(query, "groups[]".to_owned(), opts.groups);
    add_date!(query, "start".to_owned(), opts.start);
    add_date!(query, "end".to_owned(), opts.end);
    add_query!(query, "cursor".to_owned(), opts.cursor);
    add_query!(query, "limit".to_owned(), opts.limit);
    add_query_list!(query, "sources[]".to_owned(), opts.sources);
    add_query_list!(query, "tools[]".to_owned(), opts.tools);
    add_query!(query, "max_matches".to_owned(), opts.max_matches);
    add_query!(query, "scan_time".to_owned(), opts.scan_time);
    add_query!(query, "scan_idle".to_owned(), opts.scan_idle);
    add_query_bool!(query, "with_truncated".to_owned(), opts.with_truncated);
    query
}

/// A helper trait containing generic implementations for `GrepClient`
///
/// The functions are separated to allow for specific docs for each implementation
pub trait GrepClientHelper: GenericClient {
    /// Greps a page of items in Thorium
    ///
    /// # Arguments
    ///
    /// * `opts` - The options to use when grepping
    async fn grep_generic(&self, opts: GrepOpts) -> Result<GrepPage, Error> {
        // build the url for grepping a page of items
        let url = format!("{base}/grep/", base = self.base_url());
        // build the query params for this grep search
        let query = build_grep_query(&opts);
        // move the regex pattern into the request body
        let body = GrepRequest::from(opts);
        // build request
        let req = self
            .client()
            .post(&url)
            .header("authorization", self.token())
            .query(&query)
            .json(&body);
        // send this request and build a page of matches from the response
        send_build!(self.client(), req, GrepPage)
    }

    /// Greps a single item in Thorium
    ///
    /// # Arguments
    ///
    /// * `key` - The key of the item to grep
    /// * `opts` - The options to use when grepping
    async fn grep_key_generic<K: AsRef<str>>(
        &self,
        key: K,
        opts: GrepOpts,
    ) -> Result<GrepMatches, Error> {
        // build the url for grepping a single item
        let url = format!(
            "{base}/grep/{key}",
            base = self.base_url(),
            key = key.as_ref()
        );
        // build the query params for this grep search
        let query = build_grep_query(&opts);
        // move the regex pattern into the request body
        let body = GrepRequest::from(opts);
        // build request
        let req = self
            .client()
            .post(&url)
            .header("authorization", self.token())
            .query(&query)
            .json(&body);
        // send this request and build our matches from the response
        send_build!(self.client(), req, GrepMatches)
    }
}

/// Describes a client that is capable of grep based searches for a given Thorium data type
///
/// A client can implement these functions to provide specific docs for its implementation
#[cfg_attr(feature = "sync", thorium_derive::blocking_trait)]
#[allow(async_fn_in_trait)]
pub trait GrepClient {
    /// Greps a single page of items in Thorium
    ///
    /// # Arguments
    ///
    /// * `opts` - The options to use when grepping
    async fn grep(&self, opts: GrepOpts) -> Result<GrepPage, Error>;

    /// Greps a single item in Thorium
    ///
    /// # Arguments
    ///
    /// * `key` - The key of the item to grep
    /// * `opts` - The options to use when grepping
    async fn grep_key<K: AsRef<str>>(&self, key: K, opts: GrepOpts)
    -> Result<GrepMatches, Error>;
}
