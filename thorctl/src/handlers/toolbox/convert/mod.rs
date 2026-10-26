//! `thorctl toolbox convert`: converting a toolbox manifest to the current format offline
//!
//! Nothing here talks to Thorium. The command reads a toolbox, converts it with
//! [`convert_document`], and then writes the manifest, checks it against an existing file, or
//! shows what it would write. Everything the command would print is collected in an
//! [`Execution`] first so tests can inspect it.

use similar::TextDiff;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use thorium::Error;

use crate::args::toolbox::{ConvertToolbox, ManifestLocation};
use crate::handlers::progress::{Bar, BarKind};
use crate::handlers::toolbox::shared;
use crate::handlers::toolbox::v2::load::SourceFormat;
use crate::handlers::toolbox::v2::types::ToolboxManifestV2;
use crate::utils::output::{self, OutputFormat};

mod legacy;
mod report;

#[cfg(test)]
mod tests;

pub use report::ConvertSummary;

pub use legacy::{ConvertOptions, convert_document};

/// The names of the manifest files a toolbox directory may hold, legacy first since that is
/// what a conversion starts from
const INPUT_FILES: [&str; 2] = ["toolbox.json", "toolbox.v2.json"];

/// What running `toolbox convert` produced, before anything is printed
#[derive(Debug, Clone, Default)]
pub struct Execution {
    /// What goes to stdout
    pub stdout: String,
    /// What goes to stderr
    pub stderr: String,
    /// Whether the command succeeded
    pub success: bool,
}

impl Execution {
    /// Add a line to what goes to stderr
    ///
    /// # Arguments
    ///
    /// * `line` - The line to add, without its newline
    fn say(&mut self, line: &str) {
        // each message is a line of its own
        self.stderr.push_str(line);
        self.stderr.push('\n');
    }
}

/// A toolbox read for conversion
pub(in crate::handlers::toolbox) struct Input {
    /// The manifest's bytes
    pub(in crate::handlers::toolbox) bytes: Vec<u8>,
    /// The file the manifest was read from, when it came from disk
    pub(in crate::handlers::toolbox) file: Option<PathBuf>,
}

/// Where the converted manifest goes
enum Target {
    /// Printed on stdout
    Stdout,
    /// Written to this file
    File(PathBuf),
    /// Compared with this file but never written, because the input is already converted
    Existing(PathBuf),
    /// Nowhere (a dry run of a URL with no output named)
    Nowhere,
}

/// Serialize a manifest the way converted manifests are written
///
/// # Arguments
///
/// * `manifest` - The manifest to serialize
pub fn render_manifest(manifest: &ToolboxManifestV2) -> Result<String, Error> {
    // two-space pretty JSON ending in a newline, from maps that are all sorted
    output::to_json(manifest)
}

/// Find the manifest file a path names, which may be a directory or its `config.toml`
///
/// # Arguments
///
/// * `path` - The path the user gave
fn locate_input(path: &Path) -> Result<PathBuf, Error> {
    // a config.toml stands for the directory it is in
    let dir = if path.file_name().is_some_and(|name| name == "config.toml") && path.is_file() {
        match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            _ => PathBuf::from("."),
        }
    } else if path.is_dir() {
        path.to_path_buf()
    } else {
        return Ok(path.to_path_buf());
    };
    // use the first manifest file the directory holds
    INPUT_FILES
        .iter()
        .map(|file| dir.join(file))
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| {
            Error::new(format!(
                "Failed to find a toolbox manifest in '{}': it holds no {}; run `thorctl toolbox \
                 build` to generate one",
                dir.display(),
                INPUT_FILES.join(" or ")
            ))
        })
}

/// Get the path a conversion is written to when none is named
///
/// # Arguments
///
/// * `file` - The input file
fn default_output(file: &Path) -> PathBuf {
    // `toolbox.json` converts to `toolbox.v2.json` in the same directory
    let stem = file.file_stem().map_or_else(
        || "toolbox".to_owned(),
        |stem| stem.to_string_lossy().into_owned(),
    );
    file.with_file_name(format!("{stem}.v2.json"))
}

