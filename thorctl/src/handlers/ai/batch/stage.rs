//! Sample staging: the dataset hash index, zip extraction, and blind aliases

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufReader, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thorium::Error;

use super::manifest::DatasetConf;

/// Where one sample lives inside the dataset zips
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexEntry {
    /// The zip holding this sample
    pub zip: PathBuf,
    /// The member name inside that zip
    pub inner: String,
    /// The sample's uncompressed size in bytes
    pub size: u64,
}

/// The change-detection stamp for one dataset zip
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ZipStamp {
    /// The zip's size in bytes when it was indexed
    pub size: u64,
    /// The zip's mtime in whole seconds since the epoch when it was indexed
    pub mtime_secs: i64,
}

/// A persistent sha256 index over every member of the dataset zips
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatasetIndex {
    /// The index schema version
    pub version: u32,
    /// The stamps of every indexed zip keyed by path
    pub zips: BTreeMap<PathBuf, ZipStamp>,
    /// The indexed samples keyed by lowercase sha256 hex
    pub samples: BTreeMap<String, IndexEntry>,
}

impl Default for DatasetIndex {
    /// Build an empty index at the current schema version
    fn default() -> Self {
        DatasetIndex {
            version: 1,
            zips: BTreeMap::new(),
            samples: BTreeMap::new(),
        }
    }
}

impl DatasetIndex {
    /// Load the dataset index from an evaluation directory
    ///
    /// A missing index file is just an empty index.
    ///
    /// # Arguments
    ///
    /// * `dir` - The evaluation directory holding dataset-index.json
    pub fn load(dir: &Path) -> Result<DatasetIndex, Error> {
        // read the index file if one exists
        let path = dir.join("dataset-index.json");
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            // a missing index just means nothing has been indexed yet
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(DatasetIndex::default()),
            Err(err) => {
                return Err(Error::new(format!(
                    "failed to read {}: {err}",
                    path.display()
                )));
            }
        };
        // a corrupt index is derived data, so deleting it is a safe fix
        let index = serde_json::from_str(&raw).map_err(|err| {
            Error::new(format!(
                "corrupt dataset index {}: {err} (delete it to rebuild)",
                path.display()
            ))
        })?;
        Ok(index)
    }

    /// Persist this index into an evaluation directory atomically
    ///
    /// # Arguments
    ///
    /// * `dir` - The evaluation directory to write dataset-index.json into
    pub fn save(&self, dir: &Path) -> Result<(), Error> {
        // make sure the evaluation directory exists
        std::fs::create_dir_all(dir)
            .map_err(|err| Error::new(format!("failed to create {}: {err}", dir.display())))?;
        // serialize the index up front so a failure writes nothing
        let raw = serde_json::to_string_pretty(self)
            .map_err(|err| Error::new(format!("failed to serialize dataset index: {err}")))?;
        // land the bytes atomically so readers never see a torn index
        super::state::write_atomic(&dir.join("dataset-index.json"), raw.as_bytes())
    }

    /// Re-index every new or changed zip under the dataset root
    ///
    /// Hashes every member of every `*.zip` whose size/mtime stamp changed,
    /// prunes samples whose zip vanished, and returns how many zips were
    /// (re)indexed.
    ///
    /// # Arguments
    ///
    /// * `dataset` - The dataset root and password to index
    pub fn refresh(&mut self, dataset: &DatasetConf) -> Result<usize, Error> {
        // find every zip under the dataset root
        let zips = find_zips(&dataset.root)?;
        let mut reindexed = 0;
        // re-index any zip that is new or whose stamp changed
        for path in &zips {
            // stat the zip to build its change-detection stamp
            let stamp = zip_stamp(path)?;
            // skip zips already indexed at this exact size and mtime
            if self.zips.get(path) == Some(&stamp) {
                continue;
            }
            // drop this zip's stale entries before re-reading it
            self.samples.retain(|_, entry| entry.zip != *path);
            // hash every member of the zip into the index
            let members = self.index_zip(path, dataset)?;
            // report progress since hashing large zips takes a while
            eprintln!("indexed {}: {members} members", path.display());
            // remember the stamp so unchanged zips are skipped next time
            self.zips.insert(path.clone(), stamp);
            reindexed += 1;
        }
        // prune samples and stamps whose zip no longer exists on disk
        self.samples.retain(|_, entry| entry.zip.exists());
        self.zips.retain(|zip, _| zip.exists());
        Ok(reindexed)
    }

    /// Look a sample up by its sha256
    ///
    /// # Arguments
    ///
    /// * `sha` - The sha256 hex to look up, any casing
    pub fn lookup(&self, sha: &str) -> Option<&IndexEntry> {
        // the index keys are lowercase hex
        self.samples.get(&sha.to_ascii_lowercase())
    }

    /// Hash every file member of one zip into the index
    ///
    /// # Arguments
    ///
    /// * `path` - The zip to index
    /// * `dataset` - The dataset settings holding the password
    fn index_zip(&mut self, path: &Path, dataset: &DatasetConf) -> Result<usize, Error> {
        // open the archive through a buffered reader
        let file = File::open(path)
            .map_err(|err| Error::new(format!("failed to open {}: {err}", path.display())))?;
        let mut archive = zip::ZipArchive::new(BufReader::new(file))
            .map_err(|err| Error::new(format!("failed to read zip {}: {err}", path.display())))?;
        let mut members = 0;
        // hash every file member in the archive
        for index in 0..archive.len() {
            // grab the member's name and kind without decrypting it yet
            let (name, is_dir) = {
                let raw = archive.by_index_raw(index).map_err(|err| {
                    Error::new(format!(
                        "failed to read member {index} of {}: {err}",
                        path.display()
                    ))
                })?;
                (raw.name().to_owned(), raw.is_dir())
            };
            // directories carry no sample bytes
            if is_dir {
                continue;
            }
            // open the member with the dataset password
            let mut member = archive
                .by_index_decrypt(index, dataset.password.as_bytes())
                .map_err(|err| decrypt_error(path, &name, &err.to_string()))?;
            // stream the member's bytes through a sha256 hasher
            let mut hasher = Sha256::new();
            let size = io::copy(&mut member, &mut hasher)
                .map_err(|err| decrypt_error(path, &name, &err.to_string()))?;
            // hex encode the digest in the index's lowercase convention
            let sha = data_encoding::HEXLOWER.encode(&hasher.finalize());
            // the first zip to provide a sha wins; later duplicates are ignored
            self.samples.entry(sha).or_insert_with(|| IndexEntry {
                zip: path.to_path_buf(),
                inner: name,
                size,
            });
            members += 1;
        }
        Ok(members)
    }
}

