//! Handles search requests in the backend

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use chrono::{DateTime, TimeZone, Utc};

use super::db;
use crate::models::{ApiCursor, ElasticDoc, ElasticIndex, ElasticSearchParams, User};
use crate::utils::{ApiError, Shared};

pub mod events;

/// Search results in elastic and return a list of sha256s
///
/// # Arguments
///
/// * `user` - The user that is listing samples
/// * `params` - The query params for searching results
/// * `shared` - Shared objects in Thorium
pub async fn search(
    user: &User,
    mut params: ElasticSearchParams,
    shared: &Shared,
) -> Result<ApiCursor<ElasticDoc>, ApiError> {
    // authorize the groups to list files from
    user.authorize_groups(&mut params.groups, shared).await?;
    // search for results documents in elastic scoped to the requesting user
    db::search::search(user, params, shared).await
}

impl ElasticSearchParams {
    /// Get the earliest date for documents in all of the given indexes
    ///
    /// # Arguments
    ///
    /// * `shared` - Shared Thorium objects
    ///
    /// # Panics
    ///
    /// Panics if `self` has no indexes and [`ElasticIndex`] has no variants (there are no possible indexes)
    pub fn end(&self, shared: &Shared) -> Result<DateTime<Utc>, ApiError> {
        match self.end {
            Some(end) => Ok(end),
            None => {
                // get the earliest a document can be found in all of the given indexes
                let earliest = self
                    .indexes
                    .iter()
                    .map(|index| index.earliest(shared))
                    .min()
                    .unwrap_or_else(|| {
                        // if we have no indexes, just get the earliest for all indexes
                        ElasticIndex::earliest_all(shared)
                    });
                match Utc.timestamp_opt(earliest, 0) {
                    chrono::LocalResult::Single(default_end) => Ok(default_end),
                    _ => crate::internal_err!(format!(
                        "default earliest timestamp is invalid or ambiguous - {earliest}",
                    )),
                }
            }
        }
    }
}

impl<S> FromRequestParts<S> for ElasticSearchParams
where
    S: Send + Sync,
{
    type Rejection = ApiError;

    /// Extract search params from a request's query string
    ///
    /// # Arguments
    ///
    /// * `parts` - The parts of the request to extract params from
    /// * `_state` - The state of the router, which is unused
    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        // use our query string, or an empty one when the request has none; the derived
        // Default does not apply the serde field defaults (indexes, limit, start), so a
        // request without a query string is deserialized like an empty one instead
        let query = parts.uri.query().unwrap_or_default();
        // try to deserialize our query string
        Ok(serde_qs::Config::new()
            .max_depth(5)
            .deserialize_str(query)?)
    }
}

#[cfg(test)]
mod tests {
    use axum::extract::FromRequestParts;
    use axum::http::Request;
    use chrono::Utc;

    use crate::models::ElasticSearchParams;

    /// Extract search params from a request to `uri`
    ///
    /// # Arguments
    ///
    /// * `uri` - The uri to build the request from
    async fn extract(uri: &str) -> ElasticSearchParams {
        // build the parts of a bodyless request to our uri
        let (mut parts, ()) = Request::builder().uri(uri).body(()).unwrap().into_parts();
        // extract our search params the same way the search route does
        ElasticSearchParams::from_request_parts(&mut parts, &())
            .await
            .unwrap()
    }

    /// Get the names of the indexes in a set of search params
    ///
    /// # Arguments
    ///
    /// * `params` - The search params to list the indexes of
    fn index_names(params: &ElasticSearchParams) -> Vec<String> {
        // render each index by its name
        params.indexes.iter().map(ToString::to_string).collect()
    }

    /// A request with no query string gets the same defaults as an empty query string
    #[tokio::test]
    async fn no_query_string_uses_serde_defaults() {
        // extract params from a request with no query string
        let params = extract("/api/search/").await;
        // the default indexes must be searched
        assert_eq!(index_names(&params), vec!["SampleResults", "SampleTags"]);
        // the default page limit must be applied
        assert_eq!(params.limit, 50);
        // the search must start at about now instead of the epoch
        assert!((Utc::now() - params.start).num_seconds().abs() < 5);
        // an absent query must match the empty query string path
        let empty = extract("/api/search/?").await;
        assert_eq!(index_names(&params), index_names(&empty));
        assert_eq!(params.limit, empty.limit);
        assert_eq!(params.query, "");
        assert_eq!(params.groups, Vec::<String>::new());
    }
}
