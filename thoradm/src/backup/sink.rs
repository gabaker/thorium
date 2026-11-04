//! The file-system seam thoradm's writers open files through
//!
//! Archives, downloaded objects, and `redis.json` are written through a [`SinkFactory`] rather
//! than through `tokio::fs` directly, so tests can substitute a factory that injects disk faults
//! (a full disk, short writes, failed syncs or renames) and records every call. Production uses
//! [`TokioFs`], a thin wrapper over `tokio::fs`.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncWriteExt, BufWriter};

/// The capacity of the write buffer in front of each file opened by [`TokioFs`]
const WRITE_BUFFER: usize = 1_048_576;

/// The mode every directory a backup creates is given, before the umask, unless its root is shared
/// with its group (see [`BackupAccess`]): a backup holds every row, object, and credential of a
/// cluster
pub const PRIVATE_DIR_MODE: u32 = 0o700;

/// The mode every file a backup writes is created with, before the umask, unless its root is
/// shared with its group (see [`BackupAccess`])
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

/// The setgid bit, which Linux copies from a directory to the directories made in it, and which
/// gives files made in such a directory the directory's group
const SETGID: u32 = 0o2000;

/// Who besides its owner may use what a backup writes, taken once from the backup root's mode
///
/// A backup root may be shared with its group (an admin group, for example) but never with other
/// users. Only a setgid root is shared, since the setgid bit is how an admin says the directory
/// belongs to a group. Every directory a shared backup creates gets the root's group bits, and
/// every file the root's group read and write bits (never execute); nothing a backup creates gets
/// an "other" bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackupAccess {
    /// The backup root's group permission bits, `0o070` at most
    group: u32,
}

impl BackupAccess {
    /// The access of a backup only its owner may use
    pub const PRIVATE: Self = Self { group: 0 };

    /// Takes the access a backup root's mode gives its group: its group bits when it is setgid,
    /// and none otherwise
    ///
    /// # Arguments
    ///
    /// * `mode` - The backup root's mode
    #[must_use]
    pub fn from_root_mode(mode: u32) -> Self {
        // only a setgid root shares, and only the group's bits; other users never get access
        if mode & SETGID == 0 {
            return Self::PRIVATE;
        }
        Self {
            group: mode & 0o070,
        }
    }

    /// Returns the mode a backup root must have before anything is written in it
    ///
    /// A setgid root keeps its group bits and its setgid bit and loses every "other" bit. Any other
    /// root is made private to its owner (`0700`).
    ///
    /// # Arguments
    ///
    /// * `mode` - The backup root's mode
    #[must_use]
    pub fn root_mode(mode: u32) -> u32 {
        // a root that isn't setgid isn't shared, so only its owner keeps access
        if mode & SETGID == 0 {
            return PRIVATE_DIR_MODE;
        }
        // drop the file type bits and every bit other users have
        mode & 0o7777 & !0o007
    }

    /// Whether the root's group may use the backup
    #[must_use]
    pub fn is_shared(self) -> bool {
        self.group != 0
    }

    /// Whether the root grants its group access without the search (execute) bit, so group members
    /// can't enter the backup's directories
    #[must_use]
    pub fn group_cannot_enter(self) -> bool {
        self.is_shared() && self.group & 0o010 == 0
    }

    /// Returns the permission bits of every directory a backup creates
    #[must_use]
    pub fn dir_mode(self) -> u32 {
        PRIVATE_DIR_MODE | self.group
    }

    /// Returns the permission bits a file a writer asks for gets: the owner's bits it asks for, and
    /// the owner's read and write bits for the group where the root grants them
    ///
    /// # Arguments
    ///
    /// * `mode` - The owner-only mode the writer asks for
    #[must_use]
    pub fn file_mode(self, mode: u32) -> u32 {
        // the group gets no more than the owner's read and write bits and the root's group bits
        (mode & 0o700) | (((mode & 0o600) >> 3) & self.group)
    }
}

