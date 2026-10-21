//! The file-system seam thoradm's writers open files through
//!
//! Archives, downloaded objects, and `redis.json` are written through a [`SinkFactory`] rather
//! than through `tokio::fs` directly, so tests can substitute a factory that injects disk faults
//! (a full disk, short writes, failed syncs or renames) and records every call. Production uses
//! [`TokioFs`], a thin wrapper over `tokio::fs`.

use std::path::Path;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncWriteExt, BufWriter};

/// The capacity of the write buffer in front of each file opened by [`TokioFs`]
const WRITE_BUFFER: usize = 1_048_576;

/// The mode every directory a backup creates is given, before the umask: a backup holds every
/// row, object, and credential of a cluster
pub const PRIVATE_DIR_MODE: u32 = 0o700;

/// The mode every file a backup writes is created with, before the umask
pub const PRIVATE_FILE_MODE: u32 = 0o600;

/// Creates a directory and any missing parents, each private to this user
///
/// # Arguments
///
/// * `dir` - The directory to create
pub async fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    // every directory it creates gets the mode; directories that exist are left alone
    tokio::fs::DirBuilder::new()
        .recursive(true)
        .mode(PRIVATE_DIR_MODE)
        .create(dir)
        .await
}

/// A file being written by a backup, restore, or verify run
#[async_trait::async_trait]
pub trait FileSink: Send + Sync {
    /// Writes every byte or returns the error
    ///
    /// # Arguments
    ///
    /// * `buf` - The bytes to write
    async fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()>;

    /// Hands every buffered byte to the operating system without forcing it to disk
    async fn flush(&mut self) -> std::io::Result<()>;

    /// Flushes buffered bytes and syncs the file's data and metadata to disk
    async fn sync(&mut self) -> std::io::Result<()>;

    /// Cuts the file back to a length, leaving any buffered bytes unwritten
    ///
    /// The sink must be dropped afterwards without another write, flush, or sync, since its
    /// buffer may still hold bytes past the new length.
    ///
    /// # Arguments
    ///
    /// * `len` - The length to cut the file back to
    async fn truncate(&mut self, len: u64) -> std::io::Result<()>;
}

/// Opens files and changes directories for thoradm's writers
#[async_trait::async_trait]
pub trait SinkFactory: Send + Sync {
    /// Creates (or truncates) a file with a mode
    ///
    /// # Arguments
    ///
    /// * `path` - The file to create
    /// * `mode` - The Unix permission bits to create it with, before the umask
    async fn create(&self, path: &Path, mode: u32) -> std::io::Result<Box<dyn FileSink>>;

    /// Opens a file for appending, creating it with mode 0600 when missing
    ///
    /// # Arguments
    ///
    /// * `path` - The file to append to
    async fn append(&self, path: &Path) -> std::io::Result<Box<dyn FileSink>>;

    /// Renames a file within one file system
    ///
    /// # Arguments
    ///
    /// * `from` - The current path
    /// * `to` - The new path
    async fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()>;

    /// Syncs a directory so renames and creations in it are durable
    ///
    /// # Arguments
    ///
    /// * `dir` - The directory to sync
    async fn sync_dir(&self, dir: &Path) -> std::io::Result<()>;

    /// Removes a file, succeeding when it is already gone
    ///
    /// # Arguments
    ///
    /// * `path` - The file to remove
    async fn remove(&self, path: &Path) -> std::io::Result<()>;
}

/// A file opened by [`TokioFs`]
pub struct TokioFile {
    /// The buffered handle to the open file
    file: BufWriter<File>,
}

#[async_trait::async_trait]
impl FileSink for TokioFile {
    /// Writes every byte into the buffer, spilling it to the file when it fills
    ///
    /// # Arguments
    ///
    /// * `buf` - The bytes to write
    async fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        // the buffered writer loops until every byte is accepted
        self.file.write_all(buf).await
    }

    /// Hands every buffered byte to the operating system without forcing it to disk
    async fn flush(&mut self) -> std::io::Result<()> {
        // flushing the buffered writer also waits for tokio's background write to finish
        self.file.flush().await
    }

    /// Flushes buffered bytes and syncs the file's data and metadata to disk
    async fn sync(&mut self) -> std::io::Result<()> {
        // hand every buffered byte to the file first
        self.file.flush().await?;
        // then force the file's data and metadata to disk
        self.file.get_ref().sync_all().await
    }

    /// Cuts the file back to a length, leaving any buffered bytes unwritten
    ///
    /// # Arguments
    ///
    /// * `len` - The length to cut the file back to
    async fn truncate(&mut self, len: u64) -> std::io::Result<()> {
        // cut the file under the buffer, whose bytes are dropped with the sink
        self.file.get_ref().set_len(len).await
    }
}

