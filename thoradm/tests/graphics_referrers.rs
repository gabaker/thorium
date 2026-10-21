//! The record types that can name a graphic are the ones the graphics backup derives its keys
//! from
//!
//! The graphics component reads the keys from the entities archive (`Entity.image`) and from the
//! users in `redis.json` (`User.image`). A type that gains graphics support has a key the backup
//! would miss, so this test fails until the backup handles it and the list below names it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// The types the graphics backup derives keys from: users through Redis and entities through
/// their archived rows
const COVERED: [&str; 2] = ["Entity", "User"];

/// Collects every Rust file under a directory
///
/// # Arguments
///
/// * `dir` - The directory to walk
/// * `files` - Where to add the files found
fn rust_files(dir: &Path, files: &mut Vec<PathBuf>) {
    // visit each entry, descending into directories
    for entry in std::fs::read_dir(dir).unwrap_or_else(|err| panic!("{}: {err}", dir.display())) {
        let path = entry.expect("a directory entry reads").path();
        if path.is_dir() {
            rust_files(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
}

/// Returns the type a line implements graphics support for, if it is such an impl
///
/// # Arguments
///
/// * `line` - A line of source
fn implementor(line: &str) -> Option<String> {
    // `impl GraphicSupport for Name` with any generics or braces after the name
    let rest = line.trim().strip_prefix("impl GraphicSupport for ")?;
    let name: String = rest
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    (!name.is_empty()).then_some(name)
}

/// The set of types implementing the API's graphics support equals the set the backup covers
#[test]
fn every_graphic_referrer_is_backed_up() {
    // scan the API's sources for implementations of the graphics support trait
    let api = Path::new(env!("CARGO_MANIFEST_DIR")).join("../api/src");
    let mut files = Vec::new();
    rust_files(&api, &mut files);
    let mut found = BTreeSet::new();
    for file in files {
        let source = std::fs::read_to_string(&file).expect("a source file reads");
        found.extend(source.lines().filter_map(implementor));
    }
    // a new implementor is a record type with a graphic the backup doesn't derive
    let covered: BTreeSet<String> = COVERED.iter().map(|name| (*name).to_owned()).collect();
    assert_eq!(
        found, covered,
        "the types implementing GraphicSupport changed: make the graphics backup derive the keys \
         of every new one (from its archived rows or the Redis state) and list it in COVERED"
    );
}
