//! A `SinkFactory` over the real file system that injects disk faults and records every call
//!
//! It plugs into the file-system seam thoradm's writers use, so tests get `ENOSPC`, short
//! writes, and failed syncs or renames without root, and can check the order of writes, syncs,
//! and renames a writer made.

use super::mock_s3::glob_matches;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use thoradm::backup::sink::{FileSink, SinkFactory, TokioFs};

/// The errno for a full disk
const ENOSPC: i32 = 28;
/// The errno for an I/O error
const EIO: i32 = 5;
/// The errno for too many open files
const EMFILE: i32 = 24;

/// A fault for files matching a path glob
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkFault {
    /// Fail writes with `ENOSPC` once this many bytes have been written to the file
    NoSpaceAfter(u64),
    /// Write only this many bytes of the next write and report success, as a short write would
    ShortWrite(u64),
    /// Fail `sync` with `EIO`
    FailSync,
    /// Fail `flush` with `ENOSPC`, as a full disk does for bytes still in the write buffer
    FailFlush,
    /// Fail `rename` with `EIO`
    FailRename,
    /// Fail `create` and `append` with `EMFILE`
    TooManyFiles,
}

/// One recorded call
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SinkCall {
    /// A file was created, with its mode
    Create(PathBuf, u32),
    /// A file was opened for appending
    Append(PathBuf),
    /// Bytes were written
    Write(PathBuf, u64),
    /// A file's buffered bytes were handed to the operating system
    Flush(PathBuf),
    /// A file was synced
    Sync(PathBuf),
    /// A file was renamed
    Rename(PathBuf, PathBuf),
    /// A directory was synced
    SyncDir(PathBuf),
    /// A file was removed
    Remove(PathBuf),
}

/// The faults to inject, by path glob, shared by the factory and its open files
type Faults = Arc<Mutex<Vec<(String, SinkFault)>>>;

/// The recorded calls, shared by the factory and its open files
type Calls = Arc<Mutex<Vec<SinkCall>>>;

/// A `SinkFactory` over the real file system that injects faults and records every call
#[derive(Debug, Clone, Default)]
pub struct FaultySink {
    /// The faults to inject, by path glob
    faults: Faults,
    /// Every call made through the factory or its files, in order
    pub calls: Calls,
}

/// Returns the first fault of a kind set for a path
///
/// # Arguments
///
/// * `faults` - The configured faults
/// * `path` - The path being operated on
/// * `pick` - Selects the fault kinds that apply to the operation
fn fault_for(faults: &Faults, path: &Path, pick: impl Fn(&SinkFault) -> bool) -> Option<SinkFault> {
    // match each fault's glob against the path as text
    let text = path.to_string_lossy();
    faults
        .lock()
        .unwrap()
        .iter()
        .find(|(glob, fault)| glob_matches(glob, &text) && pick(fault))
        .map(|(_, fault)| *fault)
}

/// Builds an I/O error from an errno
///
/// # Arguments
///
/// * `errno` - The errno to report
fn os_error(errno: i32) -> std::io::Error {
    // carry the raw errno so callers see the same error the kernel would give
    std::io::Error::from_raw_os_error(errno)
}

impl FaultySink {
    /// Builds a factory with no faults
    #[must_use]
    pub fn new() -> Self {
        // start with no faults and no calls
        Self::default()
    }

    /// Adds a fault for every path matching a glob
    ///
    /// # Arguments
    ///
    /// * `glob` - The pattern over the full path; `*` matches any run of characters
    /// * `fault` - The fault to inject
    pub fn add_fault<T: Into<String>>(&self, glob: T, fault: SinkFault) {
        // faults apply to files opened from now on and to open files on their next call
        self.faults.lock().unwrap().push((glob.into(), fault));
    }

    /// Removes every fault
    pub fn clear_faults(&self) {
        // drop the configured faults
        self.faults.lock().unwrap().clear();
    }

    /// Returns every call recorded so far
    #[must_use]
    pub fn calls(&self) -> Vec<SinkCall> {
        // copy the record out from under the lock
        self.calls.lock().unwrap().clone()
    }

    /// Records one call
    ///
    /// # Arguments
    ///
    /// * `call` - The call to record
    fn record(&self, call: SinkCall) {
        // append under the lock so the order is the order calls happened
        self.calls.lock().unwrap().push(call);
    }

    /// Wraps a real file so its writes and syncs pass through the faults
    ///
    /// # Arguments
    ///
    /// * `path` - The file's path
    /// * `inner` - The real file
    fn wrap(&self, path: &Path, inner: Box<dyn FileSink>) -> Box<dyn FileSink> {
        // share the faults and calls with the file
        Box::new(FaultyFile {
            path: path.to_path_buf(),
            inner,
            written: 0,
            short_used: false,
            faults: self.faults.clone(),
            calls: self.calls.clone(),
        })
    }
}

/// A file opened through a `FaultySink`
struct FaultyFile {
    /// The file's path
    path: PathBuf,
    /// The real file
    inner: Box<dyn FileSink>,
    /// How many bytes were written to it
    written: u64,
    /// Whether its one short write already happened
    short_used: bool,
    /// The faults to inject, shared with the factory
    faults: Faults,
    /// The recorded calls, shared with the factory
    calls: Calls,
}

impl FaultyFile {
    /// Records one call
    ///
    /// # Arguments
    ///
    /// * `call` - The call to record
    fn record(&self, call: SinkCall) {
        // append under the lock so the order is the order calls happened
        self.calls.lock().unwrap().push(call);
    }
}

