//! The different tables we can backup and restore

mod associations;
mod comments;
mod commits;
mod entities;
mod network_policies;
mod nodes;
mod notifications;
mod repos;
mod results;
mod s3_ids;
mod samples_list;
mod tags;

use bytecheck::{CheckBytes, StructCheckError};
use rkyv::validation::validators::DefaultValidator;
use rkyv::{Archive, Deserialize, Infallible};

pub use associations::{Association, AssociationV1};
pub use comments::{Comment, CommentV1};
pub use commits::{Commitish, CommitishList, CommitishListV1, CommitishV1};
pub use entities::{Entity, EntityV1};
pub use network_policies::{NetworkPolicy, NetworkPolicyV1};
pub use nodes::{Node, NodeV1};
pub use notifications::{Notification, NotificationV1};
pub use repos::{RepoData, RepoList, RepoListV1};
pub use results::{Output, OutputStream, OutputStreamV1, OutputV1, entity_kind};
pub use s3_ids::S3Id;
pub use samples_list::{SamplesList, SamplesListV1};
pub use tags::Tag;

use scylla::value::MaybeUnset;

use crate::Error;
use crate::backup::manifest::FORMAT_VERSION;

/// Binds a column format 2 added to a table's archives: a row archived in an older format never
/// held the column, so its restore leaves the column unset and a `--merge` keeps what the target
/// holds there, while a format-2 row writes its value, or a null for a cell it recorded as unset
///
/// # Arguments
///
/// * `value` - The column's value in the row
/// * `format` - The format version the row was archived in
pub fn added_in_format_2<T>(value: T, format: u32) -> MaybeUnset<T> {
    // only a format-2 row holds the column
    if format < 2 {
        MaybeUnset::Unset
    } else {
        MaybeUnset::Set(value)
    }
}

/// A table's archive struct in the current format, with the struct format 1 archived its rows as
///
/// A table whose rows format 2 didn't change names itself as its format-1 struct.
pub trait Versioned:
    Archive<
        Archived: for<'a> CheckBytes<DefaultValidator<'a>, Error = StructCheckError>
                      + Deserialize<Self, Infallible>,
    > + Sized
{
    /// The struct format 1 archives the table's rows as; it converts with every field it holds
    type V1: Archive<
            Archived: for<'a> CheckBytes<DefaultValidator<'a>, Error = StructCheckError>
                          + Deserialize<Self::V1, Infallible>,
        > + Into<Self>;
}

/// Decodes one partition of a table's archive, written in a format version, as the current
/// archive struct
///
/// # Arguments
///
/// * `bytes` - The partition's bytes
/// * `format` - The format version the backup was written in; a legacy backup's is 1
pub fn read_rows<T: Versioned>(bytes: &[u8], format: u32) -> Result<Vec<T>, Error> {
    // every partition is an archived list of the table's rows in its format's struct
    match format {
        1 => {
            let archived = rkyv::check_archived_root::<Vec<T::V1>>(bytes)?;
            let rows: Vec<T::V1> = archived.deserialize(&mut Infallible)?;
            Ok(rows.into_iter().map(Into::into).collect())
        }
        FORMAT_VERSION => {
            let archived = rkyv::check_archived_root::<Vec<T>>(bytes)?;
            Ok(archived.deserialize(&mut Infallible)?)
        }
        other => Err(Error::new(format!(
            "archive format version {other} isn't one this thoradm reads (1 to {FORMAT_VERSION})"
        ))),
    }
}

/// Counts the rows of one partition of a table's archive, written in a format version
///
/// # Arguments
///
/// * `bytes` - The partition's bytes
/// * `format` - The format version the backup was written in; a legacy backup's is 1
pub fn count_rows<T: Versioned>(bytes: &[u8], format: u32) -> Result<u64, Error> {
    // validate the partition as its format's archived rows without copying them out
    let rows = match format {
        1 => rkyv::check_archived_root::<Vec<T::V1>>(bytes)?.len(),
        FORMAT_VERSION => rkyv::check_archived_root::<Vec<T>>(bytes)?.len(),
        other => {
            return Err(Error::new(format!(
                "archive format version {other} isn't one this thoradm reads (1 to \
                 {FORMAT_VERSION})"
            )));
        }
    };
    Ok(rows as u64)
}
