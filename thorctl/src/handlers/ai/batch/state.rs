//! Persistent per-run batch state: unit statuses, attempts, and the run lock

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thorium::Error;

use super::scan::UsageLine;

/// Where a unit stands in its run
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UnitStatus {
    /// The unit has not been launched yet
    Pending,
    /// The unit's session is currently running
    Running,
    /// The unit finished and produced a report
    Complete,
    /// The unit exhausted its retries without a report
    Failed,
    /// The unit's session was interrupted and can be continued
    Interrupted,
}

/// How a unit's session is launched
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LaunchMode {
    /// Start a brand-new session
    Fresh,
    /// Continue an interrupted session
    Continue,
    /// Resume only to regenerate the report
    ReportOnly,
}

/// One launch of a unit's session
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attempt {
    /// How this attempt launched the session
    pub mode: LaunchMode,
    /// When this attempt started
    pub started: DateTime<Utc>,
    /// When this attempt ended, once it has
    pub ended: Option<DateTime<Utc>>,
    /// The exit code the session returned, once it exited
    pub exit: Option<i32>,
    /// The stderr log length in bytes when this attempt launched
    ///
    /// Attempts append to one shared log, so usage lines at or past this
    /// offset belong to this attempt alone; the scoreable gate scans from
    /// here so a stale line from an earlier attempt can never vouch for a
    /// later one (an audit finding). Defaults to 0 for pre-existing state
    /// files, which preserves the old whole-log behavior for their units.
    #[serde(default)]
    pub err_offset: u64,
}

/// The persistent state of one unit within a run
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnitState {
    /// The session name this unit runs under
    pub session: String,
    /// The blind alias assigned to this unit
    pub alias: String,
    /// The staged sample paths this unit analyzes
    pub samples: Vec<PathBuf>,
    /// Where this unit stands
    pub status: UnitStatus,
    /// The pid of the live session process, while one runs
    pub pid: Option<u32>,
    /// Every launch this unit has had, oldest first
    pub attempts: Vec<Attempt>,
    /// Whether this unit produced a scoreable report
    pub scoreable: bool,
    /// The last usage summary scanned from the unit's log
    pub usage: Option<UsageLine>,
    /// The last estimated cost scanned from the unit's log
    pub cost: Option<f64>,
    /// Why the unit failed, when it did
    pub fail_reason: Option<String>,
}

/// The persistent state of one batch run
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunState {
    /// The state schema version
    pub version: u32,
    /// The run number this state belongs to
    pub run: u32,
    /// The per-unit states keyed by unit name
    pub units: BTreeMap<String, UnitState>,
    /// How many throttle marker lines this run has seen in total
    pub markers_seen: u64,
    /// When this state was last written
    pub updated: DateTime<Utc>,
}

impl RunState {
    /// Build a fresh empty state for a run
    ///
    /// # Arguments
    ///
    /// * `run` - The run number this state belongs to
    pub fn new(run: u32) -> RunState {
        RunState {
            version: 1,
            run,
            units: BTreeMap::new(),
            markers_seen: 0,
            updated: Utc::now(),
        }
    }

    /// Load a run's state from its run directory
    ///
    /// A missing state file is `Ok(None)`; a corrupt one is an error so a
    /// damaged run is never silently reset.
    ///
    /// # Arguments
    ///
    /// * `run_dir` - The run directory holding state.json
    pub fn load(run_dir: &Path) -> Result<Option<RunState>, Error> {
        // read the state file if one exists
        let path = run_dir.join("state.json");
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            // a missing file just means this run has no state yet
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(Error::new(format!(
                    "failed to read {}: {err}",
                    path.display()
                )));
            }
        };
        // a corrupt state file must stop the run rather than reset it
        let state = serde_json::from_str(&raw)
            .map_err(|err| Error::new(format!("corrupt run state {}: {err}", path.display())))?;
        Ok(Some(state))
    }

    /// Persist this state into a run directory atomically
    ///
    /// # Arguments
    ///
    /// * `run_dir` - The run directory to write state.json into
    pub fn save(&mut self, run_dir: &Path) -> Result<(), Error> {
        // stamp the write time before persisting
        self.updated = Utc::now();
        // make sure the run directory exists before writing into it
        std::fs::create_dir_all(run_dir)
            .map_err(|err| Error::new(format!("failed to create {}: {err}", run_dir.display())))?;
        // serialize the state up front so a failure writes nothing
        let raw = serde_json::to_string_pretty(self)
            .map_err(|err| Error::new(format!("failed to serialize run state: {err}")))?;
        // land the bytes atomically so readers never see a torn state
        write_atomic(&run_dir.join("state.json"), raw.as_bytes())
    }
}

