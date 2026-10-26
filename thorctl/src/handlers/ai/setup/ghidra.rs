//! Ghidra detection and the download/extract pipeline for the setup wizard
//!
//! Detection and validation are pure and unit-tested here; the actual release
//! resolution, download, and extraction reuse ghidra-cli's library-exported
//! installer core (`ghidra_cli::ghidra::setup`), with thorctl supplying strict
//! TLS (never `danger_accept_invalid_certs`, unlike the standalone `ghidra
//! setup` CLI) and progress surfaced as wizard events instead of stdout bars.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Context;
use ghidra_cli::ghidra::setup as installer;
use kanal::AsyncSender;
use tracing::instrument;

use super::app::SetupEvent;

/// How many bytes of download progress may pass between posted events
const DOWNLOAD_EVENT_BYTES: u64 = 1024 * 1024;

/// How much time may pass between posted download progress events
const DOWNLOAD_EVENT_INTERVAL: Duration = Duration::from_millis(250);

/// How many extracted archive entries may pass between posted events
const EXTRACT_EVENT_ENTRIES: usize = 64;

/// Whether a directory is a usable Ghidra install
///
/// A usable install is one carrying `support/analyzeHeadless`, the exact probe
/// ThorPal's Ghidra bridge uses before spawning it.
///
/// # Arguments
///
/// * `path` - The directory to validate
pub fn validate_install(path: &Path) -> bool {
    ghidra_cli::ghidra::bridge::find_headless_script(path).is_ok()
}

/// The default root downloaded Ghidra releases are installed under
///
/// Lives under the local data dir (e.g. `~/.local/share/thorium/ghidra`) so a
/// ~1.2 GB install never lands in `~/.thorium` next to configs and sessions;
/// only Ghidra *project* dirs carry the no-dot-components rule, installs are
/// fine anywhere.
pub fn default_install_root() -> Option<PathBuf> {
    dirs::data_local_dir().map(|dir| dir.join("thorium").join("ghidra"))
}

/// The candidate locations scanned for existing Ghidra installs
///
/// Ordered by how authoritative they are: the configured install and the env
/// override first, then the wizard's own install root and ghidra-cli's, then
/// the conventional system and home locations.
///
/// # Arguments
///
/// * `configured` - The `ai.ghidra_install` from the loaded config, if any
/// * `install_root` - The wizard's resolved install root (`--ghidra-dir` or
///   the default)
pub fn candidate_roots(configured: Option<&Path>, install_root: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    // the configured install is the most authoritative candidate
    if let Some(install) = configured {
        roots.push(install.to_path_buf());
    }
    // then the same env override the chat/summary flags honor
    if let Ok(env_install) = std::env::var("GHIDRA_INSTALL")
        && !env_install.is_empty()
    {
        roots.push(PathBuf::from(env_install));
    }
    // then the wizard's own install root and ghidra-cli's standalone one
    roots.push(install_root.to_path_buf());
    if let Some(data) = dirs::data_local_dir() {
        roots.push(data.join("ghidra-cli").join("ghidra"));
    }
    // and finally the conventional system and home locations
    roots.push(PathBuf::from("/opt"));
    roots.push(PathBuf::from("/usr/local"));
    if let Some(home) = dirs::home_dir() {
        roots.push(home);
    }
    roots
}

/// Find valid Ghidra installs under the candidate roots
///
/// Each root is checked both as an install itself (configured/env candidates
/// point straight at one) and as a container of `ghidra*` children (install
/// roots and system dirs hold versioned dirs like `ghidra_12.1.4_PUBLIC`).
/// Results are canonicalized and deduped so the same install reached through
/// two roots lists once.
///
/// # Arguments
///
/// * `roots` - The candidate locations to scan
pub fn detect_installs(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for root in roots {
        // the root itself may be an install
        push_valid(root, &mut found);
        // and any ghidra* children under a container root are candidates
        if let Ok(entries) = std::fs::read_dir(root) {
            // collect and sort newest-version-first (a natural sort, so
            // ghidra_12.x really outranks ghidra_9.x) with a stable order
            let mut children: Vec<PathBuf> = entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.to_ascii_lowercase().starts_with("ghidra"))
                })
                .collect();
            children.sort_by(|a, b| natural_key(b).cmp(&natural_key(a)));
            for child in &children {
                push_valid(child, &mut found);
            }
        }
    }
    found
}

