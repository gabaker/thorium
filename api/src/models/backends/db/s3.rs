use tracing::instrument;
/// Maps s3 data to sha256s using scylla
use uuid::Uuid;

use crate::models::S3Objects;
use crate::models::schema::scylla::S3IdsTableRef;
use crate::utils::{ApiError, Shared};
use crate::{not_found, unavailable};

/// Check if an s3 object id already exists in scylla
///
/// # Arguments
///
/// * `s3_id` - The s3 id to check for
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::s3::s3_id_exists", skip(s3_id, shared), err(Debug))]
pub async fn s3_id_exists(
    objects: S3Objects,
    s3_id: &Uuid,
    shared: &Shared,
) -> Result<bool, ApiError> {
    // check whether scylla has at least one row for this id
    let exists = shared
        .scylla
        .queries
        .s3_ids
        .id_exists(&shared.scylla.session, &objects, *s3_id)
        .await?;
    Ok(exists)
}

/// Checks if we have an existing s3 object for a target sha256/path already
///
/// # Arguments
///
/// * `object` - The type of objects to perform an existence check
/// * `path` - The path to check against
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::s3::object_exists", skip(shared), err(Debug))]
pub async fn object_exists(
    object: S3Objects,
    path: &str,
    shared: &Shared,
) -> Result<bool, ApiError> {
    // check whether scylla has at least one row for this path
    let exists = shared
        .scylla
        .queries
        .s3_sha256s
        .object_exists(&shared.scylla.session, path, &object)
        .await?;
    Ok(exists)
}

/// Insert a new s3 object id
///
/// # Arguments
///
/// * `s3_id` - The s3 id to insert
/// * `sha256` - The sha256 of the file this s3 id contains
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::s3::insert_s3_id", skip(s3_id, shared), err(Debug))]
pub async fn insert_s3_id(
    objects: S3Objects,
    s3_id: &Uuid,
    sha256: &str,
    shared: &Shared,
) -> Result<(), ApiError> {
    // build the row for this id
    let row = S3IdsTableRef {
        object_type: &objects,
        id: *s3_id,
        sha256,
    };
    shared
        .scylla
        .queries
        .s3_ids
        .insert(&shared.scylla.session, &row)
        .await?;
    Ok(())
}

/// Generate a new uuid that is not yet in use
///
/// # Arguments
///
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::s3::generate_id", skip(shared), err(Debug))]
pub async fn generate_id(objects: S3Objects, shared: &Shared) -> Result<Uuid, ApiError> {
    // try 10 times to generate an unused uuid before failing
    for _ in 0..10 {
        // generate a random uuid
        let id = Uuid::new_v4();
        if !s3_id_exists(objects, &id, shared).await? {
            return Ok(id);
        }
    }
    unavailable!("Unable to generate a unique sample id".to_owned())
}

/// Gets an s3 object id for a specific sha256
///
/// # Arguments
///
/// * `Objects` - The type of object to get
/// * `key` - The key of the s3 id to get
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::s3::get_s3_id", skip(shared), err(Debug))]
pub async fn get_s3_id(objects: S3Objects, key: &str, shared: &Shared) -> Result<Uuid, ApiError> {
    // get the first row for this key
    let row = shared
        .scylla
        .queries
        .s3_sha256s
        .get(&shared.scylla.session, key, &objects)
        .await?;
    if let Some(row) = row {
        Ok(row.id)
    } else {
        // we didn't get any rows so this id does not exist
        not_found!(format!(
            "Failed to find an s3 object for {}:{}",
            objects, key
        ))
    }
}

/// Delete an object from the s3 id table
///
/// # Arguments
///
/// * `Objects` - The type of object to delete
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::s3::delete", skip(id, shared), err(Debug))]
pub async fn delete(objects: S3Objects, id: Uuid, shared: &Shared) -> Result<(), ApiError> {
    // delete every row of this id
    shared
        .scylla
        .queries
        .s3_ids
        .delete(&shared.scylla.session, &objects, id)
        .await?;
    Ok(())
}