/// Write a file atomically via a same-directory tmp rename
///
/// The tmp file lands beside the target so the rename never crosses a
/// filesystem; a failed write or rename removes the tmp file best effort.
///
/// # Arguments
///
/// * `path` - The target file to write
/// * `bytes` - The bytes to land there
pub(super) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    // the tmp name appends to the full file name so extensions survive
    let file_name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = path.with_file_name(format!("{file_name}.tmp"));
    // write the tmp file, cleaning it up on failure
    if let Err(err) = std::fs::write(&tmp, bytes) {
        let _ = std::fs::remove_file(&tmp);
        return Err(Error::new(format!(
            "failed to write {}: {err}",
            tmp.display()
        )));
    }
    // rename it into place, cleaning up on failure
    if let Err(err) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(Error::new(format!(
            "failed to rename {} into place: {err}",
            tmp.display()
        )));
    }
    Ok(())
}

/// Build the directory a run's state and logs live in
///
/// # Arguments
///
/// * `dir` - The evaluation directory holding the runs tree
/// * `run` - The run number
pub fn run_dir(dir: &Path, run: u32) -> PathBuf {
    // runs nest under one runs/ tree keyed by number
    dir.join("runs").join(run.to_string())
}

/// Find the highest numbered run under an evaluation directory
///
/// # Arguments
///
/// * `dir` - The evaluation directory holding the runs tree
pub fn latest_run(dir: &Path) -> Result<Option<u32>, Error> {
    // list the runs tree if one exists yet
    let runs = dir.join("runs");
    let entries = match std::fs::read_dir(&runs) {
        Ok(entries) => entries,
        // no runs directory means no run has ever started
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(Error::new(format!(
                "failed to list {}: {err}",
                runs.display()
            )));
        }
    };
    // keep the highest child name that parses as a run number
    let mut latest = None;
    for entry in entries {
        let entry =
            entry.map_err(|err| Error::new(format!("failed to list {}: {err}", runs.display())))?;
        // skip children that are not plain run numbers
        if let Some(run) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        {
            latest = latest.max(Some(run));
        }
    }
    Ok(latest)
}

/// The pid and start time recorded inside a run lock file
#[derive(Debug, Serialize, Deserialize)]
struct LockInfo {
    /// The pid of the process holding the lock
    pid: u32,
    /// When the lock was taken
    started: DateTime<Utc>,
}

/// An exclusive per-run lock backed by an O_EXCL lock file
#[derive(Debug)]
pub struct RunLock {
    /// The lock file this lock holds
    path: PathBuf,
}

impl RunLock {
    /// Acquire the exclusive lock for a run directory
    ///
    /// A lock whose recorded pid is dead is broken and retaken once; a lock
    /// held by a live pid is an error naming that pid.
    ///
    /// # Arguments
    ///
    /// * `run_dir` - The run directory to lock
    pub fn acquire(run_dir: &Path) -> Result<RunLock, Error> {
        // make sure the run directory exists so the lock file can be created
        std::fs::create_dir_all(run_dir)
            .map_err(|err| Error::new(format!("failed to create {}: {err}", run_dir.display())))?;
        let path = run_dir.join("batch.lock");
        // try to take the lock, breaking a stale one at most once
        for attempt in 0..2 {
            match RunLock::try_create(&path) {
                Ok(lock) => return Ok(lock),
                // on the first collision, see whether the holder is dead
                Err(err) if err.kind() == ErrorKind::AlreadyExists && attempt == 0 => {
                    RunLock::break_stale(&path)?;
                }
                Err(err) => {
                    return Err(Error::new(format!(
                        "failed to create lock {}: {err}",
                        path.display()
                    )));
                }
            }
        }
        // a second collision means we raced another acquirer
        Err(Error::new(format!(
            "failed to acquire lock {} after breaking a stale one",
            path.display()
        )))
    }

