//! Saves files into the backend

use chrono::prelude::*;
use futures::{StreamExt, TryStreamExt, stream};
use itertools::Itertools;
use std::collections::hash_map::Entry::{Occupied, Vacant};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;
use tracing::{Level, event, instrument};
use uuid::Uuid;

use super::ScyllaCursor;
use crate::models::backends::TagSupport;
use crate::models::backends::db::ScyllaTagCountCursor;
use crate::models::schema::JsonText;
use crate::models::schema::scylla::{CommentsTableRef, SamplesListTableRef};
use crate::models::{
    BasicSubmissionRow, Comment, CommentForm, Event, FileListParams, OutputKind, ResultSearchEvent,
    S3Objects, Sample, SampleCheck, SampleCheckResponse, SampleForm, SampleListLine,
    SampleSubmissionResponse, Submission, SubmissionChunk, SubmissionUpdate, TagDeleteRequest,
    TagRequest, TagSearchEvent, User,
};
use crate::utils::s3::StandardHashes;
use crate::utils::{ApiError, Shared, helpers};
use crate::{
    conflict, deserialize, for_groups, internal_err, log_scylla_err, not_found, same_vec,
    unauthorized,
};

/// Check if a specific submission already exists in the DB
///
/// # Arguments
///
/// * `user` - The user that is checking if a file exists or not
/// * `check` - The info to check with
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::files::exists", skip(user, shared), err(Debug))]
pub async fn exists(
    user: &User,
    check: &SampleCheck,
    shared: &Shared,
) -> Result<SampleCheckResponse, ApiError> {
    // get this sample if it already exists
    if let Some(sample) = for_groups!(get, user, shared, user, &check.sha256)? {
        // this sample already exists so check if we should patch one of our existing samples
        // this must be for the same groups, user, name, and origin otherwise we could be leaking info
        if let Some(sub) = sample.submissions.into_iter().find(|sub| {
            sub.submitter == user.username
                && sub.origin == check.origin
                && sub.name == check.name
                && same_vec!(&sub.groups, &check.groups)
        }) {
            // This submission already exists so just upload that
            return Ok(SampleCheckResponse {
                exists: true,
                id: Some(sub.id),
            });
        }
        return Ok(SampleCheckResponse {
            exists: true,
            id: None,
        });
    }
    Ok(SampleCheckResponse {
        exists: false,
        id: None,
    })
}

/// Check if a specific submission already exists in the DB
///
/// # Arguments
///
/// * `user` - The user that is checking if a file exists or not
/// * `check` - The info to check with
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::files::sha256_exists", skip(shared), err(Debug))]
pub async fn sha256_exists(
    groups: &[String],
    sha256: &str,
    shared: &Shared,
) -> Result<bool, ApiError> {
    // check if this sample exists in any of the groups we can see, 50 groups at a time to
    // stay under the cartesian product limit, stopping at the first chunk that sees it
    let exists = shared
        .scylla
        .queries
        .samples
        .auth_any(&shared.scylla.session, &[sha256.to_owned()], groups)
        .await?;
    Ok(exists)
}

/// Adds a child sample to its originating result
///
/// # Arguments
///
/// * `sha256` - The sha256 that is being ingested as a child sample
/// * `submission` - The submission id of the child sample being ingested
/// * `results` - The result ids to add this child under
/// * `shared` - Shared Thorium objects
/// * `req_id` - The uuid for this request
#[instrument(name = "db::files::add_child", skip(shared), err(Debug))]
async fn add_child(
    sha256: &str,
    submission: &Uuid,
    results: &[Uuid],
    shared: &Shared,
) -> Result<(), ApiError> {
    // skip updating result children if none are passed
    if !results.is_empty() {
        // instance an empty result map to insert any retrieved rows into
        let mut temp = HashMap::with_capacity(results.len());
        // get every result's upload time in one statement
        let rows = shared
            .scylla
            .queries
            .results
            .get_uploaded_by_id_chunk(&shared.scylla.session, results)
            .await?;
        // crawl and consume these typed rows
        for typed in rows {
            // check if we ran into a problem casting this row
            let row = typed?;
            // add this to our temp map
            temp.insert(row.id, row.uploaded);
        }
        // add this child to the target results
        for (result_id, uploaded) in temp.into_iter() {
            // update our child field in this result
            shared
                .scylla
                .queries
                .results
                .update_children(
                    &shared.scylla.session,
                    sha256,
                    *submission,
                    result_id,
                    uploaded,
                )
                .await?;
        }
    }
    Ok(())
}