#[async_trait::async_trait]
impl FileSink for FaultyFile {
    /// Writes bytes unless a fault cuts the write short or fails it
    ///
    /// # Arguments
    ///
    /// * `buf` - The bytes to write
    async fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        // find a write fault for this file
        let fault = fault_for(&self.faults, &self.path, |fault| {
            matches!(fault, SinkFault::NoSpaceAfter(_) | SinkFault::ShortWrite(_))
        });
        // work out how much of this write lands and whether it fails
        let len = buf.len() as u64;
        let (keep, fail) = match fault {
            Some(SinkFault::NoSpaceAfter(limit)) if self.written + len > limit => {
                (limit.saturating_sub(self.written), true)
            }
            Some(SinkFault::ShortWrite(limit)) if !self.short_used => {
                self.short_used = true;
                (limit.min(len), false)
            }
            _ => (len, false),
        };
        // write the part that lands
        let keep_bytes = usize::try_from(keep).unwrap_or(buf.len());
        if keep_bytes > 0 {
            self.inner.write_all(&buf[..keep_bytes]).await?;
            self.written += keep;
            self.record(SinkCall::Write(self.path.clone(), keep));
        }
        // a full disk fails the write after the bytes that fit
        if fail {
            return Err(os_error(ENOSPC));
        }
        Ok(())
    }

    /// Hands buffered bytes to the operating system
    async fn flush(&mut self) -> std::io::Result<()> {
        // a full disk fails the flush of bytes still in the write buffer
        if fault_for(&self.faults, &self.path, |fault| {
            *fault == SinkFault::FailFlush
        })
        .is_some()
        {
            return Err(os_error(ENOSPC));
        }
        // record the flush and pass it on
        self.record(SinkCall::Flush(self.path.clone()));
        self.inner.flush().await
    }

    /// Syncs the file unless a fault fails the sync
    async fn sync(&mut self) -> std::io::Result<()> {
        // a failed sync reports an I/O error without syncing
        if fault_for(&self.faults, &self.path, |fault| {
            *fault == SinkFault::FailSync
        })
        .is_some()
        {
            return Err(os_error(EIO));
        }
        // record the sync and pass it on
        self.record(SinkCall::Sync(self.path.clone()));
        self.inner.sync().await
    }

    /// Cuts the file back to a length, which is never faulted
    ///
    /// # Arguments
    ///
    /// * `len` - The length to cut the file back to
    async fn truncate(&mut self, len: u64) -> std::io::Result<()> {
        // the file now holds that many bytes, so a full disk is measured from there
        self.written = self.written.min(len);
        self.inner.truncate(len).await
    }
}

#[async_trait::async_trait]
impl SinkFactory for FaultySink {
    /// Creates a file unless a fault reports too many open files
    ///
    /// # Arguments
    ///
    /// * `path` - The file to create
    /// * `mode` - The Unix permission bits to create it with
    async fn create(&self, path: &Path, mode: u32) -> std::io::Result<Box<dyn FileSink>> {
        // too many open files fails the open
        if fault_for(&self.faults, path, |fault| {
            *fault == SinkFault::TooManyFiles
        })
        .is_some()
        {
            return Err(os_error(EMFILE));
        }
        // create the real file and wrap it
        let inner = TokioFs.create(path, mode).await?;
        self.record(SinkCall::Create(path.to_path_buf(), mode));
        Ok(self.wrap(path, inner))
    }

    /// Opens a file for appending unless a fault reports too many open files
    ///
    /// # Arguments
    ///
    /// * `path` - The file to append to
    async fn append(&self, path: &Path) -> std::io::Result<Box<dyn FileSink>> {
        // too many open files fails the open
        if fault_for(&self.faults, path, |fault| {
            *fault == SinkFault::TooManyFiles
        })
        .is_some()
        {
            return Err(os_error(EMFILE));
        }
        // open the real file and wrap it
        let inner = TokioFs.append(path).await?;
        self.record(SinkCall::Append(path.to_path_buf()));
        Ok(self.wrap(path, inner))
    }

    /// Renames a file unless a fault fails the rename
    ///
    /// # Arguments
    ///
    /// * `from` - The current path
    /// * `to` - The new path
    async fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        // a failed rename reports an I/O error and leaves both paths as they were
        if fault_for(&self.faults, from, |fault| *fault == SinkFault::FailRename).is_some() {
            return Err(os_error(EIO));
        }
        // rename for real and record it
        TokioFs.rename(from, to).await?;
        self.record(SinkCall::Rename(from.to_path_buf(), to.to_path_buf()));
        Ok(())
    }

    /// Syncs a directory and records it
    ///
    /// # Arguments
    ///
    /// * `dir` - The directory to sync
    async fn sync_dir(&self, dir: &Path) -> std::io::Result<()> {
        // a failed sync reports an I/O error without syncing
        if fault_for(&self.faults, dir, |fault| *fault == SinkFault::FailSync).is_some() {
            return Err(os_error(EIO));
        }
        // sync for real and record it
        TokioFs.sync_dir(dir).await?;
        self.record(SinkCall::SyncDir(dir.to_path_buf()));
        Ok(())
    }

    /// Removes a file and records it
    ///
    /// # Arguments
    ///
    /// * `path` - The file to remove
    async fn remove(&self, path: &Path) -> std::io::Result<()> {
        // remove for real and record it
        TokioFs.remove(path).await?;
        self.record(SinkCall::Remove(path.to_path_buf()));
        Ok(())
    }
}