/// Build the error for a member that failed to decrypt or read
///
/// # Arguments
///
/// * `zip` - The zip the member lives in
/// * `member` - The member's name inside the zip
/// * `err` - The underlying error text
fn decrypt_error(zip: &Path, member: &str, err: &str) -> Error {
    Error::new(format!(
        "failed to read member '{member}' of {}: {err} (is the dataset password wrong?)",
        zip.display()
    ))
}

/// Find every zip under a dataset root, case-insensitively
///
/// macOS AppleDouble sidecars (`._*`) are skipped: they are resource-fork
/// metadata that shares the real zip's extension but is not an archive, so
/// indexing one would abort the whole refresh.
///
/// # Arguments
///
/// * `root` - The dataset root to walk
fn find_zips(root: &Path) -> Result<Vec<PathBuf>, Error> {
    let mut zips = Vec::new();
    // walk the dataset root recursively
    for entry in walkdir::WalkDir::new(root) {
        let entry =
            entry.map_err(|err| Error::new(format!("failed to walk {}: {err}", root.display())))?;
        // only plain files can be zips
        if !entry.file_type().is_file() {
            continue;
        }
        // match the zip extension case-insensitively
        let is_zip = entry
            .path()
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("zip"));
        // AppleDouble sidecars are metadata wearing the zip extension
        let is_sidecar = entry.file_name().to_string_lossy().starts_with("._");
        if is_zip && !is_sidecar {
            zips.push(entry.into_path());
        }
    }
    // keep a stable order so first-wins duplicate handling is deterministic
    zips.sort();
    Ok(zips)
}

/// Stat a zip into its change-detection stamp
///
/// # Arguments
///
/// * `path` - The zip to stat
fn zip_stamp(path: &Path) -> Result<ZipStamp, Error> {
    // stat the zip for its size and mtime
    let meta = std::fs::metadata(path)
        .map_err(|err| Error::new(format!("failed to stat {}: {err}", path.display())))?;
    // fold the mtime down to whole seconds since the epoch
    let mtime_secs = match meta.modified() {
        Ok(time) => match time.duration_since(std::time::UNIX_EPOCH) {
            Ok(span) => i64::try_from(span.as_secs()).unwrap_or(i64::MAX),
            // a pre-epoch mtime lands negative
            Err(err) => -i64::try_from(err.duration().as_secs()).unwrap_or(i64::MAX),
        },
        // some filesystems cannot report mtimes; stamp those as time zero
        Err(_) => 0,
    };
    Ok(ZipStamp {
        size: meta.len(),
        mtime_secs,
    })
}