/// Saves a file to the backend
///
/// # Arguments
///
/// * `user` - The user who is saving this file
/// * `upload` - The sample to save to the backend
/// * `shared` - Shared Thorium objects
/// * `span` - The span to log traces under
#[rustfmt::skip]
#[instrument(name = "db::files::create", skip(user, form, shared), err(Debug))]
pub async fn create(
    user: &User,
    mut form: SampleForm,
    hashes: StandardHashes,
    shared: &Shared,
) -> Result<SampleSubmissionResponse, ApiError> {
    // get our origin if one was set
    let (origin, results) = form.origin.to_origin()?;
    // serialize the origin
    let origin_str = origin.serialize()?;
    // add the user that is uploading this as a tag
    form.tags.entry("submitter".to_owned()).or_default().insert(user.username.clone());
    // get the partition size to use for files and tags
    let chunk = shared.config.thorium.files.partition_size;
    // If this submission doesn't exist then use the current time as its upload time
    let now = Utc::now();
    // get this sample if it already exists and build an earliest seen map
    let mut earliest = if let Some(sample) = for_groups!(get, user, shared, user, &hashes.sha256)? {
        // this sample already exists so check if we should return a conflict error
        if let Some(sub)  = sample.submissions.iter()
            .find(|sub| sub.submitter == user.username && sub.origin == origin
                && sub.description == form.description && sub.name == form.file_name
                && same_vec!(&sub.groups, &form.groups)) {
                // upload all our user specified samples tags regardless of if this has been seen before or not
                // get the earliest time this repo was uploaded for each group
                let earliest = sample.earliest();
                // build a tag request
                let mut req = TagRequest::<Sample>::default().groups(form.groups.clone());
                // move our tags over to our tag request
                req.tags = form.tags;
                // save our files tags to scylla
                super::tags::create(
                    user,
                    hashes.sha256.clone(),
                    req,
                    &earliest,
                    shared,
                )
                .await?;
                // add this child sample to its result if any were set
                add_child(&hashes.sha256, &sub.id, &results, shared).await?;
                // this user has already uploaded this same submission so return a conflict error
                return conflict!(hashes.sha256);
        }
        // get the earliest timestamp for each group that we can see this sample was submitted at
        sample.earliest_owned()
    } else {
        // build a map of the current timestamp for each group
        HashMap::with_capacity(form.groups.len())
    };
    // add any new groups
    for group in form.groups.iter() {
        earliest.entry(group.to_owned()).or_insert_with(|| now);
    }
    // get the current year and month so we can bucket this sample by time
    let year = now.year();
    let bucket = helpers::partition(now, year, chunk);
    let id = Uuid::new_v4();
    // save submission objects into scylla
    // currently do it one at a time instead of with buffered_unordered to work around Fn Once
    for group in form.groups.iter() {
        // build this group's submission row from borrowed values
        let row = SamplesListTableRef {
            group,
            year,
            bucket,
            uploaded: now,
            id,
            sha256: &hashes.sha256,
            sha1: &hashes.sha1,
            md5: &hashes.md5,
            name: form.file_name.as_deref(),
            description: form.description.as_deref(),
            submitter: &user.username,
            origin: origin_str.as_deref(),
        };
        shared.scylla.queries.samples_list.insert(&shared.scylla.session, &row).await?;
    }
    // build the keys for this items census cache
    let keys = super::keys::samples::census_keys(&form.groups, year, bucket, shared);
    // update this samples census cache info
    super::census::incr_cache(keys, shared).await?;
    // add our origin tags to our tags map
    origin.get_tags(&mut form.tags);
    // build a tag request
    let mut req = TagRequest::<Sample>::default().groups(form.groups.clone());
    // move our tags over to our tag request
    req.tags = form.tags;
    // save our files tags to scylla
    super::tags::create_owned(
        user,
        hashes.sha256.clone(),
        req,
        &earliest,
        shared,
    )
    .await?;
    // add this child sample to its result if any were set
    add_child(&hashes.sha256, &id, &results, shared).await?;
    // create our new sample event
    let event = Event::new_sample(user, form.groups.clone(), hashes.sha256.clone(), form.trigger_depth);
    // save our event
    super::events::create(&event, shared).await?;
    // build our submission response object
    let resp = SampleSubmissionResponse { sha256: hashes.sha256, sha1: hashes.sha1, md5: hashes.md5, id };
    Ok(resp)
}

