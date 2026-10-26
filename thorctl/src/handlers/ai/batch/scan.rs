//! Log scanning for the pool: throttle markers, usage tails, env files, disk

use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use thorium::Error;

/// The token totals printed by a headless session's usage line
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageLine {
    /// Fresh input tokens
    pub input: u64,
    /// Output tokens
    pub output: u64,
    /// Input tokens read from the provider's prompt cache
    pub cache_read: u64,
    /// Input tokens written into the provider's prompt cache
    pub cache_write: u64,
    /// All tokens the session consumed
    pub total: u64,
    /// How many agents contributed to the totals
    pub agents: u64,
}

/// The usage and cost tail scanned out of one unit's stderr log
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UsageTail {
    /// The last usage summary in the log, if any
    pub usage: Option<UsageLine>,
    /// The last estimated cost in the log, if any
    pub cost: Option<f64>,
    /// The model named by the last cost line, if any
    pub model: Option<String>,
    /// How many usage lines the log holds in total
    pub usage_lines: usize,
}

/// Fetch a lazily compiled regex out of a static cell
///
/// # Arguments
///
/// * `cell` - The static cell caching the compiled regex
/// * `pattern` - The pattern to compile on first use
fn regex(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    // the pattern is a trusted literal, so compilation cannot fail
    cell.get_or_init(|| Regex::new(pattern).unwrap())
}

/// The exact usage line a headless run prints on stderr
fn usage_regex() -> &'static Regex {
    static USAGE: OnceLock<Regex> = OnceLock::new();
    regex(
        &USAGE,
        r"^usage: input=(\d+) output=(\d+) cache_read=(\d+) cache_write=(\d+) total=(\d+) agents=(\d+)$",
    )
}

/// The exact estimated-cost line a headless run prints on stderr
fn cost_regex() -> &'static Regex {
    static COST: OnceLock<Regex> = OnceLock::new();
    regex(&COST, r"^estimated cost: \$(\d+\.\d+) \((.+)\)$")
}

/// The throttle markers counted across unit logs
///
/// The 429 alternative is word-bounded so token counts like `input=84290` in
/// the usage lines the pool itself writes never read as throttling.
fn marker_regex() -> &'static Regex {
    static MARKER: OnceLock<Regex> = OnceLock::new();
    regex(&MARKER, r"(?i)(throttl|\b429\b|too many request)")
}

/// Scan a unit's stderr log for its last usage and cost lines
///
/// Attempts append to one log and each prints a session-cumulative pair, so
/// the last lines win. A missing or unreadable log yields an empty tail.
///
/// # Arguments
///
/// * `err_path` - The unit's stderr log to scan
pub fn parse_usage_tail(err_path: &Path) -> UsageTail {
    // scan the whole log so the final cumulative pair wins
    parse_usage_tail_from(err_path, 0)
}

/// Scan a unit's stderr log from a byte offset for its last usage and cost lines
///
/// Passing the log's length at attempt launch scopes the scan to that attempt
/// alone, so a stale usage line appended by an earlier attempt can never mark
/// a later attempt scoreable (an audit finding). A missing or unreadable log
/// yields an empty tail, as does an offset at or past the end of the log.
///
/// # Arguments
///
/// * `err_path` - The unit's stderr log to scan
/// * `offset` - The byte offset to start scanning from, clamped to the log length
pub fn parse_usage_tail_from(err_path: &Path, offset: u64) -> UsageTail {
    // a missing or unreadable log just has no tail yet
    let Ok(raw) = std::fs::read(err_path) else {
        return UsageTail::default();
    };
    // clamp so a truncated or rewritten log can never panic the slice
    let start = usize::try_from(offset).unwrap_or(usize::MAX).min(raw.len());
    // the markers are ascii, so lossy decoding is safe
    let text = String::from_utf8_lossy(&raw[start..]);
    let mut tail = UsageTail::default();
    // walk every line keeping only the last usage and cost matches
    for line in text.lines() {
        // tolerate windows line endings in copied logs
        let line = line.trim_end_matches('\r');
        if let Some(caps) = usage_regex().captures(line) {
            // count every usage line so callers can spot multi-attempt logs
            tail.usage_lines += 1;
            // later usage lines are cumulative, so the last one wins
            tail.usage = Some(UsageLine {
                input: caps[1].parse().unwrap_or(0),
                output: caps[2].parse().unwrap_or(0),
                cache_read: caps[3].parse().unwrap_or(0),
                cache_write: caps[4].parse().unwrap_or(0),
                total: caps[5].parse().unwrap_or(0),
                agents: caps[6].parse().unwrap_or(0),
            });
        } else if let Some(caps) = cost_regex().captures(line) {
            // the last cost line prices the final cumulative usage
            tail.cost = caps[1].parse().ok();
            tail.model = Some(caps[2].to_owned());
        }
    }
    tail
}