/// Creates a directory and any missing parents with a backup's access
///
/// A private backup's directories are created as by [`create_private_dir`]. A shared backup's
/// new directories have their bits set after creation, since the umask may strip the group's.
///
/// # Arguments
///
/// * `dir` - The directory to create
/// * `access` - The access taken from the backup root
pub async fn create_backup_dir(dir: &Path, access: BackupAccess) -> std::io::Result<()> {
    // a private backup's directories need nothing past their creation mode
    if !access.is_shared() {
        return create_private_dir(dir).await;
    }
    // find the directories on the path that don't exist yet, which alone get the group's bits
    let mut missing: Vec<PathBuf> = Vec::new();
    let mut next = Some(dir);
    while let Some(candidate) = next {
        if tokio::fs::try_exists(candidate).await? {
            break;
        }
        missing.push(candidate.to_path_buf());
        next = candidate.parent();
    }
    // create them, then set each one's bits exactly
    tokio::fs::DirBuilder::new()
        .recursive(true)
        .mode(access.dir_mode())
        .create(dir)
        .await?;
    for created in missing {
        // open the directory without following a symlink, so a group member who swapped one in
        // can't make this change the mode of what it names
        let handle = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(&created)
            .await?;
        // keep the setgid bit Linux copied from a setgid parent, so files keep the shared group
        let current = handle.metadata().await?.permissions().mode();
        let wanted = access.dir_mode() | (current & SETGID);
        if current & 0o7777 != wanted {
            handle
                .set_permissions(std::fs::Permissions::from_mode(wanted))
                .await?;
        }
    }
    Ok(())
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

    /// Gives the open file a mode exactly, changing it only when its permission bits differ
    ///
    /// # Arguments
    ///
    /// * `mode` - The permission bits the file must have
    async fn set_mode(&mut self, mode: u32) -> std::io::Result<()>;
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

    /// Creates a directory and any missing parents, each private to this user unless the factory
    /// gives them a backup's access
    ///
    /// # Arguments
    ///
    /// * `dir` - The directory to create
    async fn create_dir(&self, dir: &Path) -> std::io::Result<()> {
        // directories are created private unless a factory says otherwise
        create_private_dir(dir).await
    }
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

    /// Gives the open file a mode exactly, changing it only when its permission bits differ
    ///
    /// The mode is changed through the open descriptor, never by path, so it lands on the file
    /// this sink writes.
    ///
    /// # Arguments
    ///
    /// * `mode` - The permission bits the file must have
    async fn set_mode(&mut self, mode: u32) -> std::io::Result<()> {
        // a file that already has the mode is left alone, since only its owner may change it
        let current = self.file.get_ref().metadata().await?.permissions().mode();
        if current & 0o7777 != mode {
            self.file
                .get_ref()
                .set_permissions(std::fs::Permissions::from_mode(mode))
                .await?;
        }
        Ok(())
    }
}

/// The production [`SinkFactory`], a thin wrapper over `tokio::fs`
#[derive(Debug, Clone, Copy, Default)]
pub struct TokioFs;

#[async_trait::async_trait]
impl SinkFactory for TokioFs {
    /// Creates (or truncates) a file with a mode, refusing a symlink at its path
    ///
    /// # Arguments
    ///
    /// * `path` - The file to create
    /// * `mode` - The Unix permission bits to create it with, before the umask
    async fn create(&self, path: &Path, mode: u32) -> std::io::Result<Box<dyn FileSink>> {
        // open the file for writing, creating or truncating it with the requested mode; a symlink
        // is refused, since a group member sharing the directory could have planted it
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .await?;
        Ok(Box::new(TokioFile {
            file: BufWriter::with_capacity(WRITE_BUFFER, file),
        }))
    }