    /// Create the lock file exclusively and record our pid in it
    ///
    /// # Arguments
    ///
    /// * `path` - The lock file to create
    fn try_create(path: &Path) -> Result<RunLock, std::io::Error> {
        use std::io::Write;
        // O_EXCL creation is the actual mutual exclusion
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        // record who holds the lock and since when
        let info = LockInfo {
            pid: std::process::id(),
            started: Utc::now(),
        };
        let raw = serde_json::to_string(&info).map_err(std::io::Error::other)?;
        // a failed write must not leave an unreadable lock behind
        if let Err(err) = file.write_all(raw.as_bytes()) {
            let _ = std::fs::remove_file(path);
            return Err(err);
        }
        Ok(RunLock {
            path: path.to_path_buf(),
        })
    }

    /// Break an existing lock if its recorded holder is dead
    ///
    /// # Arguments
    ///
    /// * `path` - The lock file to inspect
    fn break_stale(path: &Path) -> Result<(), Error> {
        // read whatever the current holder recorded
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            // the holder released between our create and read; just retry
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
            Err(err) => {
                return Err(Error::new(format!(
                    "failed to read lock {}: {err}",
                    path.display()
                )));
            }
        };
        // an unparseable lock came from a crashed writer; treat it as stale
        let Ok(info) = serde_json::from_str::<LockInfo>(&raw) else {
            eprintln!("breaking unreadable lock {}", path.display());
            return RunLock::remove(path);
        };
        // a live holder keeps the lock
        if pid_alive(info.pid) {
            return Err(Error::new(format!(
                "run is locked by live pid {} ({})",
                info.pid,
                path.display()
            )));
        }
        // the recorded pid is gone, so the lock is stale
        eprintln!(
            "breaking stale lock {} held by dead pid {}",
            path.display(),
            info.pid
        );
        RunLock::remove(path)
    }

    /// Remove a stale lock file
    ///
    /// # Arguments
    ///
    /// * `path` - The lock file to remove
    fn remove(path: &Path) -> Result<(), Error> {
        // losing a removal race to another breaker is fine
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
            Err(err) => Err(Error::new(format!(
                "failed to remove stale lock {}: {err}",
                path.display()
            ))),
        }
    }
}