/// Parse a shell-style env file into key/value pairs
///
/// Accepts `export K=V`, `K=V`, comments, and blank lines; values may be
/// wrapped in one pair of single or double quotes. Later keys override
/// earlier ones.
///
/// # Arguments
///
/// * `path` - The env file to parse
pub fn parse_env_file(path: &Path) -> Result<Vec<(String, String)>, Error> {
    // read the whole env file up front
    let raw = std::fs::read_to_string(path)
        .map_err(|err| Error::new(format!("failed to read env file {}: {err}", path.display())))?;
    let mut vars: Vec<(String, String)> = Vec::new();
    // parse line by line so errors can name the offending line
    for (idx, line) in raw.lines().enumerate() {
        let number = idx + 1;
        let line = line.trim();
        // skip blanks and comments
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // allow the conventional export prefix
        let line = line.strip_prefix("export ").map_or(line, str::trim_start);
        // split the assignment on its first equals sign
        let Some((key, value)) = line.split_once('=') else {
            return Err(Error::new(format!(
                "{}:{number}: expected KEY=VALUE, got '{line}'",
                path.display()
            )));
        };
        // keys must be valid environment variable names
        let key = key.trim_end();
        if !valid_env_key(key) {
            return Err(Error::new(format!(
                "{}:{number}: invalid environment variable name '{key}'",
                path.display()
            )));
        }
        // strip one matching pair of quotes off the value
        let value = unquote(value.trim());
        // later assignments override earlier ones
        match vars.iter_mut().find(|(existing, _)| existing == key) {
            Some((_, existing)) => *existing = value,
            None => vars.push((key.to_owned(), value)),
        }
    }
    Ok(vars)
}

