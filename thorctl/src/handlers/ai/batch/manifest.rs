//! The batch manifest: the per-evaluation batch.yml settings and unit list

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thorium::Error;

/// Where the password-protected dataset zips live and how to open them
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DatasetConf {
    /// The root directory walked for the dataset's sample zips
    pub root: PathBuf,
    /// The password the dataset zips are encrypted with
    #[serde(default = "default_dataset_password")]
    pub password: String,
}

/// The default password malware dataset zips are conventionally encrypted with
fn default_dataset_password() -> String {
    "infected".to_owned()
}

/// The adaptive concurrency bounds for the rolling session pool
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Concurrency {
    /// The number of sessions the pool starts with
    #[serde(default = "default_concurrency_start")]
    pub start: usize,
    /// The most sessions the pool may ramp up to
    #[serde(default = "default_concurrency_max")]
    pub max: usize,
    /// The fewest sessions the pool may back off to under throttling
    #[serde(default = "default_concurrency_floor")]
    pub floor: usize,
}

impl Default for Concurrency {
    /// Build the default 4/8/3 concurrency bounds
    fn default() -> Self {
        Concurrency {
            start: default_concurrency_start(),
            max: default_concurrency_max(),
            floor: default_concurrency_floor(),
        }
    }
}

/// The default number of sessions the pool starts with
fn default_concurrency_start() -> usize {
    4
}

/// The default ceiling for the pool's adaptive concurrency
fn default_concurrency_max() -> usize {
    8
}

/// The default floor for the pool's adaptive concurrency
fn default_concurrency_floor() -> usize {
    3
}

/// A reference to one sample: a dataset sha256 or a file on disk
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SampleRef {
    /// A sample staged out of the dataset zips by its sha256
    Sha256(String),
    /// A sample already sitting on disk at this path
    Path(PathBuf),
}

impl SampleRef {
    /// Classify a raw manifest string as a sha256 or a path
    ///
    /// # Arguments
    ///
    /// * `raw` - The plain string from the manifest
    fn classify(raw: &str) -> SampleRef {
        // exactly 64 ascii hex chars is a sha256; anything else is a path
        if raw.len() == 64 && raw.chars().all(|c| c.is_ascii_hexdigit()) {
            // normalize shas to the index's lowercase convention
            SampleRef::Sha256(raw.to_ascii_lowercase())
        } else {
            SampleRef::Path(PathBuf::from(raw))
        }
    }
}

impl Serialize for SampleRef {
    /// Serialize a sample ref back to the plain string it was written as
    ///
    /// # Arguments
    ///
    /// * `serializer` - The serializer to write with
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // both variants round trip through one plain string
        match self {
            SampleRef::Sha256(sha) => serializer.serialize_str(sha),
            SampleRef::Path(path) => serializer.serialize_str(&path.to_string_lossy()),
        }
    }
}

impl<'de> Deserialize<'de> for SampleRef {
    /// Deserialize a sample ref from a plain yaml string
    ///
    /// # Arguments
    ///
    /// * `deserializer` - The deserializer to read from
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // a sample ref is always written as a plain string
        let raw = String::deserialize(deserializer)?;
        // the shape of the string decides which variant it is
        Ok(SampleRef::classify(&raw))
    }
}

/// One evaluation unit: a named analysis over one or more samples
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UnitSpec {
    /// The unique name of this unit within the manifest
    pub name: String,
    /// The samples this unit analyzes; the first is the primary sample
    pub samples: Vec<SampleRef>,
}