    /// Opens a file for appending, creating it with mode 0600 when missing and refusing a
    /// symlink at its path
    ///
    /// # Arguments
    ///
    /// * `path` - The file to append to
    async fn append(&self, path: &Path) -> std::io::Result<Box<dyn FileSink>> {
        // open the file for appending, creating it private to this user when missing; a symlink
        // is refused, since a group member sharing the directory could have planted it
        let file = OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
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

/// A [`SinkFactory`] that gives every file and directory a backup creates the access taken from
/// its root, opening files through another factory
pub struct BackupSinks {
    /// The factory files are opened through
    inner: Arc<dyn SinkFactory>,
    /// The access taken from the backup root
    access: BackupAccess,
}

impl BackupSinks {
    /// Wraps a factory so what it creates gets a backup root's access
    ///
    /// # Arguments
    ///
    /// * `inner` - The factory files are opened through
    /// * `access` - The access taken from the backup root
    #[must_use]
    pub fn new(inner: Arc<dyn SinkFactory>, access: BackupAccess) -> Self {
        BackupSinks { inner, access }
    }

    /// Gives a shared backup's open file its mode exactly, since the umask may strip the group's
    /// bits from a new file and a file opened again keeps the mode it had
    ///
    /// A file that already has its mode is left alone, so a group member can resume a backup
    /// another member started without changing files it doesn't own.
    ///
    /// # Arguments
    ///
    /// * `file` - The open file
    /// * `mode` - The file's mode
    async fn set_mode(&self, file: &mut dyn FileSink, mode: u32) -> std::io::Result<()> {
        // a private backup's files keep their creation mode
        if !self.access.is_shared() {
            return Ok(());
        }
        // the mode is set through the open file, never by path
        file.set_mode(mode).await
    }
}

#[async_trait::async_trait]
impl SinkFactory for BackupSinks {
    /// Creates (or truncates) a file with the mode asked for, widened to the backup's group
    ///
    /// # Arguments
    ///
    /// * `path` - The file to create
    /// * `mode` - The owner-only mode the writer asks for
    async fn create(&self, path: &Path, mode: u32) -> std::io::Result<Box<dyn FileSink>> {
        // open the file with the backup's mode, then make sure the umask didn't narrow it
        let mode = self.access.file_mode(mode);
        let mut file = self.inner.create(path, mode).await?;
        self.set_mode(file.as_mut(), mode).await?;
        Ok(file)
    }

    /// Opens a file for appending with the backup's file mode
    ///
    /// # Arguments
    ///
    /// * `path` - The file to append to
    async fn append(&self, path: &Path) -> std::io::Result<Box<dyn FileSink>> {
        // open the file, then give it the mode a private file gets, widened to the backup's group
        let mut file = self.inner.append(path).await?;
        self.set_mode(file.as_mut(), self.access.file_mode(PRIVATE_FILE_MODE))
            .await?;
        Ok(file)
    }

    /// Renames a file within one file system
    ///
    /// # Arguments
    ///
    /// * `from` - The current path
    /// * `to` - The new path
    async fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        // a renamed file keeps its mode
        self.inner.rename(from, to).await
    }

    /// Syncs a directory so renames and creations in it are durable
    ///
    /// # Arguments
    ///
    /// * `dir` - The directory to sync
    async fn sync_dir(&self, dir: &Path) -> std::io::Result<()> {
        // syncing changes no modes
        self.inner.sync_dir(dir).await
    }

    /// Removes a file, succeeding when it is already gone
    ///
    /// # Arguments
    ///
    /// * `path` - The file to remove
    async fn remove(&self, path: &Path) -> std::io::Result<()> {
        // removing changes no modes
        self.inner.remove(path).await
    }

    /// Creates a directory and any missing parents with the backup's access
    ///
    /// # Arguments
    ///
    /// * `dir` - The directory to create
    async fn create_dir(&self, dir: &Path) -> std::io::Result<()> {
        // new directories get the root's group bits
        create_backup_dir(dir, self.access).await
    }
}

#[cfg(test)]
mod tests {
    use super::{BackupAccess, BackupSinks, PRIVATE_FILE_MODE, SinkFactory, TokioFs};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;

    /// A root only its owner may use gives directories 0700 and files 0600
    #[test]
    fn a_private_root_keeps_everything_private() {
        // a 0700 root shares nothing
        let access = BackupAccess::from_root_mode(0o700);
        assert_eq!(access, BackupAccess::PRIVATE);
        assert!(!access.is_shared());
        assert_eq!(BackupAccess::root_mode(0o40700), 0o700);
        assert_eq!(access.dir_mode(), 0o700);
        assert_eq!(access.file_mode(PRIVATE_FILE_MODE), 0o600);
    }

