//! Staging of anonymized, content-named sample copies for blind analysis runs.
//!
//! A sample's operator-assigned name and path can leak context an analysis
//! should not start from (a family-named directory, a suggestive file name).
//! [`stage_anonymized_sample`] copies the sample to a neutral, SHA-256-named
//! path under the Ghidra projects root so every model-visible identifier
//! downstream -- the file name, the Ghidra project and program names, and any
//! path a tool echoes into an error -- derives from the file's own content
//! instead of what the operator called it.

use std::path::{Path, PathBuf};

use tracing::instrument;

use super::tools::file::file_identity;

/// The subdirectory of a sample's per-binary projects dir its staged copy lives in.
const SAMPLE_DIR: &str = "sample";

/// Stage an anonymized copy of a sample under the Ghidra projects root
///
/// Copies `binary` to `<projects_root>/<sha256>/sample/<sha256>` so every
/// model-visible identifier downstream (the file name, the Ghidra project and
/// program names, the projects path, and tool error strings) derives from the
/// file's own SHA-256 instead of the operator's naming. A real copy is made --
/// never a symlink, which Ghidra could canonicalize back to the original path
/// -- written to a `.tmp` sibling and renamed into place so a crash can never
/// leave a torn copy behind. An existing staged copy is re-hashed and reused
/// when it still matches, so re-running the same sample is idempotent and two
/// concurrent runs cannot tear each other's copy. The copy is deliberately
/// left in place after the run: a resumed session re-reads it, and
/// `rm -rf <projects_root>/<sha256>/` cleans a sample's copy and its Ghidra
/// projects together. A multi-sample session stages one
/// `<projects_root>/<shaN>/sample/<shaN>` per sample while the Ghidra project
/// itself lives under the PRIMARY sample's sha -- cleaning such a session up
/// means removing each sample's `<projects_root>/<shaN>/` directory, not just
/// the primary's.
///
/// # Arguments
///
/// * `binary` - The original sample to stage an anonymized copy of
/// * `projects_root` - The already-validated Ghidra projects root
#[instrument(name = "anonymize::stage_anonymized_sample", skip_all, err(Debug))]
pub async fn stage_anonymized_sample(
    binary: &Path,
    projects_root: &Path,
) -> Result<PathBuf, anyhow::Error> {
    // hash the original so the staged copy is named by its own content
    let info = file_identity(binary).await.map_err(anyhow::Error::msg)?;
    // the copy lives beside the Ghidra projects its stem will name
    let dir = projects_root.join(&info.sha256).join(SAMPLE_DIR);
    tokio::fs::create_dir_all(&dir).await?;
    // reuse an intact staged copy so repeat runs never rewrite it
    let staged = dir.join(&info.sha256);
    if tokio::fs::metadata(&staged).await.is_ok() {
        // re-hash the existing copy to catch a torn or stale file before
        // trusting it; any mismatch falls through to a fresh copy
        if let Ok(existing) = file_identity(&staged).await {
            if existing.sha256 == info.sha256 {
                return Ok(staged);
            }
        }
    }
    // copy to a per-process temp sibling first so a crash can never leave a
    // torn copy and two concurrent runs can never share a temp file
    let tmp = dir.join(format!("{}.{}.tmp", info.sha256, std::process::id()));
    tokio::fs::copy(binary, &tmp).await?;
    // re-hash the copy so a source file mutating mid-staging can never leave
    // a staged file whose content disagrees with its sha-derived name
    let copied = file_identity(&tmp).await.map_err(anyhow::Error::msg)?;
    if copied.sha256 != info.sha256 {
        // drop the mismatched copy before erroring so nothing torn lingers
        let _ = tokio::fs::remove_file(&tmp).await;
        anyhow::bail!(
            "the sample at '{}' changed while its anonymized copy was being staged",
            binary.display(),
        );
    }
    // atomically move the finished copy over the staged name; a concurrent
    // run racing us renames an identical verified copy, so either order wins
    tokio::fs::rename(&tmp, &staged).await?;
    Ok(staged)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes a fake sample under a deliberately suggestive directory name
    ///
    /// # Arguments
    ///
    /// * `root` - The scratch directory to write the sample under
    fn write_sample(root: &Path) -> PathBuf {
        // bury the sample under a family-named directory like a real corpus
        let family = root.join("samples").join("cornfamily");
        std::fs::create_dir_all(&family).expect("sample dir should create");
        let original = family.join("evil.exe");
        std::fs::write(&original, b"CORN SAMPLE BYTES").expect("sample should write");
        original
    }

    /// A sample stages to a sha-named copy under the projects root
    #[tokio::test]
    async fn stages_a_sha_named_copy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let original = write_sample(dir.path());
        let projects = dir.path().join("projects");
        // stage the anonymized copy
        let staged = stage_anonymized_sample(&original, &projects)
            .await
            .expect("staging should succeed");
        // the copy is named by the sample's own sha256 under the projects root
        let sha = file_identity(&original).await.expect("identity").sha256;
        assert_eq!(staged, projects.join(&sha).join(SAMPLE_DIR).join(&sha));
        // the staged bytes match the original exactly
        assert_eq!(
            std::fs::read(&staged).expect("staged copy should read"),
            b"CORN SAMPLE BYTES"
        );
        // nothing about the staged path names the original file or its dirs
        let rendered = staged.display().to_string();
        assert!(!rendered.contains("cornfamily") && !rendered.contains("evil"));
    }

    /// Restaging the same sample reuses the existing copy untouched
    #[cfg(unix)]
    #[tokio::test]
    async fn restaging_is_idempotent() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let original = write_sample(dir.path());
        let projects = dir.path().join("projects");
        // stage the copy twice over the same projects root
        let first = stage_anonymized_sample(&original, &projects)
            .await
            .expect("first staging should succeed");
        let inode = std::fs::metadata(&first).expect("metadata").ino();
        let second = stage_anonymized_sample(&original, &projects)
            .await
            .expect("second staging should succeed");
        // the same path comes back and the file was reused, not rewritten
        assert_eq!(first, second);
        assert_eq!(std::fs::metadata(&second).expect("metadata").ino(), inode);
    }

    /// A corrupted staged copy is detected by the re-hash and replaced
    #[tokio::test]
    async fn corrupted_copy_is_replaced() {
        let dir = tempfile::tempdir().expect("tempdir");
        let original = write_sample(dir.path());
        let projects = dir.path().join("projects");
        // plant a wrong-content file where the staged copy will live
        let sha = file_identity(&original).await.expect("identity").sha256;
        let planted = projects.join(&sha).join(SAMPLE_DIR);
        std::fs::create_dir_all(&planted).expect("staging dir should create");
        std::fs::write(planted.join(&sha), b"TORN").expect("plant should write");
        // staging must notice the mismatch and rewrite the copy
        let staged = stage_anonymized_sample(&original, &projects)
            .await
            .expect("staging should succeed");
        assert_eq!(
            std::fs::read(&staged).expect("staged copy should read"),
            b"CORN SAMPLE BYTES"
        );
    }
}
