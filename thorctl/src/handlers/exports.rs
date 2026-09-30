//! Shared helpers for commands that export resources to disk
//!
//! Exports (`toolbox export`, `images export`, `pipelines export`) all write
//! config files that may already exist on disk from a previous run or a manual
//! edit. [`DiskConflictResolver`] gives them one consistent way to handle that:
//! a file whose on-disk content matches what we'd write is a silent no-op, and a
//! file that *differs* is either overwritten (`--overwrite`), resolved by prompting
//! the user (Merge / Overwrite / Skip / Overwrite-all / Skip-all / Quit), or —
//! when we can't prompt and `--overwrite` wasn't given — skipped with a warning so
//! nothing is silently clobbered.
//!
//! "Merge" opens the file's on-disk and freshly-exported versions in the user's
//! editor as a git-style conflict view (see [`crate::handlers::imports::editor`]),
//! validating the result on save against the file's format.
//!
//! The resolver must be driven from a single sequential task (it prompts and
//! remembers "all" choices), so concurrent exporters run it in a pre-flight pass
//! rather than from their worker pool.

use colored::Colorize;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::borrow::Cow;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use thorium::Error;

use crate::handlers::imports::editor::{self, EditorParseError};
use crate::handlers::progress::Bar;

/// What happened when a file was offered to one of [`DiskConflictResolver`]'s
/// `write_text`/`write_yaml`/`write_toml` methods
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    /// The file was written (new, or an approved overwrite)
    Written,
    /// The file was left as-is (identical content, or a skipped conflict)
    Skipped,
    /// The user chose to stop the whole export
    Quit,
}

/// How a conflict on an existing, differing file resolves before prompting
enum Resolution {
    /// Overwrite without asking (`--overwrite` or a remembered "overwrite all")
    Overwrite,
    /// Skip without asking (a remembered "skip all", or non-interactive)
    Skip,
    /// Ask the user
    Prompt,
}

/// Resolves on-disk write conflicts for an export, remembering "all" choices for
/// the rest of the run
pub struct DiskConflictResolver {
    /// Overwrite differing files without asking
    overwrite: bool,
    /// Whether the session can prompt (a tty and not run non-interactively)
    can_prompt: bool,
    /// The user chose "overwrite all" earlier this run
    overwrite_all: bool,
    /// The user chose "skip all" earlier this run
    skip_all: bool,
    /// The editor command used to resolve a conflict via the "Merge" choice
    editor: String,
    /// The user explicitly asked to skip conflicts (`--skip-conflicts`), so an
    /// auto-skip warning shouldn't suggest a different flag
    explicit_skip: bool,
}

impl DiskConflictResolver {
    /// Create a resolver
    ///
    /// # Arguments
    ///
    /// * `overwrite` - Overwrite differing files without prompting
    /// * `can_prompt` - Whether the session can ask the user (tty, interactive)
    /// * `editor` - The editor command used when the user chooses "Merge"
    pub fn new(overwrite: bool, can_prompt: bool, editor: String) -> Self {
        Self {
            overwrite,
            can_prompt,
            overwrite_all: false,
            skip_all: false,
            editor,
            explicit_skip: false,
        }
    }

    /// Mark that conflicts are skipped because the user passed `--skip-conflicts`,
    /// which changes the wording of the auto-skip warning
    ///
    /// # Arguments
    ///
    /// * `explicit` - Whether `--skip-conflicts` was given
    #[must_use]
    pub fn explicit_skip(mut self, explicit: bool) -> Self {
        self.explicit_skip = explicit;
        self
    }

    /// Stop prompting for the rest of the run, leaving every later differing file
    /// untouched as if the user had chosen "Skip all"
    ///
    /// Used after the user picks Quit when an export still has to finish writing
    /// files that resources already on disk depend on.
    pub fn stop_prompting(&mut self) {
        self.skip_all = true;
    }

    /// Write `content` to `path` with no save-time validation
    ///
    /// For files without a parseable schema (e.g. `description.md`); a "Merge"
    /// only checks for unresolved conflict markers.
    ///
    /// # Arguments
    ///
    /// * `path` - The file to write
    /// * `content` - The content we want on disk
    /// * `progress` - The progress bar (suspended while prompting)
    pub async fn write_text(
        &mut self,
        path: &Path,
        content: &str,
        progress: &Bar,
    ) -> Result<WriteOutcome, Error> {
        // markdown/plain text has no schema, so a merge only needs the marker check
        self.write_inner(path, content, progress, |_| {
            Ok::<(), serde_norway::Error>(())
        })
        .await
    }