    /// A root its group may use but without the setgid bit isn't shared: it is made 0700, and
    /// gives directories 0700 and files 0600
    #[test]
    fn a_group_root_without_setgid_is_private() {
        // 0770 and 0775 become 0700 and share nothing
        assert_eq!(BackupAccess::root_mode(0o770), 0o700);
        assert_eq!(BackupAccess::root_mode(0o775), 0o700);
        let access = BackupAccess::from_root_mode(0o770);
        assert_eq!(access, BackupAccess::PRIVATE);
        assert_eq!(access.dir_mode(), 0o700);
        assert_eq!(access.file_mode(PRIVATE_FILE_MODE), 0o600);
    }

    /// A setgid root its group may read, write, and enter gives directories 0770 and files 0660
    #[test]
    fn a_setgid_group_root_shares_with_its_group() {
        // 2770 keeps every group bit, and files never get the execute bit
        let access = BackupAccess::from_root_mode(0o2770);
        assert!(access.is_shared());
        assert!(!access.group_cannot_enter());
        assert_eq!(BackupAccess::root_mode(0o2770), 0o2770);
        assert_eq!(access.dir_mode(), 0o770);
        assert_eq!(access.file_mode(PRIVATE_FILE_MODE), 0o660);
    }

    /// A setgid root its group may only read and enter keeps its setgid bit, and gives directories
    /// 0750 and files 0640
    #[test]
    fn a_setgid_read_only_group_root_keeps_its_setgid_bit() {
        // the root keeps setgid; the bits it gives what the backup creates are permission bits only
        let access = BackupAccess::from_root_mode(0o2750);
        assert_eq!(BackupAccess::root_mode(0o2750), 0o2750);
        assert_eq!(access.dir_mode(), 0o750);
        assert_eq!(access.file_mode(PRIVATE_FILE_MODE), 0o640);
    }

    /// A setgid root other users can use loses only their bits, keeping its group's and its setgid
    /// bit, while a root open to everyone without setgid is made private
    #[test]
    fn a_root_open_to_everyone_loses_only_the_other_bits() {
        // 2777 becomes 2770 and 2775 becomes 2770, while 0777 becomes 0700
        assert_eq!(BackupAccess::root_mode(0o2777), 0o2770);
        assert_eq!(BackupAccess::root_mode(0o2775), 0o2770);
        assert_eq!(BackupAccess::root_mode(0o777), 0o700);
        let access = BackupAccess::from_root_mode(BackupAccess::root_mode(0o2777));
        assert_eq!(access.dir_mode(), 0o770);
        assert_eq!(access.file_mode(PRIVATE_FILE_MODE), 0o660);
    }

    /// A setgid root that gives its group read and write but no search bit is accepted, flagged as
    /// one the group can't enter
    #[test]
    fn a_group_root_without_search_is_flagged() {
        // 2760 has no other bits, so it stays, but the group can't enter its directories
        assert_eq!(BackupAccess::root_mode(0o2760), 0o2760);
        let access = BackupAccess::from_root_mode(0o2760);
        assert!(access.is_shared());
        assert!(access.group_cannot_enter());
        assert_eq!(access.dir_mode(), 0o760);
        assert_eq!(access.file_mode(PRIVATE_FILE_MODE), 0o660);
    }

    /// The variable that tells a test binary it is the child process a umask test runs in
    const UMASK_CHILD: &str = "THORADM_SINK_UMASK_CHILD";

