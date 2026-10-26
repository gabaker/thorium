//! Save/Load associations into the DB

use chrono::prelude::*;
use futures::stream::{self, StreamExt};
use std::collections::BTreeMap;
use tracing::instrument;
use uuid::Uuid;

use super::{ScyllaCursor, keys};
use crate::models::schema::QueryError;
use crate::models::schema::scylla::AssociationsTableRef;
use crate::models::{
    AssociatedSubmissions, AssociationKind, AssociationListOpts, AssociationListParams,
    AssociationTarget, AssociationTargetColumn, Directionality, ListableAssociation, User,
};
use crate::serialize;
use crate::utils::{ApiError, Shared, helpers};

/// Write both directions of one association in one group, the forward row first
///
/// # Arguments
///
/// * `forward` - The source -> target row
/// * `reverse` - The target -> source row
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::associations::create_helper", skip_all, err(Debug))]
async fn create_helper(
    forward: AssociationsTableRef<'_>,
    reverse: AssociationsTableRef<'_>,
    shared: &Shared,
) -> Result<(), QueryError> {
    // This row is in the source -> target direction
    shared
        .scylla
        .queries
        .associations
        .insert(&shared.scylla.session, &forward)
        .await?;
    // This row is in the target  -> source direction
    shared
        .scylla
        .queries
        .associations
        .insert(&shared.scylla.session, &reverse)
        .await?;
    Ok(())
}

/// Create associations from a single source to many targets across groups
///
/// This writes both directions for every target/group pair and updates the
/// census cache to reflect the new associations.
///
/// # Arguments
///
/// * `user` - The user creating these associations
/// * `size_hint` - A hint for how many associations are being created
/// * `kind` - The kind of associations being created
/// * `source` - The source these associations originate from
/// * `targets` - The targets to associate with the source and their groups
/// * `submissions` - The submission ids tied to each side of the association, if any
/// * `direction` - The directionality of the `source -> target` rows
/// * `shared` - Shared Thorium objects
#[expect(clippy::too_many_arguments)]
pub async fn create(
    user: &User,
    size_hint: usize,
    kind: AssociationKind,
    source: AssociationTarget,
    targets: &Vec<(AssociationTarget, Vec<String>)>,
    submissions: Option<AssociatedSubmissions>,
    direction: Directionality,
    shared: &Shared,
) -> Result<(), ApiError> {
    // the associations are created now
    create_at(
        user,
        size_hint,
        kind,
        source,
        targets,
        submissions,
        direction,
        Utc::now(),
        shared,
    )
    .await
}