    /// Write `content` to `path`, validating a "Merge" result as a config that
    /// deserializes into `T`
    ///
    /// The validating parser is picked from `path`'s extension so it matches the one
    /// import later reads the file with: `.json` files (resource and network-policy
    /// configs) are validated as strict JSON, anything else as YAML.
    ///
    /// # Arguments
    ///
    /// * `path` - The file to write
    /// * `content` - The content we want on disk
    /// * `progress` - The progress bar (suspended while prompting)
    pub async fn write_yaml<T: DeserializeOwned>(
        &mut self,
        path: &Path,
        content: &str,
        progress: &Bar,
    ) -> Result<WriteOutcome, Error> {
        // import reads `.json` configs with serde_json, which rejects YAML-only syntax
        // (comments, unquoted keys, trailing commas) that serde_norway would accept
        let strict_json = path.extension().is_some_and(|ext| ext == "json");
        // validate a merged config against its request type so a broken hand-merge
        // is caught on save (with line/column) rather than at import time
        self.write_inner(path, content, progress, |resolved| {
            validate_config::<T>(resolved, strict_json)
        })
        .await
    }

    /// Write `content` to `path`, validating a "Merge" result as TOML that
    /// deserializes into `T`
    ///
    /// Covers `manifest.toml` and `config.toml`.
    ///
    /// # Arguments
    ///
    /// * `path` - The file to write
    /// * `content` - The content we want on disk
    /// * `progress` - The progress bar (suspended while prompting)
    pub async fn write_toml<T: DeserializeOwned>(
        &mut self,
        path: &Path,
        content: &str,
        progress: &Bar,
    ) -> Result<WriteOutcome, Error> {
        // validate a merged manifest/config so a broken hand-merge is caught on save
        self.write_inner(path, content, progress, |resolved| {
            toml::from_str::<T>(resolved).map(|_| ())
        })
        .await
    }

    /// Write `content` to `path`, resolving the case where `path` already exists
    /// with different content
    ///
    /// On a conflict the user can Merge (edit a conflict view, validated by
    /// `validate` on save), Overwrite, Skip, Overwrite/Skip all, or Quit.
    ///
    /// # Arguments
    ///
    /// * `path` - The file to write
    /// * `content` - The content we want on disk
    /// * `progress` - The progress bar (suspended while prompting)
    /// * `validate` - Validates a merged result on each save
    async fn write_inner<E: EditorParseError>(
        &mut self,
        path: &Path,
        content: &str,
        progress: &Bar,
        validate: impl Fn(&str) -> Result<(), E>,
    ) -> Result<WriteOutcome, Error> {
        let existing = tokio::fs::read_to_string(path).await.ok();
        // identical content already on disk: nothing to do
        if existing.as_deref() == Some(content) {
            return Ok(WriteOutcome::Skipped);
        }
        // the text we ultimately write — the new content, unless a merge replaces it
        let mut to_write = Cow::Borrowed(content);
        // an existing-but-different file needs a decision before we clobber it
        if let Some(existing) = &existing {
            match self.resolve(path, progress) {
                Resolution::Skip => return Ok(WriteOutcome::Skipped),
                Resolution::Overwrite => {}
                // `resolve` only returns Prompt when it didn't resolve itself
                Resolution::Prompt => match prompt_conflict(path, progress)? {
                    PromptChoice::Merge => {
                        // open a conflict view of on-disk vs new content in the editor,
                        // naming the temp file by the on-disk extension for highlighting
                        let ext = path
                            .extension()
                            .and_then(|ext| ext.to_str())
                            .unwrap_or("txt");
                        let label = path
                            .file_stem()
                            .and_then(|stem| stem.to_str())
                            .unwrap_or("merge");
                        let merged = progress
                            .suspend_async(editor::merge_in_editor(
                                existing,
                                content,
                                "Existing (on disk)",
                                "New (from export)",
                                label,
                                &self.editor,
                                ext,
                                &validate,
                            ))
                            .await?;
                        // cancelled in the editor: keep the on-disk file
                        let Some(merged) = merged else {
                            progress.info_anonymous(format!("Skipping '{}'", path.display()));
                            return Ok(WriteOutcome::Skipped);
                        };
                        // write what the user resolved
                        to_write = Cow::Owned(merged);
                    }
                    PromptChoice::Overwrite => {}
                    PromptChoice::Skip => {
                        progress.info_anonymous(format!("Skipping '{}'", path.display()));
                        return Ok(WriteOutcome::Skipped);
                    }
                    PromptChoice::OverwriteAll => self.overwrite_all = true,
                    PromptChoice::SkipAll => {
                        self.skip_all = true;
                        progress.info_anonymous(format!("Skipping '{}'", path.display()));
                        return Ok(WriteOutcome::Skipped);
                    }
                    PromptChoice::Quit => return Ok(WriteOutcome::Quit),
                },
            }
        }
        // create the parent directory and write
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|err| {
                Error::new(format!(
                    "Failed to create directory '{}': {err}",
                    parent.display()
                ))
            })?;
        }
        tokio::fs::write(path, to_write.as_ref())
            .await
            .map_err(|err| Error::new(format!("Failed to write '{}': {err}", path.display())))?;
        Ok(WriteOutcome::Written)
    }

    /// Resolve a conflict from remembered state alone (no prompting, no IO),
    /// returning [`Resolution::Prompt`] when the caller still needs to ask
    fn resolve_kind(&self) -> Resolution {
        if self.overwrite || self.overwrite_all {
            Resolution::Overwrite
        } else if self.skip_all {
            Resolution::Skip
        } else if self.can_prompt {
            Resolution::Prompt
        } else {
            // non-interactive and not forced
            Resolution::Skip
        }
    }

    /// Resolve a conflict, warning when we auto-skip a differing file because we
    /// can't prompt and weren't told to overwrite
    fn resolve(&self, path: &Path, progress: &Bar) -> Resolution {
        let kind = self.resolve_kind();
        // the only un-chosen skip is the non-interactive case; surface it loudly
        if matches!(kind, Resolution::Skip) && !self.skip_all {
            // don't suggest a different flag to a user who explicitly asked to skip
            let hint = if self.explicit_skip {
                "leaving it untouched (--skip-conflicts)"
            } else {
                "pass --overwrite to overwrite"
            };
            progress.warning(format!(
                "Skipping '{}' — an existing on-disk copy differs; {hint}",
                path.display()
            ));
        }
        kind
    }
}