/// Authorizes that a user has access to a list of samples
///
/// # Arguments
///
/// * `groups` - The groups the user is in
/// * `sha256` - The Sha256 to authorize we have access to
/// * `shared` - Shared Thorium objects
/// * `span` - The span to log traces under
#[instrument(name = "db::files::authorize", skip(shared), err(Debug))]
pub async fn authorize(
    groups: &Vec<String>,
    sha256s: &Vec<String>,
    shared: &Shared,
) -> Result<(), ApiError> {
    // if we specified no groups then we do not have acess to this sample
    if groups.is_empty() {
        return unauthorized!();
    }
    // check if our cartesian product will be under 100
    if groups.len() * sha256s.len() < 100 {
        // our cartesian product will be low enough so just query for them all at once
        // get the number of tools or default to 10
        // check if any of our groups have access to this sample
        let rows = shared
            .scylla
            .queries
            .samples
            .auth_chunk(&shared.scylla.session, sha256s, groups)
            .await?;
        //  make sure we got back the same number of sha256s
        if rows.len() != sha256s.len() {
            return unauthorized!();
        }
    } else {
        // track the sha256s we have authed
        let mut authed = HashSet::with_capacity(sha256s.len());
        // build an iter over the cartesian product of our groups and sha256s
        // doing this in a buffered_unordered closure would probably be faster
        // but that runs into lifetime errors currently :(
        for (groups_chunk, sha256s_chunk) in groups[..]
            .chunks(50)
            .cartesian_product(sha256s[..].chunks(50))
        {
            // send a query to check if we have access to these samples
            let rows = shared
                .scylla
                .queries
                .samples
                .auth_chunk(&shared.scylla.session, sha256s_chunk, groups_chunk)
                .await?;
            // cast our rows to the right type
            for typed_row in rows {
                // check if we failed to cast this row
                let row = typed_row?;
                // add this sha256 to our authed samples
                authed.insert(row.sha256);
            }
            // if we have authed all of our sha256s then return early
            if authed.len() == sha256s.len() {
                // we have authed all of the sha256s we wanted to auth against
                return Ok(());
            }
        }
        // if we have not found all of our sha256s then we do not have access
        if authed.len() != sha256s.len() {
            return unauthorized!();
        }
    }
    // we have access to this sample
    Ok(())
}

/// Gets all the comments for a sample
///
/// # Arguments
///
/// * `groups` - The groups to restrict our returned comments too
/// * `sha256` - The sha256 to get comments for
/// * `list` - The vector to add our tags too
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::files::get_comments", skip(list, shared), err(Debug))]
async fn get_comments(
    groups: &Vec<String>,
    sha256: &str,
    list: &mut Vec<Comment>,
    shared: &Shared,
) -> Result<(), ApiError> {
    // get the comments in chunks of 100 groups, merging each chunk's rows on their own
    for chunk in groups.chunks(100) {
        // get this chunks data
        let rows = shared
            .scylla
            .queries
            .comments
            .get_chunk(&shared.scylla.session, chunk, sha256)
            .await?;
        // build a btreemap to collect all comments and then sort them
        let mut map: BTreeMap<DateTime<Utc>, HashMap<Uuid, Comment>> = BTreeMap::default();
        // crawl over rows and add them to our tag map while logging any errors
        rows.into_iter()
            .filter_map(|res| log_scylla_err!(res))
            .for_each(|comment| {
                // get an entry to the nested map of comments by id
                let id_map = map.entry(comment.uploaded).or_default();
                // get our comments doc so we can add a group or insert it
                match id_map.entry(comment.id) {
                    // this comment has already been added so just add a new group to it
                    Occupied(entry) => entry.into_mut().groups.push(comment.group),
                    Vacant(entry) => {
                        // try to turn this row into a comment
                        let res = Comment::try_from(comment);
                        // if we can deserialize this string then insert it
                        if let Some(cast) = log_scylla_err!(res) {
                            entry.insert(cast);
                        }
                    }
                }
            });
        // flatten our map of comments and append it to our list of comments
        list.extend(map.into_values().flat_map(|map| map.into_values()));
    }
    Ok(())
}