/// Check that a key is a valid environment variable name
///
/// # Arguments
///
/// * `key` - The key to check
fn valid_env_key(key: &str) -> bool {
    let mut chars = key.chars();
    // the name cannot be empty
    let Some(first) = chars.next() else {
        return false;
    };
    // the first char must be a letter or underscore
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    // the rest must be letters, digits, or underscores
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Strip one matching pair of single or double quotes off a value
///
/// # Arguments
///
/// * `value` - The raw value text
fn unquote(value: &str) -> String {
    let bytes = value.as_bytes();
    // only a matching leading and trailing quote pair strips
    if bytes.len() >= 2
        && (bytes[0] == b'"' || bytes[0] == b'\'')
        && bytes[bytes.len() - 1] == bytes[0]
    {
        return value[1..value.len() - 1].to_owned();
    }
    value.to_owned()
}

/// The incremental scan state for one log file
#[derive(Debug)]
struct FileState {
    /// How many bytes of this file have been consumed
    offset: u64,
    /// The bytes of a trailing line not yet terminated by a newline
    carry: Vec<u8>,
}

/// An incremental throttle-marker counter over a run's stderr logs
#[derive(Debug)]
pub struct MarkerScanner {
    /// The per-file scan state keyed by log path
    files: HashMap<PathBuf, FileState>,
    /// The total matching lines seen so far
    pub total: u64,
}

impl MarkerScanner {
    /// Build an empty marker scanner
    pub fn new() -> MarkerScanner {
        MarkerScanner {
            files: HashMap::new(),
            total: 0,
        }
    }

    /// Fully scan a run's logs to set this scanner's baseline
    ///
    /// Counts every matching line already in every `*.err` file (grep -c
    /// semantics: a line with several markers counts once), then positions
    /// every per-file offset at EOF so `scan` only sees appended bytes.
    ///
    /// # Arguments
    ///
    /// * `run_dir` - The run directory holding the unit logs
    pub fn seed(&mut self, run_dir: &Path) -> Result<u64, Error> {
        // seeding defines a fresh baseline for this scanner
        self.files.clear();
        self.total = 0;
        // scan every log in full and remember where each one ends
        for path in err_files(run_dir)? {
            // read the whole log and count its matching lines
            let raw = std::fs::read(&path)
                .map_err(|err| Error::new(format!("failed to read {}: {err}", path.display())))?;
            self.total += count_marker_lines(&raw);
            // future scans only look at bytes appended past this point
            let state = FileState {
                offset: raw.len() as u64,
                carry: Vec::new(),
            };
            self.files.insert(path, state);
        }
        Ok(self.total)
    }

    /// Incrementally scan a run's logs for newly appended marker lines
    ///
    /// New `*.err` files start at offset zero; only appended bytes are read
    /// per file; a carried partial line is completed by its next append; and
    /// a file that shrank is rescanned from the top.
    ///
    /// # Arguments
    ///
    /// * `run_dir` - The run directory holding the unit logs
    pub fn scan(&mut self, run_dir: &Path) -> Result<u64, Error> {
        use std::io::{Read, Seek, SeekFrom};
        // walk every log, picking up newly created ones at offset zero
        for path in err_files(run_dir)? {
            // fetch or create this file's scan state
            let state = self.files.entry(path.clone()).or_insert_with(|| FileState {
                offset: 0,
                carry: Vec::new(),
            });
            // stat the file to detect shrinks without reading it
            let len = std::fs::metadata(&path)
                .map_err(|err| Error::new(format!("failed to stat {}: {err}", path.display())))?
                .len();
            // a shrunken file was rewritten: rescan it from the top
            if len < state.offset {
                state.offset = 0;
                state.carry.clear();
            }
            // nothing new to read in this file
            if len == state.offset {
                continue;
            }
            // read only the appended bytes
            let mut file = std::fs::File::open(&path)
                .map_err(|err| Error::new(format!("failed to open {}: {err}", path.display())))?;
            file.seek(SeekFrom::Start(state.offset))
                .map_err(|err| Error::new(format!("failed to seek {}: {err}", path.display())))?;
            let mut appended = Vec::new();
            file.read_to_end(&mut appended)
                .map_err(|err| Error::new(format!("failed to read {}: {err}", path.display())))?;
            // advance past exactly what was read
            state.offset += appended.len() as u64;
            // prepend the partial line carried from the last poll
            let mut buffer = std::mem::take(&mut state.carry);
            buffer.extend_from_slice(&appended);
            // count only complete lines, carrying the trailing partial one
            match buffer.iter().rposition(|byte| *byte == b'\n') {
                Some(last_newline) => {
                    // every segment before the final newline is a complete line
                    for line in buffer[..last_newline].split(|byte| *byte == b'\n') {
                        if line_matches(line) {
                            self.total += 1;
                        }
                    }
                    // the tail past the final newline waits for its newline
                    state.carry = buffer[last_newline + 1..].to_vec();
                }
                // no newline at all: the whole buffer stays carried
                None => state.carry = buffer,
            }
        }
        Ok(self.total)
    }
}

impl Default for MarkerScanner {
    /// Build an empty marker scanner
    fn default() -> Self {
        MarkerScanner::new()
    }
}

/// List the unit stderr logs in a run directory
///
/// # Arguments
///
/// * `run_dir` - The run directory to list
fn err_files(run_dir: &Path) -> Result<Vec<PathBuf>, Error> {
    // a run dir that does not exist yet simply has no logs
    let entries = match std::fs::read_dir(run_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(Error::new(format!(
                "failed to list {}: {err}",
                run_dir.display()
            )));
        }
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry
            .map_err(|err| Error::new(format!("failed to list {}: {err}", run_dir.display())))?;
        let path = entry.path();
        // only unit stderr logs count as marker sources
        if path.extension().is_some_and(|ext| ext == "err") && path.is_file() {
            files.push(path);
        }
    }
    // keep the scan order stable across polls
    files.sort();
    Ok(files)
}

