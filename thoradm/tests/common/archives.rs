//! Reads the rows a backup's archives hold, so tests can compare them with what was seeded

use bytecheck::CheckBytes;
use rkyv::Archive;
use rkyv::validation::validators::DefaultValidator;
use std::path::{Path, PathBuf};
use thoradm::backup::ArchiveReader;
use thoradm::backup::tables::{Versioned, read_rows};

/// Lists the map files of one table's backup directory
///
/// # Arguments
///
/// * `table_dir` - The table's directory in a backup (`<backup>/<table>`)
#[must_use]
pub fn maps(table_dir: &Path) -> Vec<PathBuf> {
    // a table that wrote nothing has no map directory
    let Ok(listing) = std::fs::read_dir(table_dir.join("maps")) else {
        return Vec::new();
    };
    // keep only the map files, in a stable order
    let mut maps: Vec<PathBuf> = listing
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "thoriummap"))
        .collect();
    maps.sort();
    maps
}

/// Reads every row of one table's archives and maps each to a key
///
/// # Arguments
///
/// * `table_dir` - The table's directory in a backup (`<backup>/<table>`)
/// * `key` - Maps one archived row to what the test compares
///
/// # Panics
///
/// Panics if an archive can't be read or a partition doesn't validate.
pub async fn rows<T, K, F>(table_dir: &Path, key: F) -> Vec<K>
where
    T: Archive,
    T::Archived: for<'a> CheckBytes<DefaultValidator<'a>>,
    F: Fn(&T::Archived) -> K,
{
    // walk every archive's partitions in map order
    let mut found = Vec::new();
    for map in maps(table_dir) {
        let mut reader = ArchiveReader::new(map.clone())
            .await
            .unwrap_or_else(|err| panic!("failed to open {}: {err}", map.display()));
        while let Some(partition) = reader
            .next()
            .await
            .unwrap_or_else(|err| panic!("failed to read {}: {err}", map.display()))
        {
            // each partition is an archived list of rows
            let rows = rkyv::check_archived_root::<Vec<T>>(partition)
                .unwrap_or_else(|err| panic!("a partition of {} is invalid: {err}", map.display()));
            found.extend(rows.iter().map(&key));
        }
    }
    found
}

/// Reads every row of one table's archives, written in a format version, as the current archive
/// struct
///
/// # Arguments
///
/// * `table_dir` - The table's directory in a backup (`<backup>/<table>`)
/// * `format` - The format version the backup was written in; a legacy backup's is 1
///
/// # Panics
///
/// Panics if an archive can't be read or a partition doesn't decode.
pub async fn versioned_rows<T: Versioned>(table_dir: &Path, format: u32) -> Vec<T> {
    // walk every archive's partitions in map order, converting older formats
    let mut found = Vec::new();
    for map in maps(table_dir) {
        let mut reader = ArchiveReader::new(map.clone())
            .await
            .unwrap_or_else(|err| panic!("failed to open {}: {err}", map.display()));
        while let Some(partition) = reader
            .next()
            .await
            .unwrap_or_else(|err| panic!("failed to read {}: {err}", map.display()))
        {
            found.extend(read_rows::<T>(partition, format).unwrap_or_else(|err| {
                panic!("a partition of {} is invalid: {err}", map.display())
            }));
        }
    }
    found
}