/// Gets all submissions for a specific set of groups and a sha256 and cast it to a sample
///
/// # Arguments
///
/// * `groups` - The groups to search in
/// * `sha256` - The sha256 to retrieve submission docs for
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::files::get", skip(shared), err(Debug))]
pub async fn get(
    groups: &Vec<String>,
    user: &User,
    sha256: &str,
    shared: &Shared,
) -> Result<Option<Sample>, ApiError> {
    // build a btree to sort our submissions
    let mut sorted: BTreeMap<DateTime<Utc>, Vec<Submission>> = BTreeMap::default();
    // get every submission in chunks of 100 groups
    let rows = shared
        .scylla
        .queries
        .samples
        .get(&shared.scylla.session, sha256, groups)
        .await?;
    // crawl over rows and add them to our tag map while logging any errors
    rows.into_iter()
        .filter_map(|res| log_scylla_err!(res))
        .for_each(|sub| {
            // get an entry to the list for this timestamp
            let entry = sorted.entry(sub.uploaded).or_default();
            // crawl the entries for this timestamp and dedupe entries
            if let Some(pos) = entry.iter().position(|item| sub.id == item.id) {
                entry[pos].groups.push(sub.group);
            } else {
                // add this new submission to this timestamps list
                entry.push(Submission::from(sub));
            }
        });
    // get an iter of these submissions in descending order
    let mut descending = sorted.into_iter().rev().flat_map(|(_, sub)| sub);
    // get the first submission object and cast it to a sample
    if let Some(sub) = descending.next() {
        // cast this submission object to a sample
        let mut sample = Sample::try_from_submission(groups, sub, shared).await?;
        // crawl the remaining submission objects and add them in parallel
        let mut submissions = stream::iter(descending)
            .map(|sub| SubmissionChunk::get(groups, sub, shared))
            .buffered(10)
            .try_collect::<Vec<SubmissionChunk>>()
            .await?;
        // add all of our retrieved submissions
        sample.submissions.append(&mut submissions);
        // get the tags and comments for this sample
        sample.get_tags(groups, shared).await?;
        get_comments(groups, sha256, &mut sample.comments, shared).await?;
        // return our sample
        return Ok(Some(sample));
    };
    Ok(None)
}

async fn still_visible(sample: &Sample, shared: &Shared) -> Result<bool, ApiError> {
    // we need to see all groups so use the thorium user
    let user = User::force_get("thorium", shared).await?;
    // build a sampel check query for this sample
    let check = SampleCheck::new(sample.sha256.clone());
    // see if this sample is visible in any group
    let resp = exists(&user, &check, shared).await?;
    // return if this sample exists or not
    Ok(resp.exists)
}

/// Deletes a submission from a sample
///
/// # Arguments
///
/// * `sample` - The sample to delete a submission from
/// * `submission` - The submission to delete
/// * `groups` - The groups to delete this submission from
/// * `shared` - Shared Thorium objects
/// * `span` - The span to log traces under
#[instrument(name = "db::files::delete_submission", skip(shared), err(Debug))]
pub async fn delete_submission(
    sample: &Sample,
    sub: &SubmissionChunk,
    groups: &Vec<String>,
    shared: &Shared,
) -> Result<(), ApiError> {
    // get the target submissions year/hour of being submitted
    let year = sub.uploaded.year();
    // get the chunk size for files
    let chunk_size = shared.config.thorium.files.partition_size;
    // determine what bucket to add data too
    let bucket = helpers::partition(sub.uploaded, year, chunk_size);
    // delete the submissions from the db in chunks of 100 groups
    shared
        .scylla
        .queries
        .samples_list
        .delete_multiple_groups(
            &shared.scylla.session,
            groups,
            year,
            bucket,
            sub.uploaded,
            sub.id,
        )
        .await?;
    // sleep for a second to ensure scylla has time to become consistent
    // this isn't very performant but deletes should be relatively rare and you can
    // perform then in parallel to work around performance issues
    tokio::time::sleep(Duration::from_secs(1)).await;
    // get the groups and submitters of other submissions for this sample
    let group_submitter_map = other_submissions(&sample.sha256, sub, groups, shared).await?;
    // prune submitter tags
    prune_submitter_tags(
        &sample.sha256,
        groups,
        &sub.submitter,
        &group_submitter_map,
        shared,
    )
    .await?;
    // if no other submisisons exist then just clean up all data otherwise we
    // need to prune data from groups that no longer have access to prevent
    // leaking other groups also can see this sample
    let prunes: Vec<String> = if group_submitter_map.is_empty() {
        // no other groups have access to this sample so delete all of its data
        sample.groups().into_iter().map(ToOwned::to_owned).collect()
    } else {
        // other groups have access to this data so determine if there are any
        // groups that no longer have access and should have there access pruned
        groups
            .iter()
            .filter(|group| !group_submitter_map.contains_key(*group))
            .map(ToOwned::to_owned)
            .collect()
    };
    // if any groups need their access pruned then do it
    if !prunes.is_empty() {
        // prune access for our target groups
        prune_access(sample, &prunes, shared).await?;
    }
    // if no other groups still have access to this data then determine if anyone in Thorium
    // including those we can't see still have access to this sample
    if group_submitter_map.is_empty() {
        // check to see if this sample is still visible to others
        if !still_visible(sample, shared).await? {
            // get the s3 id for this object
            let s3_id = super::s3::get_s3_id(S3Objects::File, &sample.sha256, shared).await?;
            // no one else has access so prune this samples data
            shared.s3.files.delete(&s3_id.to_string()).await?;
            // also delete this from the s3 object id table
            super::s3::delete(S3Objects::File, s3_id, shared).await?;
        }
    }
    Ok(())
}

