//! The backup related features for Thoradm

mod archive;
pub mod controllers;
pub mod coverage;
pub mod failures;
pub(crate) mod graphics;
pub mod kept;
pub mod manifest;
pub(super) mod monitors;
pub(crate) mod new_backup;
mod restore;
pub(crate) mod s3;
mod scrub;
pub mod sink;
pub mod target;

pub mod tables;
pub mod utils;
pub mod verify;
pub mod verify_cluster;
pub mod version;
pub mod views;
pub mod wipe;
pub use archive::{ArchiveReader, MAP_ENTRY_LEN, PartitionArchive, check_partition};
pub(super) use monitors::{Monitor, MonitorUpdate};
pub use new_backup::{Backup, BackupWorker};
// thoradm's own tests drive the archive writer directly, without Scylla
#[cfg(feature = "test-support")]
pub use new_backup::{ArchiveWriter, TableTally};
pub(super) use restore::{Restore, RestoreWorker};
pub(super) use s3::{
    S3Backup, S3BackupWorker, S3Monitor, S3MonitorUpdate, S3Order, S3Restore, S3RestoreWorker,
};
pub use s3::{STAGING_DIR, staging_name};
// thoradm's own tests route virtual-hosted S3 requests through their proxy
#[cfg(feature = "test-support")]
pub use s3::route_virtual_hosts_through;
pub(super) use scrub::{Scrub, ScrubWorker};
pub use utils::Utils;
// rexport our controller handle function
pub use controllers::handle;