/// The per-evaluation batch.yml settings and unit list
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct BatchConf {
    /// The prompt text given to every unit's session
    #[serde(default)]
    pub prompt: Option<String>,
    /// A file holding the prompt instead of inlining it
    #[serde(default)]
    pub prompt_file: Option<PathBuf>,
    /// The dataset the sha256 sample refs are staged from
    #[serde(default)]
    pub dataset: Option<DatasetConf>,
    /// Whether units run blind under shuffled aliases
    #[serde(default = "default_true")]
    pub blind: bool,
    /// The session name template; `{run}` and `{alias}` are substituted
    #[serde(default = "default_name_template")]
    pub name_template: String,
    /// Whether sessions analyze anonymized copies of the samples
    #[serde(default = "default_true")]
    pub anonymize: bool,
    /// Whether sessions always create a fresh Ghidra project
    #[serde(default = "default_true")]
    pub fresh_project: bool,
    /// Whether session reports are defanged on the way out
    #[serde(default)]
    pub defang: bool,
    /// A deny-token file forwarded to every session as `--redact-tokens`, so
    /// library surfaces scrub the corpus's family names under blinding
    #[serde(default)]
    pub redact_tokens: Option<PathBuf>,
    /// Whether the static detector passes are disabled for the sessions
    #[serde(default)]
    pub disable_detectors: bool,
    /// A report template file passed through to every session
    #[serde(default)]
    pub report_template: Option<PathBuf>,
    /// An env file sourced before each spawn for per-spawn credentials
    #[serde(default)]
    pub env_file: Option<PathBuf>,
    /// The raw `--pause-wait` value passed through to child sessions
    #[serde(default = "default_pause_wait")]
    pub pause_wait: String,
    /// The adaptive concurrency bounds for the pool
    #[serde(default)]
    pub concurrency: Concurrency,
    /// How many seconds the pool sleeps between supervision polls
    #[serde(default = "default_poll_secs")]
    pub poll_secs: u64,
    /// How many seconds the pool staggers consecutive spawns by
    #[serde(default = "default_stagger_secs")]
    pub stagger_secs: u64,
    /// The free-disk floor in GiB below which spawns are held
    #[serde(default = "default_disk_free_min_gb")]
    pub disk_free_min_gb: u64,
    /// The path whose filesystem the free-disk guard measures
    #[serde(default = "default_disk_path")]
    pub disk_path: PathBuf,
    /// The main agent loop's turn cap for every child session, when set
    ///
    /// An evaluation's hardest unit finished its analysis and died at the default
    /// 100-turn cap during wrap-up; long evaluations raise this here.
    #[serde(default)]
    pub max_turns: Option<usize>,
    /// How many times a failed unit is relaunched before giving up
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    /// How many report-only relaunches a report failure gets
    #[serde(default = "default_report_retries")]
    pub report_retries: u32,
    /// The evaluation units this manifest drives
    pub units: Vec<UnitSpec>,
    /// The directory this manifest was loaded from (derived, not in the yaml)
    #[serde(skip)]
    pub dir: PathBuf,
}

/// The default for settings that are on unless opted out of
fn default_true() -> bool {
    true
}

/// The default session name template
fn default_name_template() -> String {
    "batch{run}-{alias}".to_owned()
}

/// The default pause-wait passthrough value
///
/// `4h` (not `none`): a pooled evaluation is long-lived, and one lost
/// sessions twice to ~12h credential rotations the pause gate had already
/// detected — children now hold through a refresh (a config-file touch
/// re-probes immediately) instead of dying and burning retry budgets.
/// Single interactive sessions keep the CLI's fail-loud `none` default.
fn default_pause_wait() -> String {
    "4h".to_owned()
}

/// The default seconds between pool supervision polls
fn default_poll_secs() -> u64 {
    30
}

/// The default seconds consecutive spawns are staggered by
fn default_stagger_secs() -> u64 {
    5
}

/// The default free-disk floor in GiB
fn default_disk_free_min_gb() -> u64 {
    15
}

/// The default path the free-disk guard measures
fn default_disk_path() -> PathBuf {
    PathBuf::from("/")
}

/// The default relaunch budget for failed units
fn default_max_retries() -> u32 {
    2
}

/// The default relaunch budget for report-only recoveries
fn default_report_retries() -> u32 {
    2
}