/// A config parse error from whichever parser validated a merged file
#[derive(Debug)]
enum ConfigParseError {
    /// A strict JSON parse error (for `.json` files)
    Json(serde_json::Error),
    /// A YAML parse error (for every other config file)
    Yaml(serde_norway::Error),
}

impl std::fmt::Display for ConfigParseError {
    /// Display the underlying parser's error
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigParseError::Json(err) => err.fmt(f),
            ConfigParseError::Yaml(err) => err.fmt(f),
        }
    }
}

impl EditorParseError for ConfigParseError {
    /// The 1-based line/column reported by the underlying parser, if any
    fn location(&self) -> Option<(usize, usize)> {
        match self {
            // serde_json always reports a line; a column of 0 means "end of line"
            ConfigParseError::Json(err) => Some((err.line(), err.column().max(1))),
            ConfigParseError::Yaml(err) => EditorParseError::location(err),
        }
    }
}

/// Check that `text` deserializes into `T` with the parser import will use
///
/// # Arguments
///
/// * `text` - The config text to validate
/// * `strict_json` - Validate as strict JSON rather than YAML
fn validate_config<T: DeserializeOwned>(
    text: &str,
    strict_json: bool,
) -> Result<(), ConfigParseError> {
    if strict_json {
        // `.json` configs are read back with serde_json on import
        serde_json::from_str::<T>(text)
            .map(|_| ())
            .map_err(ConfigParseError::Json)
    } else {
        // everything else is YAML (a JSON superset)
        serde_norway::from_str::<T>(text)
            .map(|_| ())
            .map_err(ConfigParseError::Yaml)
    }
}

/// Fail early when `--review` was requested but there is no terminal for an editor
///
/// # Arguments
///
/// * `review` - Whether `--review` was requested
pub fn require_review_terminal(review: bool) -> Result<(), Error> {
    // an editor needs an interactive stdin and a terminal to draw on, and the review's
    // retry prompt draws on stderr
    if review
        && !(crate::handlers::imports::is_interactive_terminal() && std::io::stdout().is_terminal())
    {
        return Err(Error::new(
            "--review opens an editor for each config and needs a terminal; rerun without --review",
        ));
    }
    Ok(())
}