    /// Files and directories made through a shared backup's factory get the group's bits under a
    /// umask of 022, which strips the group's write bit from what is created, so only an explicit
    /// mode change can give it back
    ///
    /// The umask is process-wide, so the check runs in a child process running this test alone.
    #[test]
    fn a_shared_backup_sets_group_bits_whatever_the_umask() {
        // the child sets the umask and runs the check on its own runtime
        if std::env::var_os(UMASK_CHILD).is_some() {
            // SAFETY: umask only swaps the process's file creation mask, and this child runs this
            // one test on one thread
            unsafe { libc::umask(0o022) };
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(shared_backup_modes_survive_the_umask());
            return;
        }
        // run this test alone in a child process of this test binary
        let module = module_path!().split_once("::").map_or("", |(_, rest)| rest);
        let name = format!("{module}::a_shared_backup_sets_group_bits_whatever_the_umask");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &name, "--test-threads=1", "--nocapture"])
            .env(UMASK_CHILD, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        // the child must have run exactly this test and passed it
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "child failed:\n{stdout}\n{stderr}"
        );
    }

    /// Checks that a shared backup's directories get 2770 and its files 0660 whatever the
    /// process's umask
    async fn shared_backup_modes_survive_the_umask() {
        // a setgid root its group may use
        let root = scratch("shared");
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o2770)).unwrap();
        let access = BackupAccess::from_root_mode(0o2770);
        let sinks = BackupSinks::new(Arc::new(TokioFs), access);
        // a nested directory gets 0770 and keeps the setgid bit it inherited
        let nested = root.join("table").join("data");
        sinks.create_dir(&nested).await.unwrap();
        for dir in [root.join("table"), nested.clone()] {
            let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o7777;
            assert_eq!(mode, 0o2770, "{} is {mode:o}", dir.display());
        }
        // a created file and an appended one get 0660
        let created = nested.join("archive");
        let mut file = sinks.create(&created, PRIVATE_FILE_MODE).await.unwrap();
        file.sync().await.unwrap();
        let appended = root.join("failures.jsonl");
        let mut file = sinks.append(&appended).await.unwrap();
        file.sync().await.unwrap();
        for path in [created, appended] {
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777;
            assert_eq!(mode, 0o660, "{} is {mode:o}", path.display());
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A shared backup's factory refuses to create or append through a symlink a group member
    /// planted, leaving the file it names, or the path a dangling one names, untouched
    #[tokio::test]
    async fn a_shared_backup_never_follows_a_planted_symlink() {
        // a shared root, and a private file outside it
        let root = scratch("planted");
        let outside = scratch("planted-outside");
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o2770)).unwrap();
        let precious = outside.join("precious");
        std::fs::write(&precious, b"PRECIOUS").unwrap();
        std::fs::set_permissions(&precious, std::fs::Permissions::from_mode(0o600)).unwrap();
        let sinks = BackupSinks::new(Arc::new(TokioFs), BackupAccess::from_root_mode(0o2770));
        // symlinks at the paths the backup creates and appends to
        let created = root.join("objects.index.jsonl");
        let appended = root.join("failures.jsonl");
        let dangling = root.join("redis.json");
        std::os::unix::fs::symlink(&precious, &created).unwrap();
        std::os::unix::fs::symlink(&precious, &appended).unwrap();
        std::os::unix::fs::symlink(outside.join("made"), &dangling).unwrap();
        // every open through them fails
        assert!(sinks.create(&created, PRIVATE_FILE_MODE).await.is_err());
        assert!(sinks.append(&appended).await.is_err());
        assert!(sinks.create(&dangling, PRIVATE_FILE_MODE).await.is_err());
        // the outside file keeps its bytes and mode, and nothing was created outside
        assert_eq!(std::fs::read(&precious).unwrap(), b"PRECIOUS");
        let mode = std::fs::metadata(&precious).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o600);
        assert!(!outside.join("made").exists());
        std::fs::remove_dir_all(&root).unwrap();
        std::fs::remove_dir_all(&outside).unwrap();
    }

    /// A private backup's factory creates files 0600 and directories 0700
    #[tokio::test]
    async fn a_private_backup_creates_private_files() {
        // a private root's factory
        let root = scratch("private");
        let sinks = BackupSinks::new(Arc::new(TokioFs), BackupAccess::PRIVATE);
        // a nested directory and a file in it
        let nested = root.join("table");
        sinks.create_dir(&nested).await.unwrap();
        let path = nested.join("archive");
        let mut file = sinks.create(&path, PRIVATE_FILE_MODE).await.unwrap();
        file.sync().await.unwrap();
        assert_eq!(
            std::fs::metadata(&nested).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

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