impl BatchConf {
    /// Load and validate the batch manifest from an evaluation directory
    ///
    /// Reads `<dir>/batch.yml`, resolves its relative paths against `dir`,
    /// and rejects inconsistent settings before any run state is created.
    ///
    /// # Arguments
    ///
    /// * `dir` - The evaluation directory holding batch.yml
    pub fn load(dir: &Path) -> Result<BatchConf, Error> {
        // read the manifest out of the evaluation directory
        let path = dir.join("batch.yml");
        let raw = std::fs::read_to_string(&path)
            .map_err(|err| Error::new(format!("failed to read {}: {err}", path.display())))?;
        // parse strictly so a typoed setting can never be silently ignored
        let mut conf: BatchConf = serde_norway::from_str(&raw)
            .map_err(|err| Error::new(format!("failed to parse {}: {err}", path.display())))?;
        // remember where the manifest lives for later relative lookups
        conf.dir = dir.to_path_buf();
        // resolve the manifest's relative paths against its directory
        conf.resolve_paths(dir);
        // reject inconsistent settings up front
        conf.validate()?;
        Ok(conf)
    }

    /// Render a session name from the template for one run and alias
    ///
    /// # Arguments
    ///
    /// * `run` - The run number to substitute for `{run}`
    /// * `alias` - The unit alias to substitute for `{alias}`
    pub fn render_name(&self, run: u32, alias: &str) -> Result<String, Error> {
        // substitute the two supported placeholders
        let name = self
            .name_template
            .replace("{run}", &run.to_string())
            .replace("{alias}", alias);
        // leftover braces mean an unknown placeholder or a stray brace
        if name.contains('{') || name.contains('}') {
            return Err(Error::new(format!(
                "name template '{}' has unknown placeholders or stray braces; \
                 only {{run}} and {{alias}} are supported",
                self.name_template
            )));
        }
        // rendered names become session directories, so enforce the store's rules
        thorpal::validate_session_name(&name).map_err(|err| {
            Error::new(format!(
                "name template '{}' renders the invalid session name '{name}': {err}",
                self.name_template
            ))
        })?;
        Ok(name)
    }

    /// Resolve the manifest's relative paths against its directory
    ///
    /// # Arguments
    ///
    /// * `dir` - The evaluation directory the manifest was loaded from
    fn resolve_paths(&mut self, dir: &Path) {
        // resolve the prompt file against the manifest directory
        if let Some(prompt_file) = self.prompt_file.take() {
            self.prompt_file = Some(resolve(dir, &prompt_file));
        }
        // resolve the report template the same way
        if let Some(report_template) = self.report_template.take() {
            self.report_template = Some(resolve(dir, &report_template));
        }
        // expand a leading ~ in the env file before resolving it
        if let Some(env_file) = self.env_file.take() {
            self.env_file = Some(resolve(dir, &expand_tilde(&env_file)));
        }
        // resolve the dataset root so staging works from any working directory
        if let Some(dataset) = self.dataset.as_mut() {
            dataset.root = resolve(dir, &dataset.root);
        }
        // resolve every path-based sample ref against the manifest directory
        for unit in &mut self.units {
            for sample in &mut unit.samples {
                if let SampleRef::Path(path) = sample {
                    *path = resolve(dir, path);
                }
            }
        }
    }