/// A file name's natural sort key: digit runs compare numerically
///
/// Splits a name like `ghidra_12.1.4_PUBLIC` into alternating text and
/// number chunks so version components compare by value, not byte order
/// (plain lexicographic order would rank `ghidra_9.x` above `ghidra_12.x`).
///
/// # Arguments
///
/// * `path` - The path whose file name to key
fn natural_key(path: &Path) -> Vec<(u64, String)> {
    // key off the file name alone; the parent dirs are the same per container
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let mut key = Vec::new();
    let mut chars = name.chars().peekable();
    // walk the name chunk by chunk, folding digit runs into numbers
    while let Some(&next) = chars.peek() {
        if next.is_ascii_digit() {
            // a digit run compares numerically (saturating on absurd runs)
            let mut value: u64 = 0;
            while let Some(&digit) = chars.peek() {
                let Some(digit) = digit.to_digit(10) else {
                    break;
                };
                value = value.saturating_mul(10).saturating_add(u64::from(digit));
                chars.next();
            }
            key.push((value, String::new()));
        } else {
            // a text run compares lexicographically, ranked below any number
            let mut text = String::new();
            while let Some(&ch) = chars.peek() {
                if ch.is_ascii_digit() {
                    break;
                }
                text.push(ch);
                chars.next();
            }
            key.push((0, text));
        }
    }
    key
}

/// Push a path onto the found list when it validates and is not a duplicate
///
/// # Arguments
///
/// * `path` - The candidate install to validate
/// * `found` - The deduped list of valid installs found so far
fn push_valid(path: &Path, found: &mut Vec<PathBuf>) {
    // only valid installs make the list at all
    if !validate_install(path) {
        return;
    }
    // canonicalize so the same install via two roots dedupes; a canonicalize
    // failure (racing deletion) just falls back to the raw path
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if !found.contains(&canonical) {
        found.push(canonical);
    }
}

/// Where the Ghidra zip an install run extracts comes from
#[derive(Debug, Clone)]
pub enum InstallSource {
    /// Download a release from GitHub (the latest, or a pinned version)
    Download {
        /// The release version to pin (e.g. "12.1.4"), or `None` for latest
        version: Option<String>,
    },
    /// Extract a release zip already on disk (offline boxes)
    LocalZip(PathBuf),
}

/// Run the whole Ghidra install pipeline, posting progress as wizard events
///
/// Spawned by the app; however the pipeline ends, exactly one terminal event
/// (`ExtractDone` or `GhidraFailed`) carrying the task's generation lands on
/// the channel, so a cancelled task's late results drop at the loop.
///
/// # Arguments
///
/// * `root` - The install root releases land under
/// * `source` - Where the release zip comes from
/// * `generation` - The task generation guarding against stale results
/// * `events` - The wizard's event channel
pub async fn run_install(
    root: PathBuf,
    source: InstallSource,
    generation: u64,
    events: AsyncSender<SetupEvent>,
) {
    // run the pipeline and surface however it ends as one terminal event
    match install_pipeline(&root, source, generation, &events).await {
        Ok(install) => {
            let _ = events
                .send(SetupEvent::ExtractDone {
                    generation,
                    install,
                })
                .await;
        }
        Err(error) => {
            let _ = events
                .send(SetupEvent::GhidraFailed {
                    generation,
                    error: format!("{error:#}"),
                })
                .await;
        }
    }
}