/// The combined result of an export: which resources failed and whether the user quit
#[derive(Debug, Default)]
pub struct ExportReport {
    /// The `(kind, name)` of every resource that failed to export, in the order they failed
    pub failures: Vec<(&'static str, String)>,
    /// The user chose Quit at a conflict prompt, so some resources were not exported
    pub stopped: bool,
}

impl ExportReport {
    /// Record a resource that failed to export
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of resource that failed (e.g. `image`)
    /// * `name` - The name of the resource that failed
    pub fn fail<N: Into<String>>(&mut self, kind: &'static str, name: N) {
        self.failures.push((kind, name.into()));
    }

    /// The final banner describing how the export ended
    pub fn banner(&self) -> &'static str {
        if self.stopped {
            "Export stopped early"
        } else if self.failures.is_empty() {
            "Export complete!"
        } else {
            "Export finished with errors"
        }
    }

    /// Summarize the failures grouped by kind, e.g. `1 pipeline(s): p; 2 image(s): a, b`
    fn failure_summary(&self) -> String {
        // group names by kind, keeping kinds in the order they first failed
        let mut groups: Vec<(&'static str, Vec<&str>)> = Vec::new();
        for (kind, name) in &self.failures {
            match groups.iter_mut().find(|(existing, _)| existing == kind) {
                Some((_, names)) => names.push(name),
                None => groups.push((kind, vec![name])),
            }
        }
        // render each group as `<count> <kind>(s): <names>`
        groups
            .iter()
            .map(|(kind, names)| format!("{} {kind}(s): {}", names.len(), names.join(", ")))
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// Turn this report into the command's result
    ///
    /// A user-chosen Quit is not a success: it returns an error so scripts don't
    /// treat a partial export as complete, and any failures are included.
    pub fn into_result(self) -> Result<(), Error> {
        match (self.stopped, self.failures.is_empty()) {
            (false, true) => Ok(()),
            (false, false) => Err(Error::new(format!(
                "Failed to export {}",
                self.failure_summary()
            ))),
            (true, true) => Err(Error::new(
                "Export stopped early at a conflict prompt; the export is incomplete",
            )),
            (true, false) => Err(Error::new(format!(
                "Export stopped early at a conflict prompt; the export is incomplete and also failed to export {}",
                self.failure_summary()
            ))),
        }
    }
}

/// How to write one kind of resource config to disk during an export
pub struct ConfigExport<'a> {
    /// The singular kind of resource being exported (e.g. `image`), used in messages
    pub kind: &'static str,
    /// The directory the `<name>.json` configs are written to
    pub dir: PathBuf,
    /// The curated key order the configs are serialized in
    pub order: &'a [&'a str],
    /// Whether to open each config in an editor for review before writing
    pub review: bool,
    /// The editor used for reviews
    pub editor: &'a str,
}

/// A config that ended up on disk (written this run or already present)
pub struct ExportedConfig<R> {
    /// The resource's name (the config's file stem)
    pub name: String,
    /// The request as it now exists on disk (reflecting any review or merge edits)
    pub request: R,
    /// Whether the config was written or left as-is
    pub outcome: WriteOutcome,
}

/// Serialize, optionally review, and write fetched resources as `<name>.json` configs
///
/// Writes run sequentially so the resolver can prompt without racing. Failures are
/// logged and recorded in `report` rather than aborting the export. On Quit the loop
/// stops, `report.stopped` is set, and the configs handled so far are returned so the
/// caller can still finish work that depends on them.
///
/// # Arguments
///
/// * `cfg` - How to write this kind of config
/// * `fetched` - Each resource's name paired with the result of fetching it
/// * `name_of` - Reads the name out of a request
/// * `resolver` - The on-disk conflict resolver shared across the whole export
/// * `progress` - The progress bar to log through
/// * `report` - The export report to record failures and a Quit in
pub async fn export_configs<E, R>(
    cfg: &ConfigExport<'_>,
    fetched: Vec<(String, Result<E, Error>)>,
    name_of: fn(&R) -> &str,
    resolver: &mut DiskConflictResolver,
    progress: &Bar,
    report: &mut ExportReport,
) -> Vec<ExportedConfig<R>>
where
    R: From<E> + Serialize + DeserializeOwned,
{
    // the kind of resource names every message and failure
    let kind = cfg.kind;
    // the configs that end up on disk, in input order
    let mut exported = Vec::with_capacity(fetched.len());
    for (name, result) in fetched {
        // skip (and record) anything we couldn't fetch
        let entity = match result {
            Ok(entity) => entity,
            Err(err) => {
                progress.error(format!("Failed to get {kind} '{name}': {err}"));
                report.fail(kind, name);
                continue;
            }
        };
        // write and read back this config, recording any failure without aborting
        match export_config::<E, R>(cfg, &name, entity, name_of, resolver, progress).await {
            Ok(Some(config)) => exported.push(config),
            // the user chose Quit; stop writing further configs
            Ok(None) => {
                report.stopped = true;
                break;
            }
            Err(err) => {
                progress.error(format!("Failed to export {kind} '{name}': {err}"));
                report.fail(kind, name);
            }
        }
    }
    exported
}