/// Get the path an input's backup is saved to
///
/// # Arguments
///
/// * `file` - The input file
fn backup_path(file: &Path) -> PathBuf {
    // `.bak` is added to the whole file name
    let mut name = file
        .file_name()
        .map_or_else(OsString::new, OsStr::to_os_string);
    name.push(".bak");
    file.with_file_name(name)
}

/// Check the combinations of flags that can be refused before anything is read
///
/// # Arguments
///
/// * `cmd` - The `toolbox convert` command
fn check_flags(cmd: &ConvertToolbox) -> Result<(), Error> {
    let to_stdout =
        |path: &Option<PathBuf>| path.as_deref().is_some_and(|path| path == Path::new("-"));
    // the manifest and the report can't both use stdout
    if to_stdout(&cmd.output) && to_stdout(&cmd.report) {
        return Err(Error::new(
            "The manifest (-o -) and the report (--report -) can't both be written to stdout"
                .to_owned(),
        ));
    }
    // a URL has no file to replace and no place to put a result beside it
    if let ManifestLocation::Url(_) = cmd.input {
        if cmd.in_place {
            return Err(Error::new(
                "--in-place needs a toolbox on disk, not a URL; use -o PATH".to_owned(),
            ));
        }
        if cmd.output.is_none() && !cmd.dry_run {
            return Err(Error::new(
                "Converting a URL needs -o PATH (or -o - for stdout)".to_owned(),
            ));
        }
    }
    // there is nothing to compare when the result goes to stdout
    if cmd.check && to_stdout(&cmd.output) {
        return Err(Error::new(
            "--check compares with a file, not with -o -".to_owned(),
        ));
    }
    Ok(())
}

/// Read the toolbox to convert
///
/// # Arguments
///
/// * `location` - Where the toolbox is
/// * `bar` - The progress bar to track the read through
pub(in crate::handlers::toolbox) async fn read_input(
    location: &ManifestLocation,
    bar: &Bar,
) -> Result<Input, Error> {
    // download a URL, or read the manifest file a path names
    match location {
        ManifestLocation::Url(url) => Ok(Input {
            bytes: shared::download_manifest_bytes(url, bar).await?,
            file: None,
        }),
        ManifestLocation::Path(path) => {
            let file = locate_input(path)?;
            Ok(Input {
                bytes: shared::read_manifest_bytes(&file, bar).await?,
                file: Some(file),
            })
        }
    }
}

/// Choose where the converted manifest goes
///
/// # Arguments
///
/// * `cmd` - The `toolbox convert` command
/// * `input` - The toolbox that was read
/// * `source` - The format the input was in
fn choose_target(
    cmd: &ConvertToolbox,
    input: &Input,
    source: SourceFormat,
) -> Result<Target, Error> {
    // in place replaces the input file
    if cmd.in_place {
        let file = input
            .file
            .clone()
            .ok_or_else(|| Error::new("--in-place needs a toolbox on disk".to_owned()))?;
        return Ok(Target::File(file));
    }
    // a named output is a file, or stdout
    if let Some(output) = &cmd.output {
        return Ok(if output == Path::new("-") {
            Target::Stdout
        } else {
            Target::File(output.clone())
        });
    }
    match &input.file {
        // an input that is already converted has nothing to write
        Some(file) if source == SourceFormat::V2 => Ok(Target::Existing(file.clone())),
        // otherwise the result sits beside the input
        Some(file) => Ok(Target::File(default_output(file))),
        None => Ok(Target::Nowhere),
    }
}

/// Render a unified diff between the existing file and the converted manifest
///
/// # Arguments
///
/// * `path` - The existing file
/// * `old` - What the existing file holds
/// * `new` - The converted manifest
fn unified_diff(path: &Path, old: &str, new: &str) -> String {
    // three lines of context around every change
    TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .header(&path.display().to_string(), "converted")
        .to_string()
}

/// Render the report in the format asked for
///
/// # Arguments
///
/// * `summary` - The conversion's summary
/// * `format` - The format to render it in
fn render_report<T: output::View>(summary: &T, format: OutputFormat) -> Result<String, Error> {
    // human reports are plain text, since they go to stderr or a file
    output::render(summary, format, false)
}