/// Stage one dataset sample onto disk by its sha256
///
/// Extracts the indexed member into `<dir>/samples/<sha>`, verifying that the
/// extracted bytes hash to the requested sha before the file lands. The
/// output name is the sha we chose ourselves (never the zip's member name),
/// so zip-slip is structurally impossible; nothing is ever executed.
///
/// # Arguments
///
/// * `dir` - The evaluation directory samples stage under
/// * `dataset` - The dataset settings holding the password
/// * `index` - The dataset index mapping shas to zip members
/// * `sha` - The sha256 of the sample to stage
pub fn stage_sample(
    dir: &Path,
    dataset: &DatasetConf,
    index: &DatasetIndex,
    sha: &str,
) -> Result<PathBuf, Error> {
    // normalize so lookups and file names share one casing
    let sha = sha.to_ascii_lowercase();
    let samples_dir = dir.join("samples");
    let target = samples_dir.join(&sha);
    // an already staged sample is reused as is
    if target.exists() {
        return Ok(target);
    }
    // find which zip member holds this sha
    let Some(entry) = index.lookup(&sha) else {
        return Err(Error::new(format!(
            "sha256 {sha} is not in the dataset index; refresh the index or check \
             that the sample's zip is under {}",
            dataset.root.display()
        )));
    };
    // make sure the staging directory exists
    std::fs::create_dir_all(&samples_dir)
        .map_err(|err| Error::new(format!("failed to create {}: {err}", samples_dir.display())))?;
    // open the zip holding the sample
    let file = File::open(&entry.zip)
        .map_err(|err| Error::new(format!("failed to open {}: {err}", entry.zip.display())))?;
    let mut archive = zip::ZipArchive::new(BufReader::new(file))
        .map_err(|err| Error::new(format!("failed to read zip {}: {err}", entry.zip.display())))?;
    // open the member by its stored name with the dataset password
    let mut member = archive
        .by_name_decrypt(&entry.inner, dataset.password.as_bytes())
        .map_err(|err| decrypt_error(&entry.zip, &entry.inner, &err.to_string()))?;
    // extract into a same-dir tmp file, hashing as the bytes stream
    let tmp = samples_dir.join(format!(".{sha}.tmp.{}", std::process::id()));
    if let Err(err) = extract_verified(&mut member, &tmp, &sha, &entry.zip, &entry.inner) {
        // never leave unverified bytes behind
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    // samples are inputs only: stage them read-only
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o444))
            .map_err(|err| Error::new(format!("failed to chmod {}: {err}", tmp.display())))?;
    }
    // move the verified bytes into place atomically
    std::fs::rename(&tmp, &target).map_err(|err| {
        Error::new(format!(
            "failed to rename {} into place: {err}",
            tmp.display()
        ))
    })?;
    Ok(target)
}

/// Extract a zip member into a file, verifying its sha256 on the way
///
/// # Arguments
///
/// * `member` - The opened zip member to extract
/// * `tmp` - The tmp file to extract into
/// * `sha` - The sha256 the extracted bytes must hash to
/// * `zip` - The zip the member lives in, for error context
/// * `inner` - The member's name inside the zip, for error context
fn extract_verified(
    member: &mut impl Read,
    tmp: &Path,
    sha: &str,
    zip: &Path,
    inner: &str,
) -> Result<(), Error> {
    // open the tmp file for writing
    let mut out = File::create(tmp)
        .map_err(|err| Error::new(format!("failed to create {}: {err}", tmp.display())))?;
    // hash while writing so verification needs no second read
    let mut hasher = Sha256::new();
    let mut tee = TeeWriter {
        out: &mut out,
        hasher: &mut hasher,
    };
    io::copy(member, &mut tee).map_err(|err| decrypt_error(zip, inner, &err.to_string()))?;
    // verify the extracted bytes are exactly the requested sample
    let got = data_encoding::HEXLOWER.encode(&hasher.finalize());
    if got != sha {
        return Err(Error::new(format!(
            "member '{inner}' of {} hashed to {got}, not the requested {sha}; \
             the dataset index is stale, refresh it",
            zip.display()
        )));
    }
    Ok(())
}

/// Writes bytes into a file while hashing them
struct TeeWriter<'a> {
    /// The destination file
    out: &'a mut File,
    /// The running sha256 over everything written
    hasher: &'a mut Sha256,
}