/// Count the marker lines in a whole log, grep -c style
///
/// # Arguments
///
/// * `raw` - The log bytes to count over
fn count_marker_lines(raw: &[u8]) -> u64 {
    // a trailing segment without a newline still counts, like grep -c
    raw.split(|byte| *byte == b'\n')
        .filter(|line| line_matches(line))
        .count() as u64
}

/// Check whether one log line contains a throttle marker
///
/// # Arguments
///
/// * `line` - The raw line bytes to test
fn line_matches(line: &[u8]) -> bool {
    // the markers are ascii, so lossy decoding cannot hide or forge one
    marker_regex().is_match(&String::from_utf8_lossy(line))
}

/// Measure the free disk space in whole GiB at a path
///
/// Uses `f_bavail`, the space actually available to unprivileged writers.
///
/// # Arguments
///
/// * `path` - The path whose filesystem to measure
#[cfg(unix)]
pub fn disk_free_gib(path: &Path) -> Result<u64, Error> {
    use std::os::unix::ffi::OsStrExt;
    // statvfs needs the path as a C string
    let cpath = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| Error::new(format!("path {} contains a nul byte", path.display())))?;
    // ask the OS for the filesystem stats
    let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(cpath.as_ptr(), &mut stats) } != 0 {
        return Err(Error::new(format!(
            "statvfs({}) failed: {}",
            path.display(),
            std::io::Error::last_os_error()
        )));
    }
    // widen before multiplying so huge filesystems cannot overflow
    let bytes = u128::from(stats.f_bavail) * u128::from(stats.f_frsize);
    // integer-divide down to whole gibibytes
    Ok(u64::try_from(bytes / (1024 * 1024 * 1024)).unwrap_or(u64::MAX))
}