/// Create associations from a single source to many targets across groups at a chosen time
///
/// The rows are keyed by their creation time, so a writer that must be able to repeat a write
/// without duplicating rows passes the same time every time.
///
/// # Arguments
///
/// * `user` - The user creating these associations
/// * `size_hint` - A hint for how many associations are being created
/// * `kind` - The kind of associations being created
/// * `source` - The source these associations originate from
/// * `targets` - The targets to associate with the source and their groups
/// * `submissions` - The submission ids tied to each side of the association, if any
/// * `direction` - The directionality of the `source -> target` rows
/// * `now` - When the associations are created
/// * `shared` - Shared Thorium objects
#[instrument(
    name = "db::associations::create",
    skip(user, targets, shared),
    err(Debug)
)]
#[expect(clippy::too_many_arguments)]
pub async fn create_at(
    user: &User,
    size_hint: usize,
    kind: AssociationKind,
    source: AssociationTarget,
    targets: &Vec<(AssociationTarget, Vec<String>)>,
    submissions: Option<AssociatedSubmissions>,
    direction: Directionality,
    now: DateTime<Utc>,
    shared: &Shared,
) -> Result<(), ApiError> {
    // get the submission ids tied to each side of this association if any are set
    // the other submission is applied to every target so this is only meaningful for single target requests
    let submissions_source = submissions.as_ref().and_then(|subs| subs.source);
    let submissions_other = submissions.as_ref().and_then(|subs| subs.other);
    // get the year of the time we are inserting these rows at
    let year = now.year();
    // get the partition size to use for files and tags
    let chunk = shared.config.thorium.associations.partition_size;
    // calculate the bucket for our timestamp
    let bucket = helpers::partition(now, year, chunk);
    // convert our source
    let (converted_src, extra_src) = source.to_column()?;
    // serialize our source info
    let source_str = serialize!(&converted_src);
    // keep a set of keys of census info to update
    let mut keys = Vec::with_capacity(size_hint + 10);
    // serialize every target once, so the rows built below can borrow the results
    let mut serialized = Vec::with_capacity(targets.len());
    for (target, target_groups) in targets {
        // clone and cast our target to a target column
        let (converted_targ, extra_targ) = target.clone().to_column()?;
        // serialize our target info
        let target_str = serialize!(&converted_targ);
        // build the keys for our associations_to census cache
        keys::associations::census_keys(
            &mut keys,
            target_groups,
            year,
            bucket,
            &source_str,
            &target_str,
            shared,
        );
        serialized.push((target_str, extra_targ, target_groups));
    }
    // the reverse rows borrow the opposite direction, so it must outlive them too
    let opposite = direction.opposite();
    // borrow every shared value before building rows, so every row borrows data this
    // function owns
    let (source_str, extra_src, direction, kind, opposite, submitter) = (
        source_str.as_str(),
        extra_src.as_deref(),
        &direction,
        &kind,
        &opposite,
        user.username.as_str(),
    );
    // build both rows for every target and group, borrowing the serialized targets
    let mut futs = Vec::with_capacity(size_hint);
    for (target_str, extra_target, target_groups) in &serialized {
        for group in *target_groups {
            // the source -> target row
            let forward = AssociationsTableRef {
                group,
                year,
                bucket,
                source: source_str,
                created: now,
                target: target_str,
                direction,
                kind,
                submitter,
                extra_source: extra_src,
                extra_target: extra_target.as_deref(),
                submissions_source,
                submissions_other,
            };
            // the target -> source row: the same borrows with each pair swapped
            let reverse = AssociationsTableRef {
                source: target_str,
                target: source_str,
                direction: opposite,
                extra_source: extra_target.as_deref(),
                extra_target: extra_src,
                submissions_source: submissions_other,
                submissions_other: submissions_source,
                ..forward
            };
            // create a future to insert both rows
            futs.push(create_helper(forward, reverse, shared));
        }
    }
    // execute our to/from futures
    stream::iter(futs)
        .buffer_unordered(25)
        .collect::<Vec<Result<(), QueryError>>>()
        .await
        .into_iter()
        .collect::<Result<Vec<()>, QueryError>>()?;
    // update this samples census cache info
    super::census::incr_cache(keys, shared).await?;
    Ok(())
}