impl Drop for RunLock {
    /// Release the lock by removing its file, best effort
    fn drop(&mut self) {
        // a leaked lock is broken by the stale-pid check anyway
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Check whether a pid is still alive
///
/// # Arguments
///
/// * `pid` - The pid to probe
#[cfg(unix)]
pub(super) fn pid_alive(pid: u32) -> bool {
    // signal 0 probes existence without delivering anything
    let ret = unsafe { libc::kill(pid as i32, 0) };
    // the probe reaching the process means it is alive
    if ret == 0 {
        return true;
    }
    // only ESRCH proves the pid is gone; EPERM means alive but not ours
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Check whether a pid is still alive
///
/// # Arguments
///
/// * `pid` - The pid to probe
#[cfg(not(unix))]
pub(super) fn pid_alive(_pid: u32) -> bool {
    // without a probe, assume the holder lives and never break locks
    true
}

#[cfg(test)]
mod tests {
    use super::*;

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
            "thorctl-batch-state-{name}-{}-{count}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Build a fully populated unit state for round-trip tests
    fn full_unit() -> UnitState {
        UnitState {
            session: "batch1-s01".to_owned(),
            alias: "s01".to_owned(),
            samples: vec![PathBuf::from("/tmp/samples/abc")],
            status: UnitStatus::Complete,
            pid: Some(4242),
            attempts: vec![
                Attempt {
                    mode: LaunchMode::Fresh,
                    started: Utc::now(),
                    ended: Some(Utc::now()),
                    exit: Some(75),
                    err_offset: 0,
                },
                Attempt {
                    mode: LaunchMode::ReportOnly,
                    started: Utc::now(),
                    ended: Some(Utc::now()),
                    exit: Some(0),
                    err_offset: 4096,
                },
            ],
            scoreable: true,
            usage: Some(UsageLine {
                input: 1,
                output: 2,
                cache_read: 3,
                cache_write: 4,
                total: 10,
                agents: 5,
            }),
            cost: Some(12.34),
            fail_reason: Some("flaked once".to_owned()),
        }
    }

    /// Saving then loading a run state preserves every field
    #[test]
    fn save_load_round_trip() {
        let dir = scratch_dir("roundtrip");
        // build a state exercising every field
        let mut state = RunState::new(7);
        state.markers_seen = 9;
        state.units.insert("u1".to_owned(), full_unit());
        // save stamps updated and writes atomically
        state.save(&dir).unwrap();
        assert!(dir.join("state.json").exists());
        // the tmp file never survives a successful save
        assert!(!dir.join("state.json.tmp").exists());
        // loading returns exactly what was saved
        let loaded = RunState::load(&dir).unwrap().unwrap();
        assert_eq!(loaded, state);
    }

    /// Loading a run directory with no state file yields None
    #[test]
    fn load_missing_is_none() {
        let dir = scratch_dir("missing");
        assert_eq!(RunState::load(&dir).unwrap(), None);
    }

    /// A corrupt state file errors with its path, never silently resets
    #[test]
    fn load_corrupt_errors_with_path() {
        let dir = scratch_dir("corrupt");
        // write something that is not json
        std::fs::write(dir.join("state.json"), "{ not json").unwrap();
        let err = RunState::load(&dir).unwrap_err();
        assert!(err.to_string().contains("state.json"), "err was: {err}");
    }

    /// The latest run is the highest numeric child of runs/
    #[test]
    fn latest_run_picks_the_max() {
        let dir = scratch_dir("latest");
        // no runs tree at all means no runs
        assert_eq!(latest_run(&dir).unwrap(), None);
        // numeric children compete; junk is ignored
        for child in ["1", "3", "12", "junk"] {
            std::fs::create_dir_all(dir.join("runs").join(child)).unwrap();
        }
        assert_eq!(latest_run(&dir).unwrap(), Some(12));
        // run_dir builds the matching layout
        assert_eq!(run_dir(&dir, 12), dir.join("runs").join("12"));
    }

    /// A held lock blocks a second acquire, naming the live holder
    #[test]
    fn lock_is_exclusive() {
        let dir = scratch_dir("exclusive");
        // take the lock as ourselves
        let _lock = RunLock::acquire(&dir).unwrap();
        // a second acquire sees our live pid and refuses
        let err = RunLock::acquire(&dir).unwrap_err();
        assert!(
            err.to_string().contains(&std::process::id().to_string()),
            "err was: {err}"
        );
    }

    /// A lock held by a dead pid is broken and retaken
    #[test]
    fn stale_lock_is_broken() {
        let dir = scratch_dir("stale");
        // spawn and reap a short-lived process so its pid is surely dead
        let mut child = std::process::Command::new("/bin/true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        // forge a lock naming the dead pid
        let info = format!("{{\"pid\":{pid},\"started\":\"2026-01-01T00:00:00Z\"}}");
        std::fs::write(dir.join("batch.lock"), info).unwrap();
        // acquiring breaks the stale lock and takes it over
        let _lock = RunLock::acquire(&dir).unwrap();
        // the lock file now records our own pid
        let raw = std::fs::read_to_string(dir.join("batch.lock")).unwrap();
        assert!(raw.contains(&std::process::id().to_string()));
    }

    /// Dropping the lock releases it for the next acquirer
    #[test]
    fn drop_releases_the_lock() {
        let dir = scratch_dir("droplock");
        // hold then drop the lock
        {
            let _lock = RunLock::acquire(&dir).unwrap();
            assert!(dir.join("batch.lock").exists());
        }
        // the file is gone and the lock can be retaken
        assert!(!dir.join("batch.lock").exists());
        let _lock = RunLock::acquire(&dir).unwrap();
    }
}