/// Serialize, optionally review, and write a single config, returning `None` on Quit
///
/// # Arguments
///
/// * `cfg` - How to write this kind of config
/// * `name` - The resource's name
/// * `entity` - The fetched resource
/// * `name_of` - Reads the name out of a request
/// * `resolver` - The on-disk conflict resolver
/// * `progress` - The progress bar to log through
async fn export_config<E, R>(
    cfg: &ConfigExport<'_>,
    name: &str,
    entity: E,
    name_of: fn(&R) -> &str,
    resolver: &mut DiskConflictResolver,
    progress: &Bar,
) -> Result<Option<ExportedConfig<R>>, Error>
where
    R: From<E> + Serialize + DeserializeOwned,
{
    // the kind of resource names the review label
    let kind = cfg.kind;
    // convert the fetched resource into the request import will read back
    let request = R::from(entity);
    // serialize in the curated field order shared with `init` and toolbox export
    let mut config_json = crate::utils::curated_json(&request, cfg.order)?;
    // optionally open the config in an editor for review before writing
    if cfg.review {
        config_json = progress
            .suspend_async(editor::review_config_in_editor::<R>(
                &config_json,
                &format!("export-{kind}-{name}"),
                cfg.editor,
                cfg.order,
            ))
            .await?;
        // import names a resource after its file, so a rename in review would split the
        // config from its tarball and from pipelines that reference the original name
        let reviewed: R = serde_json::from_str(&config_json)
            .map_err(|err| Error::new(format!("Failed to parse the reviewed config: {err}")))?;
        if name_of(&reviewed) != name {
            return Err(Error::new(format!(
                "the reviewed config renames it to '{}', which export doesn't support; rename it in Thorium instead",
                name_of(&reviewed)
            )));
        }
    }
    // write the config, resolving any on-disk conflict
    let path = cfg.dir.join(format!("{name}.json"));
    let outcome = resolver
        .write_yaml::<R>(&path, &config_json, progress)
        .await?;
    if outcome == WriteOutcome::Quit {
        return Ok(None);
    }
    // read back what is actually on disk (reflecting a merge or a skipped conflict) so
    // dependent work matches the config import will see; fall back to what we built
    let request = match tokio::fs::read_to_string(&path).await {
        Ok(text) => serde_json::from_str::<R>(&text).unwrap_or(request),
        Err(_) => request,
    };
    Ok(Some(ExportedConfig {
        name: name.to_owned(),
        request,
        outcome,
    }))
}

/// The user's per-file choice when an existing file differs
enum PromptChoice {
    /// Open an editor to merge the on-disk and new versions
    Merge,
    /// Overwrite this one file
    Overwrite,
    /// Keep this one file unchanged
    Skip,
    /// Overwrite this file and every later conflict
    OverwriteAll,
    /// Keep this file and every later conflict unchanged
    SkipAll,
    /// Stop the export
    Quit,
}

/// Map a 0-based menu selection to a [`PromptChoice`]
///
/// Merge is first (the default), followed by the overwrite/skip/all/quit options;
/// any out-of-range value falls back to Quit.
///
/// # Arguments
///
/// * `selection` - The 0-based index the user picked from the conflict menu
fn choice_from_selection(selection: usize) -> PromptChoice {
    match selection {
        0 => PromptChoice::Merge,
        1 => PromptChoice::Overwrite,
        2 => PromptChoice::Skip,
        3 => PromptChoice::OverwriteAll,
        4 => PromptChoice::SkipAll,
        _ => PromptChoice::Quit,
    }
}