/// Copy one of a source's listed associations into more groups
///
/// The copied rows keep the association's kind, direction, extra info, and submissions, but are
/// created at the time of the group change and submitted by the user who made it, since that user
/// is the one who put the association in these groups.
///
/// # Arguments
///
/// * `submitter` - The user adding the groups, who submits the copied rows
/// * `now` - The time the groups were added, used as the copied rows' creation time
/// * `source` - The association target column the association was listed from
/// * `source_extra` - Any extra info tied to the source (an entity's name)
/// * `association` - The listed association to copy
/// * `groups` - The groups to copy the association into
/// * `shared` - Shared Thorium objects
#[instrument(
    name = "db::associations::copy_to_groups",
    skip(association, shared),
    err(Debug)
)]
pub async fn copy_to_groups(
    submitter: &str,
    now: DateTime<Utc>,
    source: &AssociationTargetColumn,
    source_extra: Option<String>,
    association: &ListableAssociation,
    groups: &Vec<String>,
    shared: &Shared,
) -> Result<(), ApiError> {
    // serialize our source
    let source_str = serialize!(source);
    // get the year and bucket of the group change the copied rows are created at
    let year = now.year();
    let chunk = shared.config.thorium.associations.partition_size;
    let bucket = helpers::partition(now, year, chunk);
    // the reverse rows borrow the opposite direction, so it must outlive them
    let opposite = association.direction.opposite();
    // build a future to write both rows of this association in each group
    let mut futs = Vec::with_capacity(groups.len());
    for group in groups {
        // the source -> other row
        let forward = AssociationsTableRef {
            group,
            year,
            bucket,
            source: &source_str,
            created: now,
            target: &association.other,
            direction: &association.direction,
            kind: &association.kind,
            submitter,
            extra_source: source_extra.as_deref(),
            extra_target: association.extra_other.as_deref(),
            submissions_source: association.submissions_source,
            submissions_other: association.submissions_other,
        };
        // the other -> source row: the same borrows with each pair swapped
        let reverse = AssociationsTableRef {
            source: &association.other,
            target: &source_str,
            direction: &opposite,
            extra_source: association.extra_other.as_deref(),
            extra_target: source_extra.as_deref(),
            submissions_source: association.submissions_other,
            submissions_other: association.submissions_source,
            ..forward
        };
        // add the future writing both rows to our list
        futs.push(create_helper(forward, reverse, shared));
    }
    // write the rows 25 at a time
    stream::iter(futs)
        .buffer_unordered(25)
        .collect::<Vec<Result<(), QueryError>>>()
        .await
        .into_iter()
        .collect::<Result<Vec<()>, QueryError>>()?;
    // add the copied rows to the census cache of each group
    let mut keys = Vec::with_capacity(groups.len() * 2);
    keys::associations::census_keys(
        &mut keys,
        groups,
        year,
        bucket,
        &source_str,
        &association.other,
        shared,
    );
    super::census::incr_cache(keys, shared).await?;
    Ok(())
}

/// Help delete both the regular and inverted rows for this association
///
/// # Arguments
///
/// * `group_chunk` - The chunk of groups to delete this association from
/// * `year` - The year this association was created in
/// * `bucket` - The partition bucket for this association's timestamp
/// * `source_serialized` - The serialized source of this association
/// * `association` - The association to delete
/// * `shared` - Shared Thorium objects
async fn delete_helper(
    group_chunk: &[String],
    year: i32,
    bucket: i32,
    source_serialized: &String,
    association: &ListableAssociation,
    shared: &Shared,
) -> Result<(), QueryError> {
    // build the future to delete this association
    shared
        .scylla
        .queries
        .associations
        .delete_chunk(
            &shared.scylla.session,
            group_chunk,
            year,
            bucket,
            source_serialized,
            association.created,
            &association.other,
            &association.direction,
        )
        .await?;
    // get our opposite direction
    let opposite_dir = association.direction.opposite();
    // build the future to delete this association
    shared
        .scylla
        .queries
        .associations
        .delete_chunk(
            &shared.scylla.session,
            group_chunk,
            year,
            bucket,
            &association.other,
            association.created,
            source_serialized,
            &opposite_dir,
        )
        .await?;
    Ok(())
}

/// Delete a list of associations from multiple groups
///
/// # Arguments
///
/// * `source` - The association target column these associations originate from
/// * `associations` - The associations to delete
/// * `shared` - Shared Thorium objects
pub async fn delete_many(
    source: &AssociationTargetColumn,
    associations: &Vec<ListableAssociation>,
    shared: &Shared,
) -> Result<(), ApiError> {
    // serialize our source
    let source_serialized = serialize!(&source);
    // instance a list of futures for our deletes
    let mut futs = Vec::with_capacity(associations.len());
    // create the futures for each association to delete
    for assoc in associations {
        // get the year and bucket for this association
        let year = assoc.created.year();
        // get the partition size to use for files and tags
        let chunk = shared.config.thorium.associations.partition_size;
        // calculate the bucket for our timestamp
        let bucket = helpers::partition(assoc.created, year, chunk);
        // chunk this associations groups into chunks of 99
        for group_chunk in assoc.groups.chunks(99) {
            // build the future to delete this association
            let fut = delete_helper(group_chunk, year, bucket, &source_serialized, assoc, shared);
            // add this future to our future set
            futs.push(fut);
        }
    }
    // execute all of our deletes 10 at a time
    stream::iter(futs)
        .buffer_unordered(10)
        .collect::<Vec<Result<(), QueryError>>>()
        .await
        .into_iter()
        .collect::<Result<Vec<()>, QueryError>>()?;
    Ok(())
}

