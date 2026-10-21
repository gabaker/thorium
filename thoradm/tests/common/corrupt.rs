//! Corruption helpers for copies of fixture backups
//!
//! Each helper damages one file the way a disk, a truncated copy, or a careless edit would, so
//! tests can check that verify and restore notice and name the damaged object.

use std::path::Path;

/// Flips every bit of the byte at an offset
///
/// # Arguments
///
/// * `path` - The file to damage
/// * `offset` - The byte to flip
pub fn flip_byte(path: &Path, offset: u64) -> std::io::Result<()> {
    // read the file, flip the byte, and write it back
    let mut bytes = std::fs::read(path)?;
    let index = usize::try_from(offset).map_err(std::io::Error::other)?;
    let byte = bytes.get_mut(index).ok_or_else(|| {
        std::io::Error::other(format!("{} has no byte at offset {offset}", path.display()))
    })?;
    *byte = !*byte;
    std::fs::write(path, bytes)
}

/// Cuts bytes off the end of a file
///
/// # Arguments
///
/// * `path` - The file to truncate
/// * `by` - How many bytes to remove from its end
pub fn truncate_by(path: &Path, by: u64) -> std::io::Result<()> {
    // shorten the file in place
    let len = std::fs::metadata(path)?.len();
    let file = std::fs::OpenOptions::new().write(true).open(path)?;
    file.set_len(len.saturating_sub(by))
}

/// Deletes a file
///
/// # Arguments
///
/// * `path` - The file to delete
pub fn delete(path: &Path) -> std::io::Result<()> {
    // remove it outright
    std::fs::remove_file(path)
}

/// Adds a file that the backup never wrote
///
/// # Arguments
///
/// * `path` - Where to add it
/// * `contents` - Its bytes
pub fn add_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    // create its directory and write it
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents)
}

/// Replaces one value in a JSON file, addressed by a JSON pointer
///
/// # Arguments
///
/// * `path` - The JSON file to edit
/// * `pointer` - The JSON pointer to the value (`/format_version`)
/// * `value` - The value to put there
pub fn edit_json(path: &Path, pointer: &str, value: serde_json::Value) -> std::io::Result<()> {
    // parse the document
    let text = std::fs::read_to_string(path)?;
    let mut doc: serde_json::Value = serde_json::from_str(&text).map_err(std::io::Error::other)?;
    // replace the addressed value
    let target = doc.pointer_mut(pointer).ok_or_else(|| {
        std::io::Error::other(format!("{} has no value at {pointer}", path.display()))
    })?;
    *target = value;
    // write it back
    std::fs::write(
        path,
        serde_json::to_vec(&doc).map_err(std::io::Error::other)?,
    )
}
