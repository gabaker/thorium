//! The archives of backed up data

use bytecheck::CheckBytes;
use bytes::BytesMut;
use data_encoding::HEXLOWER;
use rkyv::{Archive, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, BufReader};

use crate::Error;
use crate::backup::utils::is_link;

/// A single archive of a partition
#[derive(Debug, Archive, Serialize, Deserialize)]
#[archive_attr(derive(Debug, CheckBytes))]
pub struct PartitionArchive {
    /// What byte this partitions archive starts at
    pub start: u64,
    /// What byte this partitions archive stops at
    pub end: u64,
    /// The hash for this partition
    pub partition: u64,
    /// The sha256 for this partition
    pub sha256: String,
}

impl PartitionArchive {
    /// Create a new partition archive
    ///
    /// # Argumetns
    ///
    /// * `start` - What byte this partition archive starts at
    /// * `end` - What byte this partitions archive stops as
    /// * `partition` - The hash for this partition
    /// * `sha256` - The sha256 for this partition
    pub fn new<T: Into<String>>(start: usize, end: usize, partition: u64, sha256: T) -> Self {
        PartitionArchive {
            start: start as u64,
            end: end as u64,
            partition,
            sha256: sha256.into(),
        }
    }
}

/// Checks a partition's bytes against the sha256 its map entry records
///
/// Scrub, verify, and restore all check partitions through this one function.
///
/// # Arguments
///
/// * `bytes` - The partition's bytes as read from the data file
/// * `entry` - The partition's map entry
pub fn check_partition(bytes: &[u8], entry: &PartitionArchive) -> Result<(), String> {
    // hash the bytes and compare them with what the backup recorded when it wrote them
    let sha256 = HEXLOWER.encode(&Sha256::digest(bytes));
    if sha256 == entry.sha256 {
        Ok(())
    } else {
        Err(format!(
            "partition {} (bytes {} to {}) fails its checksum: expected {}, found {sha256}",
            entry.partition, entry.start, entry.end, entry.sha256
        ))
    }
}

/// The size of one archived map entry
pub const MAP_ENTRY_LEN: u64 = 96;

/// The archive to read data from
///
/// A map is read as corrupt, rather than as a clean end, when it stops part way through an
/// entry, and when an entry doesn't start where the previous one ended. A data file shorter than
/// a map entry says is corrupt too.
pub struct ArchiveReader {
    /// The map file being read, for errors
    map_path: PathBuf,
    /// The data file being read, for errors
    data_path: PathBuf,
    /// The data file's length when it was opened
    data_len: u64,
    /// The map's length when it was opened
    map_len: u64,
    /// How many bytes of the map were read
    map_offset: u64,
    /// Where the next partition must start in the data file: the previous entry's end
    data_offset: u64,
    /// The map to read our archive map from
    map_reader: BufReader<File>,
    /// The file to read data from
    data_reader: BufReader<File>,
    /// The current buffer to map info into
    pub map_buffer: BytesMut,
    /// The current buffer to read data into
    pub data_buffer: BytesMut,
}

impl ArchiveReader {
    /// Create a new archive reader
    ///
    /// # Arguments
    ///
    /// * `path` - The path to the map file to read in
    pub async fn new(path: PathBuf) -> Result<Self, Error> {
        // get the id for our archive
        let Some(id) = path.file_stem().map(ToOwned::to_owned) else {
            return Err(Error::new(format!(
                "Failed to get archive id from {}",
                path.display()
            )));
        };
        // the data file shares the map's id in the data directory beside the map directory
        let mut data_path = path.clone();
        data_path.pop();
        data_path.pop();
        data_path.push("data");
        data_path.push(id);
        // a linked map or data file isn't followed, since its bytes would come from outside the
        // backup that verify checked
        for file in [&path, &data_path] {
            if is_link(file).await? {
                return Err(Error::new(format!(
                    "{} is a symbolic link; a backup holds only real files, so it isn't followed",
                    file.display()
                )));
            }
        }
        // open our map file and note its length
        let map_file = File::open(&path).await?;
        let map_len = map_file.metadata().await?.len();
        let map_reader = BufReader::new(map_file);
        // open our archive file and note its length
        let data_file = File::open(&data_path).await?;
        let data_len = data_file.metadata().await?.len();
        let data_reader = BufReader::new(data_file);
        // create a bytesmut obejct for our map info
        let map_buffer = BytesMut::zeroed(96);
        // create a bytesmut object at least 1MiB big
        let data_buffer = BytesMut::zeroed(1_048_576);
        // create our archive reader
        let reader = ArchiveReader {
            map_path: path,
            data_path,
            data_len,
            map_len,
            map_offset: 0,
            data_offset: 0,
            map_reader,
            data_reader,
            map_buffer,
            data_buffer,
        };
        Ok(reader)
    }