/// Resolve, download, extract, and validate one Ghidra install
///
/// # Arguments
///
/// * `root` - The install root releases land under
/// * `source` - Where the release zip comes from
/// * `generation` - The task generation stamped onto progress events
/// * `events` - The wizard's event channel
#[instrument(name = "setup::ghidra::install_pipeline", skip_all, err(Debug))]
async fn install_pipeline(
    root: &Path,
    source: InstallSource,
    generation: u64,
    events: &AsyncSender<SetupEvent>,
) -> Result<PathBuf, anyhow::Error> {
    // make sure the install root exists before anything lands in it
    tokio::fs::create_dir_all(root)
        .await
        .with_context(|| format!("could not create the install root '{}'", root.display()))?;
    // get the zip to extract, downloading it unless one was handed in
    let (zip_path, downloaded) = match source {
        InstallSource::LocalZip(zip) => {
            // an offline install extracts the user's own zip and keeps it
            anyhow::ensure!(
                zip.is_file(),
                "the Ghidra zip '{}' does not exist or is not a file",
                zip.display()
            );
            (zip, false)
        }
        InstallSource::Download { version } => {
            // resolve the release with strict TLS; never accept invalid certs
            let release = installer::resolve_release(version.as_deref(), false)
                .await
                .context(
                    "could not resolve the Ghidra release from api.github.com: check \
                    connectivity (or set GITHUB_TOKEN if rate limited), pass \
                    --ghidra-zip, or install Ghidra manually and enter its path",
                )?;
            let _ = events
                .send(SetupEvent::ReleaseResolved {
                    generation,
                    tag: release.tag.clone(),
                    name: release.name.clone(),
                    size: release.size,
                })
                .await;
            // short-circuit onto an already-installed copy of this release
            if let Some(existing) = installed_release(root, &release.tag) {
                return Ok(existing);
            }
            // stream the asset into a hidden partial file with byte progress
            let part = root.join(format!(".{}.part", release.name));
            download_with_events(&release.url, &part, generation, events).await?;
            // a short read means a truncated download, not a usable zip
            let written = std::fs::metadata(&part)
                .with_context(|| format!("could not stat the download '{}'", part.display()))?
                .len();
            anyhow::ensure!(
                release.size == 0 || written == release.size,
                "the download is truncated ({written} of {} bytes): retry the install",
                release.size
            );
            (part, true)
        }
    };
    // sweep temp dirs stale runs left behind, best-effort and off the loop: a
    // cancelled attempt's extractor cannot be aborted mid-file and may still
    // be writing into its own dir, so a failure to clear one never matters
    let sweep_root = root.to_path_buf();
    let _ = tokio::task::spawn_blocking(move || sweep_extract_tmp(&sweep_root)).await;
    // extract into a per-attempt temp dir so a torn or still-cancelling
    // extract can never collide with this one, and so a torn extract never
    // looks like a finished install
    let tmp = root.join(format!(".extract-tmp-{generation}"));
    tokio::fs::create_dir_all(&tmp).await?;
    let extracted = extract_with_events(zip_path.clone(), tmp.clone(), generation, events).await?;
    // the single root must be a directory to be an install at all
    anyhow::ensure!(
        extracted.is_dir(),
        "the archive's top-level entry '{}' is not a directory",
        extracted.display()
    );
    // move the extracted root into place, tolerating an existing valid copy
    let name = extracted
        .file_name()
        .context("the archive had no root directory to install")?
        .to_owned();
    let target = root.join(&name);
    if target.exists() {
        // an existing valid copy of this release wins; a broken one is not
        // silently clobbered, the user removes it deliberately
        anyhow::ensure!(
            validate_install(&target),
            "'{}' already exists but is not a valid Ghidra install (no \
            support/analyzeHeadless): remove it and retry",
            target.display()
        );
    } else {
        tokio::fs::rename(&extracted, &target)
            .await
            .with_context(|| format!("could not move the install into '{}'", target.display()))?;
    }
    // the finished install must actually carry analyzeHeadless
    anyhow::ensure!(
        validate_install(&target),
        "the extracted archive at '{}' is missing support/analyzeHeadless",
        target.display()
    );
    // best-effort cleanup: the temp tree always, the zip only when downloaded
    let _ = tokio::fs::remove_dir_all(&tmp).await;
    if downloaded {
        let _ = tokio::fs::remove_file(&zip_path).await;
    }
    Ok(target)
}