/// Measure the free disk space in whole GiB at a path
///
/// # Arguments
///
/// * `path` - The path whose filesystem to measure
#[cfg(not(unix))]
pub fn disk_free_gib(_path: &Path) -> Result<u64, Error> {
    // no statvfs here: report unlimited so the disk guard never trips
    Ok(u64::MAX)
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
            "thorctl-batch-scan-{name}-{}-{count}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Append text to a file, creating it if needed
    ///
    /// # Arguments
    ///
    /// * `path` - The file to append to
    /// * `text` - The text to append
    fn append(path: &Path, text: &str) {
        use std::io::Write;
        // append without truncating what a prior step wrote
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        file.write_all(text.as_bytes()).unwrap();
    }

    /// The last usage and cost lines win and usage lines are counted
    #[test]
    fn usage_tail_last_lines_win() {
        let dir = scratch_dir("usagetail");
        let log = dir.join("u1.err");
        // two attempts appended two cumulative usage/cost pairs
        let text = "some chatter\n\
            usage: input=10 output=20 cache_read=30 cache_write=40 total=100 agents=2\n\
            estimated cost: $1.0000 (model-a)\n\
            usage: input=1 output=2 near miss without the rest\n\
            retrying after a failure\n\
            usage: input=11 output=21 cache_read=31 cache_write=41 total=104 agents=3\n\
            estimated cost: $2.5000 (model-b)\n\
            trailing noise\n";
        std::fs::write(&log, text).unwrap();
        let tail = parse_usage_tail(&log);
        // the second full pair wins and both full lines were counted
        assert_eq!(
            tail.usage,
            Some(UsageLine {
                input: 11,
                output: 21,
                cache_read: 31,
                cache_write: 41,
                total: 104,
                agents: 3
            })
        );
        assert_eq!(tail.cost, Some(2.5));
        assert_eq!(tail.model.as_deref(), Some("model-b"));
        assert_eq!(tail.usage_lines, 2);
    }

    /// An offset scopes the scan to one attempt's bytes (stale-usage gate)
    #[test]
    fn usage_tail_offset_scopes_to_one_attempt() {
        let dir = scratch_dir("usageoffset");
        let log = dir.join("u1.err");
        // the first attempt exits cleanly and prints its cumulative pair
        let first = "first attempt chatter\n\
            usage: input=10 output=20 cache_read=30 cache_write=40 total=100 agents=2\n\
            estimated cost: $1.0000 (model-a)\n";
        std::fs::write(&log, first).unwrap();
        let offset = std::fs::metadata(&log).unwrap().len();
        // the second attempt dies before printing any usage line
        append(&log, "second attempt chatter\nkilled before usage\n");
        // a whole-log scan still sees the stale line, the scoped scan does not
        assert_eq!(parse_usage_tail(&log).usage_lines, 1);
        assert_eq!(parse_usage_tail_from(&log, offset), UsageTail::default());
        // an offset past the end of the log is clamped, not a panic
        assert_eq!(parse_usage_tail_from(&log, u64::MAX), UsageTail::default());
        // a third attempt that does print is seen from its own offset
        let offset = std::fs::metadata(&log).unwrap().len();
        append(
            &log,
            "usage: input=11 output=21 cache_read=31 cache_write=41 total=104 agents=3\n",
        );
        assert_eq!(parse_usage_tail_from(&log, offset).usage_lines, 1);
    }

    /// Logs without usage lines and missing logs yield empty tails
    #[test]
    fn usage_tail_empty_cases() {
        let dir = scratch_dir("usageempty");
        // a log with no usage lines at all
        let log = dir.join("quiet.err");
        std::fs::write(&log, "hello\nworld\n").unwrap();
        assert_eq!(parse_usage_tail(&log), UsageTail::default());
        // a log that does not exist
        assert_eq!(
            parse_usage_tail(&dir.join("missing.err")),
            UsageTail::default()
        );
    }

    /// The env parser handles exports, quotes, comments, and overrides
    #[test]
    fn env_file_parses_and_overrides() {
        let dir = scratch_dir("envgood");
        let env = dir.join("creds.env");
        let text = "# a comment\n\
            \n\
            export API_KEY=abc123\n\
            PLAIN=value\n\
            QUOTED=\"hello world\"\n\
            SINGLE='single quoted'\n\
            OVERRIDE=first\n\
            export OVERRIDE=second\n";
        std::fs::write(&env, text).unwrap();
        let vars = parse_env_file(&env).unwrap();
        assert_eq!(
            vars,
            vec![
                ("API_KEY".to_owned(), "abc123".to_owned()),
                ("PLAIN".to_owned(), "value".to_owned()),
                ("QUOTED".to_owned(), "hello world".to_owned()),
                ("SINGLE".to_owned(), "single quoted".to_owned()),
                ("OVERRIDE".to_owned(), "second".to_owned()),
            ]
        );
    }

    /// Bad env lines error naming their line number
    #[test]
    fn env_file_errors_name_the_line() {
        let dir = scratch_dir("envbad");
        // an invalid key errors with its line number
        let env = dir.join("badkey.env");
        std::fs::write(&env, "GOOD=1\n1BAD=x\n").unwrap();
        let err = parse_env_file(&env).unwrap_err();
        assert!(err.to_string().contains(":2:"), "err was: {err}");
        assert!(err.to_string().contains("1BAD"), "err was: {err}");
        // a line with no assignment errors with its line number
        let env = dir.join("noeq.env");
        std::fs::write(&env, "justwords\n").unwrap();
        let err = parse_env_file(&env).unwrap_err();
        assert!(err.to_string().contains(":1:"), "err was: {err}");
    }

    /// The marker scanner seeds, tails appends, and completes partial lines
    #[test]
    fn marker_scanner_counts_lines() {
        let dir = scratch_dir("markers");
        let log = dir.join("u1.err");
        // the baseline log holds exactly one marker line
        std::fs::write(&log, "starting up\nHTTP 429 from provider\nall fine\n").unwrap();
        let mut scanner = MarkerScanner::new();
        assert_eq!(scanner.seed(&dir).unwrap(), 1);
        // append one marker line, one line with several markers, and a partial
        append(&log, "throttled by upstream\n");
        append(&log, "saw 429 after Too Many Requests throttling\n");
        append(&log, "partial 429 tail");
        // the multi-marker line counts once and the partial not at all
        assert_eq!(scanner.scan(&dir).unwrap(), 3);
        // completing the partial line counts it exactly once
        append(&log, " now terminated\n");
        assert_eq!(scanner.scan(&dir).unwrap(), 4);
        // a brand-new log is discovered mid-run at offset zero
        let other = dir.join("u2.err");
        std::fs::write(&other, "Too Many Requests\n").unwrap();
        assert_eq!(scanner.scan(&dir).unwrap(), 5);
        // a shrunken log is rescanned from the top
        std::fs::write(&log, "429\n").unwrap();
        assert_eq!(scanner.scan(&dir).unwrap(), 6);
        // a scan with nothing new changes nothing
        assert_eq!(scanner.scan(&dir).unwrap(), 6);
        assert_eq!(scanner.total, 6);
    }

    /// Token counts containing 429 never read as throttle markers
    #[test]
    fn marker_429_is_word_bounded() {
        let dir = scratch_dir("bounded");
        let log = dir.join("u1.err");
        // the usage tail the pool itself writes often embeds 429 in counts
        std::fs::write(
            &log,
            "usage: input=84290 output=14291 cache_read=0 cache_write=0 total=1442900 agents=2\n\
             elapsed 1429ms\n\
             HTTP 429 from provider\n",
        )
        .unwrap();
        // only the real throttle line counts
        let mut scanner = MarkerScanner::new();
        assert_eq!(scanner.seed(&dir).unwrap(), 1);
    }

    /// Seeding counts an unterminated final line like grep -c does
    #[test]
    fn marker_seed_counts_unterminated_tails() {
        let dir = scratch_dir("seedtail");
        // the final line matches but has no newline
        std::fs::write(dir.join("u1.err"), "fine\nthrottling hard").unwrap();
        let mut scanner = MarkerScanner::new();
        assert_eq!(scanner.seed(&dir).unwrap(), 1);
        // seeding an empty or missing run dir is just zero
        let empty = scratch_dir("seedempty");
        assert_eq!(scanner.seed(&empty).unwrap(), 0);
        assert_eq!(scanner.seed(&empty.join("missing")).unwrap(), 0);
    }

    /// The disk guard reports a plausible nonzero free space on linux
    #[cfg(unix)]
    #[test]
    fn disk_free_is_plausible() {
        // the root filesystem always has some free space on a working box
        let free = disk_free_gib(Path::new("/")).unwrap();
        assert!(free > 0, "free was: {free}");
        // a missing path errors instead of guessing
        assert!(disk_free_gib(Path::new("/definitely/not/a/path")).is_err());
    }
}