/// Determine if any other submissions for this sha256 exist
/// and return a map of the submission groups and submitters
///
/// # Arguments
///
/// * `sha256` - The sha256 to check for other submissions for
/// * `sub` - The submission that was deleted
/// * `groups` - The groups that the submission was deleted from
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::files::other_submissions", skip(shared), err(Debug))]
async fn other_submissions(
    sha256: &str,
    sub: &SubmissionChunk,
    groups: &Vec<String>,
    shared: &Shared,
) -> Result<HashMap<String, HashSet<String>>, ApiError> {
    // get the groups and submitters for the sample
    let rows = shared
        .scylla
        .queries
        .samples
        .get_basic_submission_info(&shared.scylla.session, sha256)
        .await?;
    // size our map correctly for the number of rows returned
    let mut map: HashMap<String, HashSet<String>> = HashMap::with_capacity(rows.len());
    // cast our rows to typed values
    for row in rows {
        // try to cast our row to the group, submitter, and id
        let BasicSubmissionRow {
            group,
            submitter,
            id,
        } = row?;
        // ignore row if it was supposed to be deleted, but the db
        // hasn't yet updated to reflect the delete
        if id == sub.id && groups.contains(&group) {
            continue;
        }
        // add this group's submitter to our map
        map.entry(group).or_default().insert(submitter);
    }
    return Ok(map);
}

/// Prune submitter tag if submitter no longer has a submission in the groups
///
/// # Arguments
///
/// * `sha256` - The sample sha256
/// * `groups` - The list of groups that were deleted from
/// * `submitter` - The submitter of the deleted submission
/// * `group_submitter_map` - A map of groups to submitters after deletion
/// * `shared` - Shared Thorium objects
#[instrument(
    name = "db::files::prune_submitter_tags",
    skip(group_submitter_map, shared),
    err(Debug)
)]
async fn prune_submitter_tags(
    sha256: &String,
    groups: &Vec<String>,
    submitter: &String,
    group_submitter_map: &HashMap<String, HashSet<String>>,
    shared: &Shared,
) -> Result<(), ApiError> {
    // identify all groups to which the submitter no longer has submissions
    let mut dangling_groups: HashSet<&String> = HashSet::default();
    for group in groups {
        if let Some(submitters) = group_submitter_map.get(group) {
            if !submitters.contains(submitter) {
                dangling_groups.insert(group);
            }
        } else {
            dangling_groups.insert(group);
        }
    }
    // delete the submitter tag for the submitter and groups
    if !dangling_groups.is_empty() {
        let tag_deletes: TagDeleteRequest<Sample> = TagDeleteRequest::default()
            .groups(dangling_groups)
            .add("submitter", submitter);
        super::tags::delete(sha256, &tag_deletes, shared).await?;
    }
    Ok(())
}