/// Write a file, creating the directories above it
///
/// # Arguments
///
/// * `path` - The file to write
/// * `contents` - What to write
fn write_file(path: &Path, contents: &str) -> Result<(), Error> {
    // make sure the directory exists, then write
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .map_err(|err| Error::new(format!("Failed to create '{}': {err}", parent.display())))?;
    }
    std::fs::write(path, contents)
        .map_err(|err| Error::new(format!("Failed to write '{}': {err}", path.display())))
}

/// Writes, compares, or prints the converted manifest for one run of the command
struct Writer<'a> {
    /// The command
    cmd: &'a ConvertToolbox,
    /// The converted manifest as text
    text: &'a str,
    /// The execution being built
    execution: &'a mut Execution,
    /// Whether diffs go to stderr because stdout carries the JSON report
    diff_to_stderr: bool,
}

impl Writer<'_> {
    /// Print a diff where diffs go
    ///
    /// # Arguments
    ///
    /// * `diff` - The diff to print
    fn diff(&mut self, diff: &str) {
        // stdout normally, stderr when stdout carries the JSON report
        if self.diff_to_stderr {
            self.execution.stderr.push_str(diff);
        } else {
            self.execution.stdout.push_str(diff);
        }
    }

    /// Compare the converted manifest with an existing file for `--check` and `--dry-run`
    ///
    /// Returns whether the file is current.
    ///
    /// # Arguments
    ///
    /// * `path` - The file to compare with
    fn compare(&mut self, path: &Path) -> bool {
        let shown = path.display();
        match std::fs::read_to_string(path) {
            // the file is current
            Ok(old) if old == self.text => {
                self.execution.say(&format!("'{shown}' is up to date"));
                true
            }
            // the file differs, so show how
            Ok(old) => {
                let diff = unified_diff(path, &old, self.text);
                self.diff(&diff);
                self.execution
                    .say(&format!("'{shown}' differs from the converted toolbox"));
                false
            }
            // there is nothing to compare with
            Err(_) => {
                self.execution.say(&format!(
                    "'{shown}' does not exist yet; the converted toolbox would create it"
                ));
                false
            }
        }
    }

    /// Write the converted manifest to a file, saving a backup first when replacing the input
    ///
    /// # Arguments
    ///
    /// * `path` - The file to write
    fn write(&mut self, path: &Path) -> Result<(), Error> {
        // an identical file is left alone
        if std::fs::read_to_string(path).is_ok_and(|old| old == self.text) {
            self.execution
                .say(&format!("'{}' is already up to date", path.display()));
            return Ok(());
        }
        // replacing the input keeps the original beside it, unless that is turned off
        if self.cmd.in_place && !self.cmd.no_backup {
            let backup = backup_path(path);
            // an existing backup is never overwritten
            if backup.exists() {
                return Err(Error::new(format!(
                    "The backup '{}' already exists; move it away or pass --no-backup",
                    backup.display()
                )));
            }
            std::fs::copy(path, &backup).map_err(|err| {
                Error::new(format!("Failed to back up '{}': {err}", path.display()))
            })?;
            self.execution
                .say(&format!("Saved the original as '{}'", backup.display()));
        }
        // write the manifest
        write_file(path, self.text)?;
        self.execution.say(&format!("Wrote '{}'", path.display()));
        Ok(())
    }

    /// Do what the command asks with the converted manifest
    ///
    /// Returns whether the command succeeded.
    ///
    /// # Arguments
    ///
    /// * `target` - Where the converted manifest goes
    fn finish(&mut self, target: Target) -> Result<bool, Error> {
        let (check, dry_run) = (self.cmd.check, self.cmd.dry_run);
        match target {
            // a check fails unless the file is current
            Target::Existing(path) | Target::File(path) if check => Ok(self.compare(&path)),
            // a dry run reports a differing file but never fails
            Target::Existing(path) | Target::File(path) if dry_run => {
                if path.exists() {
                    self.compare(&path);
                }
                Ok(true)
            }
            // an input that is already converted has nothing to write
            Target::Existing(path) => {
                self.execution.say(&format!(
                    "'{}' is already in the v2 format; nothing to convert. Use -o PATH to write a \
                     normalized copy or --in-place to normalize it",
                    path.display()
                ));
                Ok(true)
            }
            // write the file
            Target::File(path) => self.write(&path).map(|()| true),
            // print the manifest, unless nothing is to be written
            Target::Stdout if !dry_run => {
                self.execution.stdout.push_str(self.text);
                Ok(true)
            }
            // nothing is written, and the report is all there is
            Target::Stdout | Target::Nowhere => Ok(true),
        }
    }
}