impl Write for TeeWriter<'_> {
    /// Write bytes to the file and hash exactly what landed
    ///
    /// # Arguments
    ///
    /// * `buf` - The bytes to write
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // write to the file first so a short write hashes correctly
        let wrote = self.out.write(buf)?;
        // hash exactly the bytes that landed
        self.hasher.update(&buf[..wrote]);
        Ok(wrote)
    }

    /// Flush the underlying file
    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

/// Shuffle items with OS entropy for blind alias assignment
///
/// # Arguments
///
/// * `items` - The items to shuffle
pub fn shuffled<T>(mut items: Vec<T>) -> Vec<T> {
    use rand::seq::SliceRandom;
    // the OS-seeded rng keeps blind alias order unpredictable
    items.shuffle(&mut rand::rng());
    items
}

/// Build the blind alias for a unit's position
///
/// # Arguments
///
/// * `index` - The unit's 0-based position in the shuffled order
pub fn alias_for(index: usize) -> String {
    // aliases are 1-based with at least two digits: s01, s02, ... s100
    format!("s{:02}", index + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sha256 of the fixture's a.bin member
    const SHA_A: &str = "c963adf97969ecfc8f01f98660d86d58d8cb7847fb39ae5475779722aba10604";
    /// The sha256 of the fixture's b.bin member
    const SHA_B: &str = "51e6508fc112d8d99137ee0a558a685066b622ad6a52a1087e221a0e659c286b";
    /// The exact bytes of the fixture's a.bin member
    const BYTES_A: &[u8] = b"thorctl batch staging fixture member a\n";

    /// Build a unique scratch directory for one test
    ///
    /// # Arguments
    ///
    /// * `name` - A short name distinguishing the test
    fn scratch_dir(name: &str) -> PathBuf {
        // unique per process and call so parallel tests never collide
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let count = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "thorctl-batch-stage-{name}-{}-{count}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The committed ZipCrypto fixture holding the two known members
    fn fixture_zip() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/handlers/ai/batch/testdata/staging.zip")
    }

    /// Build a scratch dataset holding a copy of the fixture zip
    ///
    /// # Arguments
    ///
    /// * `name` - A short name distinguishing the test
    fn fixture_dataset(name: &str) -> (PathBuf, DatasetConf) {
        // the dataset root holds one copy of the committed fixture
        let dir = scratch_dir(name);
        let root = dir.join("dataset");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::copy(fixture_zip(), root.join("staging.zip")).unwrap();
        let dataset = DatasetConf {
            root,
            password: "infected".to_owned(),
        };
        (dir, dataset)
    }

    /// AppleDouble sidecars in the dataset are skipped, not indexed
    #[test]
    fn appledouble_sidecars_are_skipped() {
        let (_dir, dataset) = fixture_dataset("sidecars");
        // drop a non-archive AppleDouble sidecar next to the real zip
        std::fs::write(dataset.root.join("._staging.zip"), b"not a zip").unwrap();
        let mut index = DatasetIndex::default();
        // the refresh indexes only the real zip instead of failing
        assert_eq!(index.refresh(&dataset).unwrap(), 1);
        // the real zip's members still land in the index
        assert!(index.lookup(SHA_A).is_some());
        // the sidecar was never recorded as a dataset zip
        assert!(!index.zips.contains_key(&dataset.root.join("._staging.zip")));
    }

    /// An index round-trips through save and load
    #[test]
    fn index_round_trip() {
        let dir = scratch_dir("roundtrip");
        // loading before any save is just the empty default
        assert_eq!(DatasetIndex::load(&dir).unwrap(), DatasetIndex::default());
        // build a small index by hand
        let mut index = DatasetIndex::default();
        index.zips.insert(
            PathBuf::from("/data/a.zip"),
            ZipStamp {
                size: 9,
                mtime_secs: 42,
            },
        );
        index.samples.insert(
            SHA_A.to_owned(),
            IndexEntry {
                zip: PathBuf::from("/data/a.zip"),
                inner: "a.bin".to_owned(),
                size: 39,
            },
        );
        // save then load preserves every field
        index.save(&dir).unwrap();
        assert_eq!(DatasetIndex::load(&dir).unwrap(), index);
    }

    /// Refresh indexes the fixture's members and lookups hit
    #[test]
    fn refresh_indexes_the_fixture() {
        let (_dir, dataset) = fixture_dataset("refresh");
        let mut index = DatasetIndex::default();
        // the first refresh indexes the one zip
        assert_eq!(index.refresh(&dataset).unwrap(), 1);
        // both members landed with their names and sizes
        let entry = index.lookup(SHA_A).unwrap();
        assert_eq!(entry.inner, "a.bin");
        assert_eq!(entry.size, BYTES_A.len() as u64);
        assert_eq!(entry.zip, dataset.root.join("staging.zip"));
        let entry = index.lookup(SHA_B).unwrap();
        assert_eq!(entry.inner, "b.bin");
        // lookups normalize casing
        assert!(index.lookup(&SHA_A.to_ascii_uppercase()).is_some());
        // an unchanged zip is not re-indexed
        assert_eq!(index.refresh(&dataset).unwrap(), 0);
        // a vanished zip's samples are pruned on the next refresh
        std::fs::remove_file(dataset.root.join("staging.zip")).unwrap();
        assert_eq!(index.refresh(&dataset).unwrap(), 0);
        assert!(index.lookup(SHA_A).is_none());
        assert!(index.zips.is_empty());
    }

    /// Staging extracts, verifies, write-protects, and is idempotent
    #[test]
    fn stage_sample_extracts_and_verifies() {
        let (dir, dataset) = fixture_dataset("stage");
        let mut index = DatasetIndex::default();
        index.refresh(&dataset).unwrap();
        // the sample stages under its sha with the exact fixture bytes
        let staged = stage_sample(&dir, &dataset, &index, SHA_A).unwrap();
        assert_eq!(staged, dir.join("samples").join(SHA_A));
        assert_eq!(std::fs::read(&staged).unwrap(), BYTES_A);
        // staged samples are read-only inputs
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&staged).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o444, "mode was: {mode:o}");
        }
        // staging again just returns the existing file
        assert_eq!(stage_sample(&dir, &dataset, &index, SHA_A).unwrap(), staged);
        // no tmp litter survives a successful staging
        let litter: Vec<_> = std::fs::read_dir(dir.join("samples"))
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains("tmp"))
            .collect();
        assert!(litter.is_empty(), "litter was: {litter:?}");
    }

    /// A sha missing from the index errors telling the operator so
    #[test]
    fn stage_sample_unknown_sha_errors() {
        let (dir, dataset) = fixture_dataset("unknownsha");
        let mut index = DatasetIndex::default();
        index.refresh(&dataset).unwrap();
        // an unknown sha names the problem instead of guessing
        let missing = "0".repeat(64);
        let err = stage_sample(&dir, &dataset, &index, &missing).unwrap_err();
        assert!(
            err.to_string().contains("not in the dataset index"),
            "err was: {err}"
        );
    }

    /// Extracted bytes that hash to the wrong sha are rejected
    #[test]
    fn stage_sample_verifies_the_hash() {
        let (dir, dataset) = fixture_dataset("verify");
        // forge an index claiming a.bin hashes to b.bin's sha
        let mut lying = DatasetIndex::default();
        lying.samples.insert(
            SHA_B.to_owned(),
            IndexEntry {
                zip: dataset.root.join("staging.zip"),
                inner: "a.bin".to_owned(),
                size: BYTES_A.len() as u64,
            },
        );
        // verification catches the mismatch and stages nothing
        let err = stage_sample(&dir, &dataset, &lying, SHA_B).unwrap_err();
        assert!(err.to_string().contains("hashed to"), "err was: {err}");
        assert!(!dir.join("samples").join(SHA_B).exists());
    }

    /// A wrong dataset password errors naming the zip
    #[test]
    fn wrong_password_names_the_zip() {
        let (_dir, dataset) = fixture_dataset("wrongpw");
        // the same dataset with a wrong password cannot be indexed
        let wrong = DatasetConf {
            root: dataset.root,
            password: "wrong".to_owned(),
        };
        let mut index = DatasetIndex::default();
        let err = index.refresh(&wrong).unwrap_err();
        assert!(err.to_string().contains("staging.zip"), "err was: {err}");
        assert!(err.to_string().contains("password"), "err was: {err}");
    }

    /// Aliases are 1-based and zero padded to at least two digits
    #[test]
    fn alias_formatting() {
        assert_eq!(alias_for(0), "s01");
        assert_eq!(alias_for(8), "s09");
        assert_eq!(alias_for(99), "s100");
    }

    /// Shuffling preserves the multiset of items
    #[test]
    fn shuffled_preserves_the_multiset() {
        // shuffle a list with duplicates
        let items: Vec<u32> = (0..50).chain(0..5).collect();
        let mut out = shuffled(items.clone());
        // sorting both sides proves nothing was lost or invented
        let mut expected = items;
        expected.sort_unstable();
        out.sort_unstable();
        assert_eq!(out, expected);
    }
}