/// Prune access to any sample info for the target groups
///
/// This will remove access to any tags, comments, and results.
///
/// # Arguments
///
/// * `sample` - The sample to remove access too
/// * `groups` - The groups that will get pruned
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::files::prune_access", skip(sample, shared), err(Debug))]
async fn prune_access(
    sample: &Sample,
    groups: &Vec<String>,
    shared: &Shared,
) -> Result<(), ApiError> {
    // log the sha256 we are pruning access too
    event!(Level::INFO, sha256 = &sample.sha256);
    // build our tag delete object with empty groups to delete from all
    let mut tag_deletes: TagDeleteRequest<Sample> = TagDeleteRequest::default().groups(groups);
    // delete all tag rows for our groups + sample
    for (key, val_map) in &sample.tags {
        // add all values for this key
        tag_deletes.add_values_ref(key, val_map.keys());
    }
    // delete the requested tags for this repo if they exist
    super::tags::delete(&sample.sha256, &tag_deletes, shared).await?;
    // delete all comment rows for our groups + sample
    for comment in &sample.comments {
        // delete the rows for this comment
        delete_comment(&sample.sha256, groups, comment, shared).await?;
    }
    // prune comment attachments now that the comments are deleted
    prune_comment_attachments(&sample.comments, &sample.sha256, shared).await?;
    // prune access to results for this sample
    super::results::delete(OutputKind::Files, groups, &sample.sha256, shared).await?;
    // create events to delete search info for this sample in the pruned groups
    let tag_event = TagSearchEvent::deleted::<Sample>(sample.sha256.clone(), groups.clone());
    let result_event = ResultSearchEvent::deleted::<Sample>(sample.sha256.clone(), groups.clone());
    if let Err(err) = tokio::try_join!(
        super::search::events::create(tag_event, shared),
        super::search::events::create(result_event, shared)
    ) {
        return internal_err!(format!("Failed to create search event: {err}"));
    }
    Ok(())
}

/// List samples for specific groups
///
/// # Arguments
///
/// * `user` - The user that is listing files
/// * `params` - The query params to use when listing files
/// * `dedupe` - Whether to dedupe submissions for the same sha256
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::files::list", skip(user, shared), err(Debug))]
pub async fn list(
    user: &User,
    params: FileListParams,
    dedupe: bool,
    shared: &Shared,
) -> Result<ScyllaCursor<SampleListLine>, ApiError> {
    // get our cursor scoped to the creating user
    let mut cursor = ScyllaCursor::from_params(params, &user.username, dedupe, shared).await?;
    // get the next page of data for this cursor
    cursor.next(shared).await?;
    // save this cursor
    cursor.save(shared).await?;
    Ok(cursor)
}

/// List samples for a grep search without saving the list cursor
///
/// Grep cursors embed the retained state of their inner list cursor, so the cursor
/// built here is handed back unsaved for its owner to persist atomically. A resumed
/// cursor lists only the saved groups the params may still list.
///
/// # Arguments
///
/// * `user` - The user that is listing files
/// * `params` - The query params to use when listing files
/// * `retain` - The serialized retained state of the list cursor to resume
/// * `shared` - Shared Thorium objects
#[instrument(
    name = "db::files::list_greppable",
    skip(user, retain, shared),
    err(Debug)
)]
pub async fn list_greppable(
    user: &User,
    params: FileListParams,
    retain: Option<String>,
    shared: &Shared,
) -> Result<ScyllaCursor<SampleListLine>, ApiError> {
    // resume our cursor from its retained state or build a new one from our params
    let mut cursor = match retain {
        // narrow the resumed cursor to the groups this page was authorized for
        Some(retain) => {
            ScyllaCursor::from_retain(deserialize!(&retain), params, &user.username, true, shared)?
        }
        None => ScyllaCursor::from_params(params, &user.username, true, shared).await?,
    };
    // get the next page of data for this cursor
    cursor.next(shared).await?;
    Ok(cursor)
}