/// Prompt the user about a single differing file
fn prompt_conflict(path: &Path, progress: &Bar) -> Result<PromptChoice, Error> {
    progress.suspend(|| {
        println!(
            "\n{} '{}' already exists on disk with different content.",
            "Conflict:".bright_yellow(),
            path.display().to_string().bright_blue(),
        );
        let items = &[
            "Merge         - open an editor to merge the on-disk and new versions",
            "Overwrite     - replace the file on disk",
            "Skip          - keep the file on disk unchanged",
            "Overwrite all - replace this and every later conflict",
            "Skip all      - keep this and every later conflict",
            "Quit          - stop the export",
        ];
        let selection = dialoguer::Select::new()
            .items(items)
            .default(0)
            .interact()
            .map_err(|err| Error::new(format!("Failed to read user input: {err}")))?;
        Ok(choice_from_selection(selection))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--overwrite` and "overwrite all" overwrite without prompting
    #[test]
    fn resolves_overwrite_without_prompt() {
        let forced = DiskConflictResolver::new(true, true, "vi".to_string());
        assert!(matches!(forced.resolve_kind(), Resolution::Overwrite));
        let mut all = DiskConflictResolver::new(false, true, "vi".to_string());
        all.overwrite_all = true;
        assert!(matches!(all.resolve_kind(), Resolution::Overwrite));
    }

    /// "skip all" skips without prompting
    #[test]
    fn resolves_skip_all_without_prompt() {
        let mut all = DiskConflictResolver::new(false, true, "vi".to_string());
        all.skip_all = true;
        assert!(matches!(all.resolve_kind(), Resolution::Skip));
    }

    /// a prompt-capable session defers to the user
    #[test]
    fn resolves_prompt_when_interactive() {
        let interactive = DiskConflictResolver::new(false, true, "vi".to_string());
        assert!(matches!(interactive.resolve_kind(), Resolution::Prompt));
    }

    /// a non-interactive, non-forced session skips (and would warn)
    #[test]
    fn resolves_skip_when_noninteractive() {
        let headless = DiskConflictResolver::new(false, false, "vi".to_string());
        assert!(matches!(headless.resolve_kind(), Resolution::Skip));
    }

    /// a `.json` config is validated as strict JSON, rejecting YAML-only syntax
    #[test]
    fn json_configs_are_validated_strictly() {
        #[derive(serde::Deserialize)]
        struct Named {
            #[allow(dead_code)]
            name: String,
        }
        assert!(validate_config::<Named>(r#"{"name": "a"}"#, true).is_ok());
        assert!(validate_config::<Named>("name: a", true).is_err());
        assert!(validate_config::<Named>("{\"name\": \"a\",}", true).is_err());
        assert!(validate_config::<Named>("name: a", false).is_ok());
    }

    /// the report banner and result never read as success after a Quit or a failure
    #[test]
    fn report_banner_and_result() {
        let clean = ExportReport::default();
        assert_eq!(clean.banner(), "Export complete!");
        assert!(clean.into_result().is_ok());
        let mut failed = ExportReport::default();
        failed.fail("image", "a");
        failed.fail("pipeline", "p");
        failed.fail("image", "b");
        assert_eq!(failed.banner(), "Export finished with errors");
        assert_eq!(
            failed.failure_summary(),
            "2 image(s): a, b; 1 pipeline(s): p"
        );
        assert!(failed.into_result().is_err());
        let stopped = ExportReport {
            failures: Vec::new(),
            stopped: true,
        };
        assert_eq!(stopped.banner(), "Export stopped early");
        assert!(stopped.into_result().is_err());
    }

    /// stopping prompts makes every later conflict skip without asking
    #[test]
    fn stop_prompting_skips_later_conflicts() {
        let mut resolver = DiskConflictResolver::new(false, true, "vi".to_string());
        resolver.stop_prompting();
        assert!(matches!(resolver.resolve_kind(), Resolution::Skip));
    }

    /// the conflict menu maps Merge first (the default) and Quit for any
    /// out-of-range selection
    #[test]
    fn selection_maps_merge_first() {
        assert!(matches!(choice_from_selection(0), PromptChoice::Merge));
        assert!(matches!(choice_from_selection(1), PromptChoice::Overwrite));
        assert!(matches!(choice_from_selection(2), PromptChoice::Skip));
        assert!(matches!(
            choice_from_selection(3),
            PromptChoice::OverwriteAll
        ));
        assert!(matches!(choice_from_selection(4), PromptChoice::SkipAll));
        assert!(matches!(choice_from_selection(5), PromptChoice::Quit));
        assert!(matches!(choice_from_selection(99), PromptChoice::Quit));
    }
}