    /// Reject inconsistent or incomplete manifest settings
    fn validate(&self) -> Result<(), Error> {
        // exactly one prompt source must be configured
        match (&self.prompt, &self.prompt_file) {
            (Some(_), Some(_)) => {
                return Err(Error::new(
                    "set exactly one of prompt or prompt_file, not both".to_owned(),
                ));
            }
            (None, None) => {
                return Err(Error::new(
                    "set exactly one of prompt or prompt_file".to_owned(),
                ));
            }
            _ => (),
        }
        // the prompt file must exist so a run cannot fail after staging
        if let Some(prompt_file) = &self.prompt_file
            && !prompt_file.is_file()
        {
            return Err(Error::new(format!(
                "prompt_file {} does not exist",
                prompt_file.display()
            )));
        }
        // the report template must exist when one is set
        if let Some(report_template) = &self.report_template
            && !report_template.is_file()
        {
            return Err(Error::new(format!(
                "report_template {} does not exist",
                report_template.display()
            )));
        }
        // the template must mention the alias or every session name collides
        if !self.name_template.contains("{alias}") {
            return Err(Error::new(format!(
                "name_template '{}' must contain the {{alias}} placeholder",
                self.name_template
            )));
        }
        // a manifest with no units drives nothing
        if self.units.is_empty() {
            return Err(Error::new(
                "the manifest must list at least one unit".to_owned(),
            ));
        }
        // every unit needs a unique name and at least one sample
        let mut names = HashSet::with_capacity(self.units.len());
        for unit in &self.units {
            // unit names key the run state, so duplicates would collide
            if !names.insert(unit.name.as_str()) {
                return Err(Error::new(format!(
                    "unit name '{}' is used more than once",
                    unit.name
                )));
            }
            // a unit with no samples has nothing to analyze
            if unit.samples.is_empty() {
                return Err(Error::new(format!("unit '{}' lists no samples", unit.name)));
            }
        }
        // sha256 refs can only be staged out of a configured dataset
        let uses_sha = self.units.iter().any(|unit| {
            unit.samples
                .iter()
                .any(|sample| matches!(sample, SampleRef::Sha256(_)))
        });
        match &self.dataset {
            // sha refs with no dataset block cannot be staged
            None if uses_sha => {
                return Err(Error::new(
                    "units reference dataset sha256s but no dataset block is set".to_owned(),
                ));
            }
            // the dataset root must exist when a dataset is configured
            Some(dataset) if !dataset.root.exists() => {
                return Err(Error::new(format!(
                    "dataset root {} does not exist",
                    dataset.root.display()
                )));
            }
            _ => (),
        }
        // the concurrency bounds must nest: 1 <= floor <= start <= max
        let Concurrency { start, max, floor } = self.concurrency;
        if floor < 1 || floor > start || start > max {
            return Err(Error::new(format!(
                "concurrency bounds must satisfy 1 <= floor <= start <= max, \
                 got start={start} max={max} floor={floor}"
            )));
        }
        // a configured turn cap must be sane enough to finish any session
        if let Some(turns) = self.max_turns
            && !(10..=10_000).contains(&turns)
        {
            return Err(Error::new(format!(
                "max_turns must be between 10 and 10000, got {turns}"
            )));
        }
        // the pause-wait string must parse so children never reject it
        crate::args::ai::parse_pause_wait(&self.pause_wait).map_err(|err| {
            Error::new(format!("invalid pause_wait '{}': {err}", self.pause_wait))
        })?;
        // render a probe name to fail fast on a bad template
        self.render_name(1, "s01")?;
        Ok(())
    }
}

/// Resolve a possibly relative path against a base directory
///
/// # Arguments
///
/// * `dir` - The base directory relative paths resolve against
/// * `path` - The path to resolve
fn resolve(dir: &Path, path: &Path) -> PathBuf {
    // absolute paths stand on their own
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        dir.join(path)
    }
}