    /// Builds the error for a map that is corrupt at an offset
    ///
    /// # Arguments
    ///
    /// * `problem` - What is wrong at the current offset
    fn corrupt(&self, problem: String) -> Error {
        // name the map and where in it the problem is
        Error::Corruption {
            path: self.map_path.clone(),
            offset: self.map_offset,
            problem,
        }
    }

    /// Reads the next map entry, or `None` at a clean end of the map
    async fn next_entry(&mut self) -> Result<Option<PartitionArchive>, Error> {
        // the map ends cleanly only at an entry boundary
        let left = self.map_len.saturating_sub(self.map_offset);
        if left == 0 {
            return Ok(None);
        }
        if left < MAP_ENTRY_LEN {
            return Err(self.corrupt(format!(
                "the map ends {left} bytes into an entry of {MAP_ENTRY_LEN} bytes"
            )));
        }
        // read the whole entry; the map was measured when it was opened, so a short read means
        // it changed underneath us
        self.map_reader.read_exact(&mut self.map_buffer[..]).await?;
        // deserialize this archived map entry
        let archived_entry = rkyv::check_archived_root::<PartitionArchive>(&self.map_buffer)?;
        let entry: PartitionArchive = archived_entry.deserialize(&mut rkyv::Infallible)?;
        // each partition starts where the previous one ended
        if entry.start != self.data_offset || entry.end < entry.start {
            return Err(self.corrupt(format!(
                "the entry covers bytes {} to {} of the data file, but the previous entry ended at {}",
                entry.start, entry.end, self.data_offset
            )));
        }
        self.map_offset += MAP_ENTRY_LEN;
        self.data_offset = entry.end;
        Ok(Some(entry))
    }

