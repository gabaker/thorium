//! Handles grep based searches, including creating/retrieving their cursors in redis

use tracing::{Level, event, instrument};

use super::GrepCursor;
use crate::models::backends::GrepSupport;
use crate::models::backends::grep::build_matcher;
use crate::models::{ApiCursor, GrepMatches, GrepParams, GrepStats, User};
use crate::utils::{ApiError, Shared};

/// Grep a page of items for a user
///
/// # Arguments
///
/// * `user` - The user that is grepping these items
/// * `params` - The query params for this grep search
/// * `requested_groups` - The groups the user explicitly requested before any defaults were applied
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::grep::grep", skip(user, shared), err(Debug))]
pub async fn grep<T: GrepSupport>(
    user: &User,
    params: GrepParams,
    requested_groups: Vec<String>,
    shared: &Shared,
) -> Result<ApiCursor<GrepMatches>, ApiError> {
    // compile this queries pattern once up front so an invalid pattern always
    // returns a 400 before any cursor state is created or touched
    let mut matcher = build_matcher(
        &params.pattern,
        params.case_insensitive,
        params.multi_line,
        &shared.config.thorium.grep,
    )?;
    // if we are searching no groups then just short circuit and return nothing
    if params.groups.is_empty() {
        // log that this user has no groups
        event!(Level::WARN, msg = "User has no groups");
        // return an empty cursor; grep responses always carry their document
        // counts, even when nothing was scanned
        return Ok(ApiCursor {
            cursor: None,
            data: Vec::default(),
            metadata: Some(GrepStats::default()),
        });
    }
    // get the earliest timestamp this type of item can exist at
    let earliest = T::grep_earliest(&shared.config);
    // get our cursor or build a new one scoped to this user
    let mut cursor =
        GrepCursor::from_params(user, params, requested_groups, earliest, shared).await?;
    // a continued cursor searches the pattern it was created with, so rebuild the
    // matcher from its retained params instead of the resubmitted ones
    if !cursor.new {
        matcher = build_matcher(
            &cursor.retain.pattern,
            cursor.retain.case_insensitive,
            cursor.retain.multi_line,
            &shared.config.thorium.grep,
        )?;
    }
    // grep the next page of items
    cursor.next::<T>(user, &matcher, shared).await?;
    // check if this cursor still has items left to search before we take its data
    let exhausted = cursor.exhausted();
    // save this cursor so it can be continued
    cursor.save(shared).await?;
    // take the matches this page found
    let data = std::mem::take(&mut cursor.data);
    // grep responses always carry this pages document counts
    let metadata = Some(cursor.stats);
    if exhausted {
        // this cursor is exhausted so omit the cursor id
        Ok(ApiCursor {
            cursor: None,
            data,
            metadata,
        })
    } else {
        // this cursor has more items to search so hand back its id
        Ok(ApiCursor {
            cursor: Some(cursor.id),
            data,
            metadata,
        })
    }
}