/// Gets details on a specific samples by group and sha256
///
/// # Arguments
///
/// * `groups` - The groups to search in
/// * `sha256` - The sha256 to retrieve submission docs for
/// * `shared` - Shared Thorium objects
/// * `req_id` - The uuid for this request
#[instrument(name = "db::files::list_details", skip(shared), err(Debug))]
pub async fn list_details(
    groups: &Vec<String>,
    sha256s: Vec<String>,
    shared: &Shared,
) -> Result<Vec<Sample>, ApiError> {
    // build a btreemap to store the order samples should be returned in
    let mut sorted: BTreeMap<DateTime<Utc>, Vec<String>> = BTreeMap::default();
    // build a hashmap to store our sha256s in
    let mut map = HashMap::with_capacity(sha256s.len());
    // preallocate space for the futures we are going to create
    let mut futures = Vec::with_capacity(100);
    // split our groups and samples into chunks of 50
    for (sha256s_chunk, groups_chunk) in sha256s.chunks(50).cartesian_product(groups.chunks(50)) {
        // build a query to get this chunks data
        let query = shared.scylla.queries.samples.get_many_chunk(
            &shared.scylla.session,
            sha256s_chunk,
            groups_chunk,
        );
        // add this future to our future set
        futures.push(query);
    }
    // build our stream of futures
    let mut stream = stream::iter(futures.drain(..)).buffer_unordered(50);
    // start getting our details from our queries
    while let Some(query) = stream.next().await {
        // unwrap our query
        let rows = query?;
        // crawl the rows returned by this query
        for cast in rows {
            // get this row if possible
            let row = cast?;
            // get an entry to this submissions timestamp list
            let entry = sorted.entry(row.uploaded).or_default();
            // add this submission to the btreemap
            entry.push(row.sha256.clone());
            // try to get a mutable entry to this submissions sample
            match map.entry(row.sha256.clone()) {
                // we do not yet have an entry for this sha256
                Vacant(entry) => {
                    // create a sample entry for this submission
                    let sample = Sample::try_from(row)?;
                    // add this sample to the map
                    entry.insert(sample);
                }
                // we already have an entry for this submission
                Occupied(entry) => entry.into_mut().add_row(row)?,
            }
        }
    }
    // build our vec of sample details to return
    let mut details = Vec::with_capacity(map.len());
    // crawl the submissions in order and add them to our map
    for (_, sha256s) in sorted.into_iter().rev() {
        // crawl over the submission in this timestamp group
        for sha256 in sha256s {
            // try to pop this submission
            if let Some(mut sample) = map.remove(&sha256) {
                // get the tags for this sample
                sample.get_tags(groups, shared).await?;
                // add this sample to our vec
                details.push(sample);
            }
        }
    }
    Ok(details)
}

/// Updates a submission object in scylla
///
/// # Arguments
///
/// * `sample` - The sample we are updating
/// * `update` - The update to apply to this submission object
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::files::update", skip_all, err(Debug))]
pub async fn update(
    sample: Sample,
    update: &SubmissionUpdate,
    shared: &Shared,
) -> Result<Sample, ApiError> {
    // log the sample we are updating
    event!(Level::INFO, sha256 = &sample.sha256);
    // get the submission to update if it exists
    if let Some(sub) = sample.submissions.iter().find(|sub| update.id == sub.id) {
        // serialize the new origin string
        let origin_str = sub.origin.serialize()?;
        // get the year and month this submission was originally added
        let year = sub.uploaded.year();
        // get the chunk size for files
        let chunk_size = shared.config.thorium.files.partition_size;
        // determine what bucket to add data too
        let bucket = helpers::partition(sub.uploaded, year, chunk_size);
        // update all of the submission rows for this submission
        for group in sub.groups.iter() {
            // build this group's submission row from borrowed values
            let row = SamplesListTableRef {
                group,
                year,
                bucket,
                uploaded: sub.uploaded,
                id: sub.id,
                sha256: &sample.sha256,
                sha1: &sample.sha1,
                md5: &sample.md5,
                name: sub.name.as_deref(),
                description: sub.description.as_deref(),
                submitter: &sub.submitter,
                origin: origin_str.as_deref(),
            };
            shared
                .scylla
                .queries
                .samples_list
                .insert(&shared.scylla.session, &row)
                .await?;
        }
        // if any groups are set to be removed then remove them in chunks of 100
        if !update.remove_groups.is_empty() {
            shared
                .scylla
                .queries
                .samples_list
                .delete_multiple_groups(
                    &shared.scylla.session,
                    &update.remove_groups,
                    year,
                    bucket,
                    sub.uploaded,
                    sub.id,
                )
                .await?;
        }
        Ok(sample)
    } else {
        not_found!(format!(
            "Submission {}:{} not found",
            sample.sha256, update.id
        ))
    }
}

/// Creates a comment for a specific sha256
///
/// # Arguments
///
/// * `user` - The user that is adding new comments
/// * `sha256` - The sha256 to comment on
/// * `form` - The comment to save to scylla
/// * `shared` - Shared objects in Thorium
#[instrument(
    name = "db::files::create_comment",
    skip(user, form, shared),
    err(Debug)
)]
pub async fn create_comment(
    user: &User,
    sha256: &str,
    form: &CommentForm,
    shared: &Shared,
) -> Result<(), ApiError> {
    // get the current timestamp
    let now = Utc::now();
    // create a comment row for each group, with its s3 paths serialized as JSON
    for group in form.groups.iter() {
        let row = CommentsTableRef {
            group,
            sha256,
            uploaded: now,
            id: form.id,
            author: &user.username,
            comment: &form.comment,
            files: JsonText(&form.attachments),
        };
        shared
            .scylla
            .queries
            .comments
            .insert(&shared.scylla.session, &row)
            .await?;
    }
    Ok(())
}