/// List associations for a specific entity/file/repo
///
/// # Arguments
///
/// * `user` - The user that is listing associations
/// * `opts` - The query params to use when listing files
/// * `source` - The association target column to list associations for
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::associations::list", skip(user, shared), err(Debug))]
pub async fn list<P: Into<AssociationListParams> + std::fmt::Debug>(
    user: &User,
    opts: P,
    source: &AssociationTargetColumn,
    shared: &Shared,
) -> Result<ScyllaCursor<ListableAssociation>, ApiError> {
    // convert our params
    let params = opts.into();
    // serialize our association target column before list things related to it
    let source_str = serialize!(&source);
    // get our cursor scoped to the creating user
    let mut cursor =
        ScyllaCursor::from_params_extra(params, source_str, &user.username, false, shared).await?;
    // get the next page of data for this cursor
    cursor.next(shared).await?;
    // save this cursor
    cursor.save(shared).await?;
    Ok(cursor)
}

/// An edge whose source references an entity, found in one group
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupReferrer {
    /// The kind of the referencing edge
    pub kind: AssociationKind,
    /// The referencing data at the edge's source
    pub source: AssociationTargetColumn,
    /// The referenced entity at the edge's target
    pub target: Uuid,
}

/// List the edges in some groups that reference any of the given entities
///
/// An edge references an entity when the entity is its target; bidirectional edges reference both
/// ends. Every referrer in the given groups is returned, keyed by group, including referrers the
/// caller can't see, so a caller listing them must name only the visible ones.
///
/// # Arguments
///
/// * `user` - The user checking for referrers
/// * `targets` - The entities to find referrers for (e.g. a root and its instances)
/// * `groups` - The groups to look for referrers in
/// * `shared` - Shared Thorium objects
#[instrument(
    name = "db::associations::referrers_in_groups",
    skip(user, shared),
    err(Debug)
)]
pub async fn referrers_in_groups(
    user: &User,
    targets: &[Uuid],
    groups: &[String],
    shared: &Shared,
) -> Result<BTreeMap<String, Vec<GroupReferrer>>, ApiError> {
    // keep the referrers found in each group
    let mut referrers: BTreeMap<String, Vec<GroupReferrer>> = BTreeMap::new();
    // there are no referrers without groups to look in
    if groups.is_empty() {
        return Ok(referrers);
    }
    // list the associations of each target
    for target in targets {
        // the target's own rows hold every edge it's part of
        let column = AssociationTargetColumn::Entity(*target);
        // list this target's associations in the requested groups
        let opts = AssociationListOpts::default()
            .groups(groups.to_vec())
            .limit(500);
        let mut cursor = list(user, opts, &column, shared).await?;
        // step over every page of associations
        loop {
            // keep the rows whose other end is the edge's source
            for assoc in &cursor.data {
                // rows pointing at this target come from the edge's source
                if !matches!(
                    assoc.direction,
                    Directionality::From | Directionality::Bidirectional
                ) {
                    continue;
                }
                // get the referencing source
                let source = assoc.deserialize_other()?;
                // add this referrer to each requested group it's in
                for group in assoc.groups.iter().filter(|group| groups.contains(group)) {
                    referrers
                        .entry(group.clone())
                        .or_default()
                        .push(GroupReferrer {
                            kind: assoc.kind,
                            source: source.clone(),
                            target: *target,
                        });
                }
            }
            // stop once this cursor has been exhausted
            if cursor.exhausted() {
                break;
            }
            // clear our current page and get the next one
            cursor.data.clear();
            cursor.next(shared).await?;
        }
    }
    Ok(referrers)
}