/// Print the report where it goes
///
/// Returns whether the report went to stdout.
///
/// # Arguments
///
/// * `cmd` - The `toolbox convert` command
/// * `summary` - The conversion's summary
/// * `execution` - The execution to add the report to
fn emit_report(
    cmd: &ConvertToolbox,
    summary: &ConvertSummary,
    execution: &mut Execution,
) -> Result<bool, Error> {
    match &cmd.report {
        // JSON on stdout
        Some(path) if path == Path::new("-") => {
            execution
                .stdout
                .push_str(&render_report(summary, OutputFormat::Json)?);
            Ok(true)
        }
        // JSON in a file
        Some(path) => {
            write_file(path, &render_report(summary, OutputFormat::Json)?)?;
            Ok(false)
        }
        // the requested format on stderr
        None => {
            let format = cmd.format.format.unwrap_or(OutputFormat::Human);
            execution.stderr.push_str(&render_report(summary, format)?);
            Ok(false)
        }
    }
}

/// Run `toolbox convert` without printing anything
///
/// # Arguments
///
/// * `cmd` - The `toolbox convert` command
pub async fn execute(cmd: &ConvertToolbox) -> Result<Execution, Error> {
    // refuse impossible flag combinations before reading anything
    check_flags(cmd)?;
    // read the toolbox without drawing a progress bar, since the report goes to the terminal
    let bar = Bar::new_or_quiet(
        "toolbox convert",
        "Reading manifest...",
        BarKind::UnboundIO,
        true,
    );
    let input = read_input(&cmd.input, &bar).await?;
    // convert it and write down the manifest that results
    let base_dir = input
        .file
        .as_ref()
        .and_then(|file| file.parent().map(Path::to_path_buf));
    let converted =
        convert_document(&input.bytes, base_dir, &ConvertOptions::from(cmd), &bar).await?;
    let text = render_manifest(&converted.manifest)?;
    // decide where the manifest goes and say so in the report
    let target = choose_target(cmd, &input, converted.summary.source)?;
    let mut summary = converted.summary;
    summary.output = match &target {
        Target::File(path) | Target::Existing(path) => Some(path.display().to_string()),
        Target::Stdout | Target::Nowhere => None,
    };
    // print the report first so a refusal below is read with it in view
    let mut execution = Execution::default();
    let report_to_stdout = emit_report(cmd, &summary, &mut execution)?;
    // names that are defined twice can't be converted until the groups are settled
    if !summary.collisions.is_empty() {
        let names = summary.collisions.join(", ");
        execution.say(&format!(
            "Not converted: {names} defined more than once (collision); pass --group-override \
             GROUP to put everything in one group"
        ));
        return Ok(execution);
    }
    // a strict conversion stops at the first issue, before anything is written
    if cmd.strict && summary.has_issues() {
        execution.say("Not converted: --strict was given and the conversion raised issues");
        return Ok(execution);
    }
    // write, compare, or print the manifest as asked
    let mut writer = Writer {
        cmd,
        text: &text,
        execution: &mut execution,
        diff_to_stderr: report_to_stdout,
    };
    let success = writer.finish(target)?;
    execution.success = success;
    Ok(execution)
}

/// Convert a toolbox and print what the command reports
///
/// Returns whether the command succeeded, so the caller can exit non-zero when it did not.
///
/// # Arguments
///
/// * `cmd` - The `toolbox convert` command
pub async fn convert(cmd: &ConvertToolbox) -> Result<bool, Error> {
    // run the command, then print what it produced
    let execution = execute(cmd).await?;
    print!("{}", execution.stdout);
    eprint!("{}", execution.stderr);
    Ok(execution.success)
}