/// Sweep the stale per-attempt extract temp dirs under an install root
///
/// Best-effort by design: a cancelled attempt's extractor cannot be aborted
/// mid-file and may still be writing into its own (uniquely named) dir, so a
/// dir that will not delete right now simply waits for the next sweep.
///
/// # Arguments
///
/// * `root` - The install root whose temp dirs to sweep
fn sweep_extract_tmp(root: &Path) {
    // walk the root's children and clear anything named like an extract temp
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with(".extract-tmp") {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// An already-installed copy of a resolved release under the install root
///
/// Release tags look like `Ghidra_12.1.4_build` and install as
/// `ghidra_12.1.4_PUBLIC`, so a tag maps straight to the directory a prior
/// run would have left behind.
///
/// # Arguments
///
/// * `root` - The install root releases land under
/// * `tag` - The resolved release's git tag
fn installed_release(root: &Path, tag: &str) -> Option<PathBuf> {
    // pull the version out of the tag's `Ghidra_<ver>_<kind>` shape
    let version = tag.strip_prefix("Ghidra_")?.split('_').next()?;
    // the release installs under its conventional PUBLIC directory name
    let expected = root.join(format!("ghidra_{version}_PUBLIC"));
    validate_install(&expected).then_some(expected)
}

/// Stream a download to disk, posting throttled byte-progress events
///
/// # Arguments
///
/// * `url` - The asset url to download
/// * `path` - The partial file the bytes stream into
/// * `generation` - The task generation stamped onto progress events
/// * `events` - The wizard's event channel
async fn download_with_events(
    url: &str,
    path: &Path,
    generation: u64,
    events: &AsyncSender<SetupEvent>,
) -> Result<(), anyhow::Error> {
    // the download callback is sync, so progress posts over the sync handle
    let progress_tx = events.clone_sync();
    // throttle progress to a sane event rate: a 400 MB zip arrives in tens of
    // thousands of chunks and every event costs a wakeup and a draw
    let mut last_bytes = 0u64;
    let mut last_sent = Instant::now();
    installer::download_file_with(url, path, false, move |bytes, total| {
        // always post the first and final readings, throttle the middle
        let done = total.is_some_and(|total| bytes >= total);
        if bytes == 0
            || done
            || bytes.saturating_sub(last_bytes) >= DOWNLOAD_EVENT_BYTES
            || last_sent.elapsed() >= DOWNLOAD_EVENT_INTERVAL
        {
            last_bytes = bytes;
            last_sent = Instant::now();
            let _ = progress_tx.send(SetupEvent::DownloadProgress {
                generation,
                bytes,
                total,
            });
        }
    })
    .await
    .with_context(|| format!("downloading '{url}' failed"))
}

/// Extract a release zip on the blocking pool, posting entry-progress events
///
/// Returns the extracted root directory inside the target dir. The extraction
/// itself cannot be aborted mid-file; a cancelled task's leftover temp dir is
/// cleared by the next install attempt.
///
/// # Arguments
///
/// * `zip` - The release zip to extract
/// * `target` - The temp dir the archive extracts into
/// * `generation` - The task generation stamped onto progress events
/// * `events` - The wizard's event channel
async fn extract_with_events(
    zip: PathBuf,
    target: PathBuf,
    generation: u64,
    events: &AsyncSender<SetupEvent>,
) -> Result<PathBuf, anyhow::Error> {
    // the zip walk is blocking io, so it runs on the blocking pool and posts
    // progress over the channel's sync handle
    let progress_tx = events.clone_sync();
    tokio::task::spawn_blocking(move || {
        installer::extract_zip_with(&zip, &target, |done, total| {
            // post every Nth entry plus the final one; losing one is harmless
            if done % EXTRACT_EVENT_ENTRIES == 0 || done == total {
                let _ = progress_tx.send(SetupEvent::ExtractProgress {
                    generation,
                    done,
                    total,
                });
            }
        })
    })
    .await
    .context("the extract task panicked")?
    .with_context(|| "extracting the Ghidra zip failed")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lay a fake Ghidra install down under a root, returning its path
    ///
    /// # Arguments
    ///
    /// * `root` - The directory to create the install under
    /// * `name` - The install directory's name
    fn fake_install(root: &Path, name: &str) -> PathBuf {
        // a valid install is anything carrying support/analyzeHeadless
        let install = root.join(name);
        let support = install.join("support");
        std::fs::create_dir_all(&support).unwrap();
        let script = if cfg!(windows) {
            "analyzeHeadless.bat"
        } else {
            "analyzeHeadless"
        };
        std::fs::write(support.join(script), "#!/bin/sh\n").unwrap();
        install
    }

    /// Only directories carrying support/analyzeHeadless validate
    #[test]
    fn validate_install_requires_analyze_headless() {
        let dir = tempfile::tempdir().unwrap();
        // a bare directory is not an install
        assert!(!validate_install(dir.path()));
        // one carrying the headless script is
        let install = fake_install(dir.path(), "ghidra_12.1.4_PUBLIC");
        assert!(validate_install(&install));
    }

    /// Detection finds roots that are installs, children, and dedupes both
    #[test]
    fn detect_installs_scans_roots_and_children_deduped() {
        let dir = tempfile::tempdir().unwrap();
        // one install sits directly at a candidate root (a configured path)
        let direct = fake_install(dir.path(), "direct");
        // another sits as a ghidra_* child of a container root
        let container = dir.path().join("container");
        std::fs::create_dir_all(&container).unwrap();
        let child = fake_install(&container, "ghidra_12.1.4_PUBLIC");
        // a non-ghidra child never validates its way in
        fake_install(&container, "radare2");
        // scan with the direct install listed twice to prove deduping
        let roots = vec![direct.clone(), container.clone(), direct.clone()];
        let found = detect_installs(&roots);
        let canonical_direct = std::fs::canonicalize(&direct).unwrap();
        let canonical_child = std::fs::canonicalize(&child).unwrap();
        assert_eq!(found, vec![canonical_direct, canonical_child]);
    }

    /// Version ordering is natural, so 12.x outranks 9.x despite '9' > '1'
    #[test]
    fn detect_installs_orders_versions_naturally() {
        let dir = tempfile::tempdir().unwrap();
        // three versioned installs under one container root
        let container = dir.path().join("container");
        std::fs::create_dir_all(&container).unwrap();
        let old = fake_install(&container, "ghidra_9.2.4_PUBLIC");
        let mid = fake_install(&container, "ghidra_11.3.2_PUBLIC");
        let new = fake_install(&container, "ghidra_12.1.4_PUBLIC");
        // detection leads with the newest version, not the biggest byte
        let found = detect_installs(&[container]);
        let expect: Vec<PathBuf> = [&new, &mid, &old]
            .iter()
            .map(|path| std::fs::canonicalize(path).unwrap())
            .collect();
        assert_eq!(found, expect);
    }

    /// A resolved tag maps onto an already-installed PUBLIC directory
    #[test]
    fn installed_release_short_circuits_on_the_tagged_dir() {
        let dir = tempfile::tempdir().unwrap();
        // nothing installed yet: no short-circuit
        assert!(installed_release(dir.path(), "Ghidra_12.1.4_build").is_none());
        // once the conventional directory exists and validates, it wins
        let install = fake_install(dir.path(), "ghidra_12.1.4_PUBLIC");
        assert_eq!(
            installed_release(dir.path(), "Ghidra_12.1.4_build"),
            Some(install)
        );
        // a malformed tag never matches anything
        assert!(installed_release(dir.path(), "not-a-ghidra-tag").is_none());
    }
}