/// The production [`SinkFactory`], a thin wrapper over `tokio::fs`
#[derive(Debug, Clone, Copy, Default)]
pub struct TokioFs;

#[async_trait::async_trait]
impl SinkFactory for TokioFs {
    /// Creates (or truncates) a file with a mode
    ///
    /// # Arguments
    ///
    /// * `path` - The file to create
    /// * `mode` - The Unix permission bits to create it with, before the umask
    async fn create(&self, path: &Path, mode: u32) -> std::io::Result<Box<dyn FileSink>> {
        // open the file for writing, creating or truncating it with the requested mode
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(path)
            .await?;
        Ok(Box::new(TokioFile {
            file: BufWriter::with_capacity(WRITE_BUFFER, file),
        }))
    }

    /// Opens a file for appending, creating it with mode 0600 when missing
    ///
    /// # Arguments
    ///
    /// * `path` - The file to append to
    async fn append(&self, path: &Path) -> std::io::Result<Box<dyn FileSink>> {
        // open the file for appending, creating it private to this user when missing
        let file = OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(path)
            .await?;
        Ok(Box::new(TokioFile {
            file: BufWriter::with_capacity(WRITE_BUFFER, file),
        }))
    }

    /// Renames a file within one file system
    ///
    /// # Arguments
    ///
    /// * `from` - The current path
    /// * `to` - The new path
    async fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        // rename replaces the target atomically on the same file system
        tokio::fs::rename(from, to).await
    }

    /// Syncs a directory so renames and creations in it are durable
    ///
    /// # Arguments
    ///
    /// * `dir` - The directory to sync
    async fn sync_dir(&self, dir: &Path) -> std::io::Result<()> {
        // a directory is synced by opening it read-only and syncing the handle
        File::open(dir).await?.sync_all().await
    }

    /// Removes a file, succeeding when it is already gone
    ///
    /// # Arguments
    ///
    /// * `path` - The file to remove
    async fn remove(&self, path: &Path) -> std::io::Result<()> {
        // a file that is already gone is what the caller asked for
        match tokio::fs::remove_file(path).await {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            result => result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{SinkFactory, TokioFs};
    use std::os::unix::fs::PermissionsExt;

    /// Builds a fresh scratch directory under the system temp directory
    ///
    /// # Arguments
    ///
    /// * `name` - The test's name for the directory
    fn scratch(name: &str) -> std::path::PathBuf {
        // name the directory by test and process so parallel runs never share it
        let dir = std::env::temp_dir().join(format!("thoradm-sink-{name}-{}", std::process::id()));
        // start from an empty directory
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Created files take their mode, show their bytes once flushed, and are truncated when
    /// created again
    #[tokio::test]
    async fn create_writes_flushes_and_truncates() {
        // create a file, write to it, and flush it
        let dir = scratch("create");
        let path = dir.join("data");
        let mut file = TokioFs.create(&path, 0o600).await.unwrap();
        file.write_all(b"hello ").await.unwrap();
        file.write_all(b"world").await.unwrap();
        file.flush().await.unwrap();
        // the bytes are visible once flushed, and the mode was applied
        assert_eq!(std::fs::read(&path).unwrap(), b"hello world");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        // creating it again truncates it
        let mut file = TokioFs.create(&path, 0o600).await.unwrap();
        file.write_all(b"x").await.unwrap();
        file.sync().await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"x");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Appending keeps earlier lines and creates the file private, and renames, directory syncs,
    /// and removes of missing files succeed
    #[tokio::test]
    async fn append_rename_sync_dir_and_remove() {
        // append to a file twice through two handles
        let dir = scratch("append");
        let path = dir.join("log");
        for line in [b"a\n", b"b\n"] {
            let mut file = TokioFs.append(&path).await.unwrap();
            file.write_all(line).await.unwrap();
            file.sync().await.unwrap();
        }
        assert_eq!(std::fs::read(&path).unwrap(), b"a\nb\n");
        // an appended file is created private to this user
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        // rename it and sync the directory
        let renamed = dir.join("log.old");
        TokioFs.rename(&path, &renamed).await.unwrap();
        TokioFs.sync_dir(&dir).await.unwrap();
        assert!(!path.exists() && renamed.exists());
        // removing succeeds whether or not the file is still there
        TokioFs.remove(&renamed).await.unwrap();
        TokioFs.remove(&renamed).await.unwrap();
        assert!(!renamed.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