    /// Reads one partition's bytes into the data buffer and returns its length
    ///
    /// # Arguments
    ///
    /// * `entry` - The partition's map entry
    async fn read_partition(&mut self, entry: &PartitionArchive) -> Result<usize, Error> {
        // a partition past the end of the data file is corruption of the data file at its start
        let past_data = |problem: String| Error::Corruption {
            path: self.data_path.clone(),
            offset: entry.start,
            problem,
        };
        if entry.end > self.data_len {
            return Err(past_data(format!(
                "the map says the partition ends at byte {}, but the data file holds {} bytes",
                entry.end, self.data_len
            )));
        }
        // calculate the size of this partitions data
        let partition_len = usize::try_from(entry.end - entry.start)
            .map_err(|_| Error::new("a partition is larger than memory"))?;
        // extend our buffer to fit this partitions data if required
        // this currently never frees space but it likely should
        if self.data_buffer.len() < partition_len {
            // resize our buffer to fit
            self.data_buffer.resize(partition_len, 0x0);
        }
        // read this partitions data in; a data file that shrank since it was opened ends early
        match self
            .data_reader
            .read_exact(&mut self.data_buffer[..partition_len])
            .await
        {
            Ok(_) => Ok(partition_len),
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                Err(past_data(format!(
                    "the data file ends before the partition's end at byte {}",
                    entry.end
                )))
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Read in the bytes for the next partition, or `None` at the map's clean end
    ///
    /// A map or data file that is damaged returns a corruption error instead.
    pub async fn next(&mut self) -> Result<Option<&[u8]>, Error> {
        // read the next entry, stopping at a clean end of the map
        let Some(entry) = self.next_entry().await? else {
            return Ok(None);
        };
        // read the partition it describes
        let partition_len = self.read_partition(&entry).await?;
        Ok(Some(&self.data_buffer[..partition_len]))
    }

    /// Read in the bytes for the next partition and return them with the map entry that describes
    /// them, or `None` at the map's clean end
    ///
    /// A map or data file that is damaged returns a corruption error instead.
    pub async fn next_partition(&mut self) -> Result<Option<(&[u8], PartitionArchive)>, Error> {
        // read the next entry, stopping at a clean end of the map
        let Some(entry) = self.next_entry().await? else {
            return Ok(None);
        };
        // read the partition it describes
        let partition_len = self.read_partition(&entry).await?;
        Ok(Some((&self.data_buffer[..partition_len], entry)))
    }
}

#[cfg(test)]
mod tests {
    use super::{ArchiveReader, PartitionArchive};
    use std::path::{Path, PathBuf};

    /// Builds a fresh archive directory with `data/` and `maps/` under the system temp directory
    ///
    /// # Arguments
    ///
    /// * `name` - The test's name for the directory
    fn scratch(name: &str) -> PathBuf {
        // name the directory by test and process so parallel runs never share it
        let dir =
            std::env::temp_dir().join(format!("thoradm-archive-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("data")).unwrap();
        std::fs::create_dir_all(dir.join("maps")).unwrap();
        dir
    }

    /// Writes an archive whose map holds the given entries, with data long enough for all of them
    ///
    /// # Arguments
    ///
    /// * `dir` - The archive directory
    /// * `entries` - Each entry's start and end
    fn write_archive(dir: &Path, entries: &[(usize, usize)]) -> PathBuf {
        // one 96-byte map entry per partition, in order
        let mut map = Vec::new();
        for (start, end) in entries {
            let entry = PartitionArchive::new(*start, *end, 7, "a".repeat(64));
            map.extend_from_slice(&rkyv::to_bytes::<_, 1024>(&entry).unwrap());
        }
        // a data file as long as the furthest entry's end
        let data_len = entries.iter().map(|(_, end)| *end).max().unwrap_or(0);
        std::fs::write(dir.join("data").join("archive"), vec![1_u8; data_len]).unwrap();
        // the map beside it in maps/
        let map_path = dir.join("maps").join("archive.thoriummap");
        std::fs::write(&map_path, map).unwrap();
        map_path
    }

    /// A map cut off in the middle of an entry gives its whole entries, then a corruption error
    /// naming the map and the offset, from both readers
    #[tokio::test]
    async fn a_map_cut_mid_entry_is_corruption_after_its_whole_entries() {
        let dir = scratch("cut");
        let map = write_archive(&dir, &[(0, 10), (10, 25), (25, 40)]);
        // trim the map by 40 bytes, leaving two whole entries and part of the third
        let len = std::fs::metadata(&map).unwrap().len();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&map)
            .unwrap()
            .set_len(len - 40)
            .unwrap();
        // next reads two partitions, then fails
        let mut reader = ArchiveReader::new(map.clone()).await.unwrap();
        assert!(reader.next().await.unwrap().is_some());
        assert!(reader.next().await.unwrap().is_some());
        let error = reader
            .next()
            .await
            .expect_err("a cut map read as a clean end");
        let text = error.to_string();
        assert!(
            text.contains("archive.thoriummap") && text.contains("192"),
            "{text}"
        );
        // next_partition does the same
        let mut reader = ArchiveReader::new(map.clone()).await.unwrap();
        assert!(reader.next_partition().await.unwrap().is_some());
        assert!(reader.next_partition().await.unwrap().is_some());
        let error = reader
            .next_partition()
            .await
            .expect_err("a cut map read as a clean end");
        let text = error.to_string();
        assert!(
            text.contains("archive.thoriummap") && text.contains("192"),
            "{text}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// An entry that doesn't start where the previous one ended is corruption, from both readers
    #[tokio::test]
    async fn an_entry_that_skips_bytes_is_corruption() {
        let dir = scratch("gap");
        let map = write_archive(&dir, &[(0, 10), (12, 25)]);
        // the first partition reads and the second is refused
        let mut reader = ArchiveReader::new(map.clone()).await.unwrap();
        assert!(reader.next().await.unwrap().is_some());
        let text = reader.next().await.expect_err("a gap was read").to_string();
        assert!(
            text.contains("archive.thoriummap") && text.contains("96"),
            "{text}"
        );
        let mut reader = ArchiveReader::new(map.clone()).await.unwrap();
        assert!(reader.next_partition().await.unwrap().is_some());
        let text = reader
            .next_partition()
            .await
            .expect_err("a gap was read")
            .to_string();
        assert!(
            text.contains("archive.thoriummap") && text.contains("96"),
            "{text}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A map entry that ends past the data file's end is corruption naming the data file and the
    /// partition's offset, from both readers
    #[tokio::test]
    async fn an_entry_past_the_data_is_corruption_naming_the_data_file() {
        let dir = scratch("past-data");
        let map = write_archive(&dir, &[(0, 10), (10, 25)]);
        // cut the data file to 15 bytes, inside the second partition
        let data = dir.join("data").join("archive");
        std::fs::write(&data, vec![1_u8; 15]).unwrap();
        // the first partition reads and the second is refused, naming the data file at byte 10
        let mut reader = ArchiveReader::new(map.clone()).await.unwrap();
        assert!(reader.next().await.unwrap().is_some());
        let error = reader
            .next()
            .await
            .expect_err("a map past the data was read");
        assert!(
            matches!(&error, crate::Error::Corruption { path, offset: 10, .. } if *path == data),
            "{error}"
        );
        let mut reader = ArchiveReader::new(map.clone()).await.unwrap();
        assert!(reader.next_partition().await.unwrap().is_some());
        let error = reader
            .next_partition()
            .await
            .expect_err("a map past the data was read");
        assert!(
            matches!(&error, crate::Error::Corruption { path, offset: 10, .. } if *path == data),
            "{error}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A whole map reads every partition and then ends cleanly
    #[tokio::test]
    async fn a_whole_map_ends_cleanly() {
        // an archive of two partitions
        let dir = scratch("whole");
        let map = write_archive(&dir, &[(0, 10), (10, 25)]);
        // both partitions read at their lengths
        let mut reader = ArchiveReader::new(map).await.unwrap();
        assert_eq!(reader.next().await.unwrap().map(<[u8]>::len), Some(10));
        assert_eq!(reader.next().await.unwrap().map(<[u8]>::len), Some(15));
        // and then the map ends without an error
        assert!(reader.next().await.unwrap().is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