/// Expand a leading `~/` to the user's home directory
///
/// # Arguments
///
/// * `path` - The path to expand
fn expand_tilde(path: &Path) -> PathBuf {
    // only a leading tilde component expands
    if let Ok(stripped) = path.strip_prefix("~") {
        // fall back to the raw path when no home directory is known
        if let Some(home) = dirs::home_dir() {
            return home.join(stripped);
        }
    }
    path.to_path_buf()
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
            "thorctl-batch-manifest-{name}-{}-{count}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Write a batch.yml into a fresh scratch dir and load it
    ///
    /// # Arguments
    ///
    /// * `name` - A short name distinguishing the test
    /// * `yaml` - The manifest body to write
    fn load_yaml(name: &str, yaml: &str) -> (PathBuf, Result<BatchConf, Error>) {
        // write the manifest where load expects it
        let dir = scratch_dir(name);
        std::fs::write(dir.join("batch.yml"), yaml).unwrap();
        let loaded = BatchConf::load(&dir);
        (dir, loaded)
    }

    /// A minimal manifest parses with every documented default
    #[test]
    fn minimal_manifest_gets_all_defaults() {
        // one inline prompt and one path-based unit is the smallest manifest
        let yaml = "prompt: analyze it\nunits:\n  - name: u1\n    samples:\n      - bin/mal.exe\n";
        let (dir, loaded) = load_yaml("defaults", yaml);
        let conf = loaded.unwrap();
        // the explicit settings survive
        assert_eq!(conf.prompt.as_deref(), Some("analyze it"));
        assert_eq!(conf.prompt_file, None);
        assert_eq!(conf.units.len(), 1);
        assert_eq!(conf.units[0].name, "u1");
        // the path ref resolved against the manifest directory
        assert_eq!(
            conf.units[0].samples[0],
            SampleRef::Path(dir.join("bin/mal.exe"))
        );
        // every documented default applies
        assert_eq!(conf.dataset, None);
        assert!(conf.blind);
        assert_eq!(conf.name_template, "batch{run}-{alias}");
        assert!(conf.anonymize);
        assert!(conf.fresh_project);
        assert!(!conf.defang);
        assert_eq!(conf.redact_tokens, None);
        assert!(!conf.disable_detectors);
        assert_eq!(conf.report_template, None);
        assert_eq!(conf.env_file, None);
        assert_eq!(conf.pause_wait, "4h");
        assert_eq!(conf.max_turns, None);
        assert_eq!(
            conf.concurrency,
            Concurrency {
                start: 4,
                max: 8,
                floor: 3
            }
        );
        assert_eq!(conf.poll_secs, 30);
        assert_eq!(conf.stagger_secs, 5);
        assert_eq!(conf.disk_free_min_gb, 15);
        assert_eq!(conf.disk_path, PathBuf::from("/"));
        assert_eq!(conf.max_retries, 2);
        assert_eq!(conf.report_retries, 2);
        // the derived directory points back at the manifest's home
        assert_eq!(conf.dir, dir);
    }

    /// Unknown manifest fields are rejected, not silently ignored
    #[test]
    fn unknown_fields_are_rejected() {
        // a typoed setting must fail the parse
        let yaml = "prompt: p\nbogus: 1\nunits:\n  - name: u1\n    samples:\n      - a.bin\n";
        let (_dir, loaded) = load_yaml("unknown", yaml);
        let err = loaded.unwrap_err();
        assert!(err.to_string().contains("bogus"), "err was: {err}");
    }

    /// Exactly one of prompt and prompt_file must be set
    #[test]
    fn prompt_xor_prompt_file() {
        // neither prompt source is an error
        let yaml = "units:\n  - name: u1\n    samples:\n      - a.bin\n";
        let (_dir, loaded) = load_yaml("noprompt", yaml);
        assert!(loaded.unwrap_err().to_string().contains("prompt"));
        // both prompt sources at once is also an error
        let dir = scratch_dir("bothprompt");
        std::fs::write(dir.join("p.md"), "from a file").unwrap();
        let yaml = "prompt: inline\nprompt_file: p.md\nunits:\n  - name: u1\n    samples:\n      - a.bin\n";
        std::fs::write(dir.join("batch.yml"), yaml).unwrap();
        let err = BatchConf::load(&dir).unwrap_err();
        assert!(err.to_string().contains("not both"), "err was: {err}");
        // a prompt file alone works and resolves against the manifest dir
        let yaml = "prompt_file: p.md\nunits:\n  - name: u1\n    samples:\n      - a.bin\n";
        std::fs::write(dir.join("batch.yml"), yaml).unwrap();
        let conf = BatchConf::load(&dir).unwrap();
        assert_eq!(conf.prompt_file, Some(dir.join("p.md")));
        // a missing prompt file is caught at load time
        let yaml = "prompt_file: gone.md\nunits:\n  - name: u1\n    samples:\n      - a.bin\n";
        std::fs::write(dir.join("batch.yml"), yaml).unwrap();
        let err = BatchConf::load(&dir).unwrap_err();
        assert!(err.to_string().contains("gone.md"), "err was: {err}");
    }

    /// Sample refs classify as shas only when exactly 64 ascii hex chars
    #[test]
    fn sample_ref_classification() {
        // a 64 char uppercase hex string is a sha and normalizes to lowercase
        let upper = "ABCDEF0123456789".repeat(4);
        assert_eq!(upper.len(), 64);
        let sample: SampleRef = serde_norway::from_str(&upper).unwrap();
        assert_eq!(sample, SampleRef::Sha256(upper.to_ascii_lowercase()));
        // 63 hex chars is a path, not a sha
        let hex63 = &upper[..63];
        let sample: SampleRef = serde_norway::from_str(hex63).unwrap();
        assert_eq!(sample, SampleRef::Path(PathBuf::from(hex63)));
        // 64 chars with one non-hex char is a path
        let nonhex = format!("{}g", &upper[..63]);
        let sample: SampleRef = serde_norway::from_str(&nonhex).unwrap();
        assert_eq!(sample, SampleRef::Path(PathBuf::from(&nonhex)));
        // a sha serializes back to its plain lowercase string
        let out = serde_norway::to_string(&SampleRef::Sha256(upper.to_ascii_lowercase())).unwrap();
        assert_eq!(out.trim(), upper.to_ascii_lowercase());
    }

    /// Bad name templates are rejected at load time
    #[test]
    fn name_template_rules() {
        // a template without the alias placeholder is rejected
        let yaml = "prompt: p\nname_template: \"batch{run}\"\nunits:\n  - name: u1\n    samples:\n      - a.bin\n";
        let (_dir, loaded) = load_yaml("noalias", yaml);
        assert!(loaded.unwrap_err().to_string().contains("{alias}"));
        // an unknown placeholder leaves braces behind and is rejected
        let yaml = "prompt: p\nname_template: \"batch{run}-{alias}-{what}\"\nunits:\n  - name: u1\n    samples:\n      - a.bin\n";
        let (_dir, loaded) = load_yaml("unknownph", yaml);
        assert!(loaded.unwrap_err().to_string().contains("placeholders"));
        // a template that renders an invalid session name is rejected
        let yaml = "prompt: p\nname_template: \"batch {alias}\"\nunits:\n  - name: u1\n    samples:\n      - a.bin\n";
        let (_dir, loaded) = load_yaml("badname", yaml);
        assert!(loaded.unwrap_err().to_string().contains("session name"));
        // a good template renders both placeholders
        let yaml = "prompt: p\nunits:\n  - name: u1\n    samples:\n      - a.bin\n";
        let (_dir, loaded) = load_yaml("render", yaml);
        let conf = loaded.unwrap();
        assert_eq!(conf.render_name(3, "s07").unwrap(), "batch3-s07");
    }

    /// A dataset block is required exactly when a unit references a sha
    #[test]
    fn dataset_required_iff_sha_refs() {
        // a sha ref with no dataset block cannot be staged
        let sha = "a".repeat(64);
        let yaml = format!("prompt: p\nunits:\n  - name: u1\n    samples:\n      - {sha}\n");
        let (_dir, loaded) = load_yaml("shanodataset", &yaml);
        assert!(loaded.unwrap_err().to_string().contains("dataset"));
        // the same manifest with an existing dataset root loads
        let dir = scratch_dir("shadataset");
        std::fs::create_dir_all(dir.join("zips")).unwrap();
        let yaml = format!(
            "prompt: p\ndataset:\n  root: zips\nunits:\n  - name: u1\n    samples:\n      - {sha}\n"
        );
        std::fs::write(dir.join("batch.yml"), yaml).unwrap();
        let conf = BatchConf::load(&dir).unwrap();
        // the dataset root resolved against the manifest dir and defaulted its password
        let dataset = conf.dataset.unwrap();
        assert_eq!(dataset.root, dir.join("zips"));
        assert_eq!(dataset.password, "infected");
        // a dataset pointing at a missing root is caught at load time
        let yaml = format!(
            "prompt: p\ndataset:\n  root: missing\nunits:\n  - name: u1\n    samples:\n      - {sha}\n"
        );
        std::fs::write(dir.join("batch.yml"), yaml).unwrap();
        let err = BatchConf::load(&dir).unwrap_err();
        assert!(err.to_string().contains("does not exist"), "err was: {err}");
    }

    /// The concurrency bounds must nest as 1 <= floor <= start <= max
    #[test]
    fn concurrency_bounds() {
        // a zero floor is rejected
        let yaml = "prompt: p\nconcurrency:\n  floor: 0\nunits:\n  - name: u1\n    samples:\n      - a.bin\n";
        let (_dir, loaded) = load_yaml("floorzero", yaml);
        assert!(loaded.unwrap_err().to_string().contains("floor"));
        // a floor above the start is rejected
        let yaml = "prompt: p\nconcurrency:\n  start: 2\n  floor: 3\nunits:\n  - name: u1\n    samples:\n      - a.bin\n";
        let (_dir, loaded) = load_yaml("floorhigh", yaml);
        assert!(loaded.unwrap_err().to_string().contains("floor"));
        // a start above the max is rejected
        let yaml = "prompt: p\nconcurrency:\n  start: 9\nunits:\n  - name: u1\n    samples:\n      - a.bin\n";
        let (_dir, loaded) = load_yaml("starthigh", yaml);
        assert!(loaded.unwrap_err().to_string().contains("start"));
        // valid custom bounds load
        let yaml = "prompt: p\nconcurrency:\n  start: 5\n  max: 10\n  floor: 2\nunits:\n  - name: u1\n    samples:\n      - a.bin\n";
        let (_dir, loaded) = load_yaml("goodbounds", yaml);
        let conf = loaded.unwrap();
        assert_eq!(
            conf.concurrency,
            Concurrency {
                start: 5,
                max: 10,
                floor: 2
            }
        );
    }

    /// Duplicate unit names and sample-less units are rejected
    #[test]
    fn unit_rules() {
        // two units with one name would collide in the run state
        let yaml = "prompt: p\nunits:\n  - name: u1\n    samples:\n      - a.bin\n  - name: u1\n    samples:\n      - b.bin\n";
        let (_dir, loaded) = load_yaml("dupunits", yaml);
        assert!(loaded.unwrap_err().to_string().contains("more than once"));
        // a unit with no samples has nothing to analyze
        let yaml = "prompt: p\nunits:\n  - name: u1\n    samples: []\n";
        let (_dir, loaded) = load_yaml("nosamples", yaml);
        assert!(loaded.unwrap_err().to_string().contains("no samples"));
        // an empty unit list drives nothing
        let yaml = "prompt: p\nunits: []\n";
        let (_dir, loaded) = load_yaml("nounits", yaml);
        assert!(
            loaded
                .unwrap_err()
                .to_string()
                .contains("at least one unit")
        );
    }

    /// The pause-wait passthrough must parse like the child flag does
    #[test]
    fn pause_wait_is_validated() {
        // an unparseable pause wait is rejected
        let yaml =
            "prompt: p\npause_wait: sometimes\nunits:\n  - name: u1\n    samples:\n      - a.bin\n";
        let (_dir, loaded) = load_yaml("badpause", yaml);
        assert!(loaded.unwrap_err().to_string().contains("pause_wait"));
        // a duration keeps its raw spelling for child passthrough
        let yaml =
            "prompt: p\npause_wait: 15m\nunits:\n  - name: u1\n    samples:\n      - a.bin\n";
        let (_dir, loaded) = load_yaml("goodpause", yaml);
        assert_eq!(loaded.unwrap().pause_wait, "15m");
    }
}