/// Prunes the attachments for the given list of comments if needed
///
/// Attachments are only pruned if the comment is no longer reachable (does not exist)
///
/// # Arguments
///
/// * `comments` - The comments to prune attachments for
/// * `sha256` - The SHA256 of the associated file
/// * `shared` - Shared Thorium objects
pub async fn prune_comment_attachments(
    comments: &[Comment],
    sha256: &str,
    shared: &Shared,
) -> Result<(), ApiError> {
    // downselect down to only those comments that have attachments
    let has_attachments = comments
        .iter()
        .filter(|comment| !comment.attachments.is_empty())
        .map(|comment| &comment.id)
        .collect::<Vec<&Uuid>>();
    // determine which comment attachments are still reachable (the comments exist)
    let reachable = comments_exist(&has_attachments, shared).await?;
    // downselect to comments that are unreachable
    let unreachable = comments
        .iter()
        .filter(|comment| !comment.attachments.is_empty())
        .filter(|comment| !reachable.contains(&comment.id));
    // crawl through and delete all of our comment attachments
    for comment in unreachable {
        // delete all of this comments files
        for s3_id in comment.attachments.values() {
            // build the path to save this attachment at in s3
            let s3_path = format!("{}/{}/{}", sha256, comment.id, s3_id);
            shared.s3.attachments.delete(&s3_path).await?;
        }
    }
    Ok(())
}

/// Determines which of the given comments still exist in Thorium
///
/// # Arguments
///
/// * `ids` - The comment ids to check for
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::files::comment_exists", skip(shared), err(Debug))]
async fn comments_exist(ids: &[&Uuid], shared: &Shared) -> Result<HashSet<Uuid>, ApiError> {
    // build our hashset of comments
    let mut found = HashSet::with_capacity(ids.len());
    // look the ids up in chunks of 100
    let ids = ids.iter().map(|id| **id).collect::<Vec<Uuid>>();
    let rows = shared
        .scylla
        .queries
        .comments_by_id
        .exists(&shared.scylla.session, &ids)
        .await?;
    // get the rows from this query if they were returned
    for cast in rows {
        // raise instead of just logging errors as ignoring them can cause
        // dangling references in the Db
        let row = cast?;
        // track that this comment id was still found
        found.insert(row.id);
    }
    Ok(found)
}

/// Deletes a comment for a specific sha256
///
/// This doesn't delete any of the comments attachments.
///
/// # Arguments
///
/// * `sha256` - The sha256 to delete the comment from
/// * `groups` - The groups to delete this comment for
/// * `comment` - The comment to delete
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::files::delete_comment", skip(comment, shared), err(Debug))]
pub async fn delete_comment(
    sha256: &str,
    groups: &[String],
    comment: &Comment,
    shared: &Shared,
) -> Result<(), ApiError> {
    // log the comment we are deleting
    event!(Level::INFO, comment = &comment.id.to_string());
    // delete this comment for each of our target groups
    for group in groups {
        // delete this comment row
        shared
            .scylla
            .queries
            .comments
            .delete(
                &shared.scylla.session,
                group,
                sha256,
                comment.uploaded,
                comment.id,
            )
            .await?;
    }
    Ok(())
}

/// Count a page of files and their tags
///
/// # Arguments
///
/// * `user` - The user that is counting files
/// * `params` - The params for counting tags
/// * `shared` - Shared Thorium objects
pub async fn count(
    user: &User,
    params: FileListParams,
    shared: &Shared,
) -> Result<ScyllaTagCountCursor<SampleListLine>, ApiError> {
    // Get an existing tag count cursor or create a new one scoped to the creating user
    let mut cursor =
        ScyllaTagCountCursor::<SampleListLine>::from_params(params, (), &user.username, shared)
            .await?;
    // get the next page of data
    cursor.next(user, shared).await?;
    // save our cursors state
    cursor.save(shared).await?;
    Ok(cursor)
}
