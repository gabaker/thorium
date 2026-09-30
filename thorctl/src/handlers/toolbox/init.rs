//! Scaffolds toolbox, image, and pipeline files with default configs
//!
//! Supports both interactive (default) and non-interactive (`-n`) modes.
//! Interactive mode builds the default config and opens it in the user's editor
//! to fill in (see `fill_config`); it requires a terminal. Non-interactive mode
//! writes the defaults with no editor. Names, groups, and references are validated
//! before the editor opens, and nothing is written until the config is valid.

use colored::Colorize;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::collections::{HashMap, HashSet};
use std::io::IsTerminal;
use std::path::{Component, Path, PathBuf};
use thorium::Error;
use thorium::models::{ImageRequest, PipelineRequest};
use walkdir::WalkDir;

use super::build::{BaseImage, config_base_dir, default_version, load_config};
use super::prompt::{self, ImageConfigAnswers, PipelineConfigAnswers};
use crate::args::Args;
use crate::args::toolbox::{Init, InitImage, InitPipeline, InitToolbox, PipelineSpec};
use crate::handlers::imports::editor;
use crate::handlers::imports::merge::{IMAGE_FIELD_ORDER, PIPELINE_FIELD_ORDER};
use crate::handlers::progress;

// ─── File Helpers ────────────────────────────────────────────────────────────

/// Writes a scaffolded file, skipping or overwriting an existing one based on `overwrite`
///
/// Returns `true` when the file was written and `false` when it already existed and
/// was skipped.
///
/// # Arguments
///
/// * `path` - The path to write to
/// * `contents` - The file contents to write
/// * `overwrite` - Overwrite an existing file instead of skipping it
pub(crate) async fn write_file(
    path: &Path,
    contents: &str,
    overwrite: bool,
) -> Result<bool, Error> {
    // stat once and surface a real IO error rather than treating it as "absent",
    // which would silently overwrite a file we couldn't read
    let exists = tokio::fs::try_exists(path)
        .await
        .map_err(|e| Error::new(format!("Failed to stat '{}': {e}", path.display())))?;
    // never clobber an existing file unless the caller opted in; report the skip so
    // the user knows a stale file was left untouched and how to force a replace
    if !overwrite && exists {
        println!(
            "{} {} (already exists; pass --overwrite to replace)",
            "Skipped".bright_yellow(),
            path.display()
        );
        return Ok(false);
    }
    // ensure the destination directory tree exists before writing into it
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|e| {
            Error::new(format!(
                "Failed to create directory '{}': {e}",
                parent.display()
            ))
        })?;
    }
    // distinguish replacing a present file from creating a new one for the status line;
    // captured before the write because the file always exists afterward
    let overwritten = overwrite && exists;
    tokio::fs::write(path, contents)
        .await
        .map_err(|e| Error::new(format!("Failed to write '{}': {e}", path.display())))?;
    // tell the user which action happened (replaced vs newly created)
    if overwritten {
        println!("{} {}", "Overwrote".bright_yellow(), path.display());
    } else {
        println!("{} {}", "Created".bright_green(), path.display());
    }
    Ok(true)
}

/// Lexically normalize a path against the current directory, resolving `.` and `..`
///
/// Used as a fallback for paths that don't exist yet (so [`std::fs::canonicalize`] can't
/// resolve them). Symlinks are not followed, so `a/link/..` resolves to `a`.
///
/// # Arguments
///
/// * `path` - The path to normalize
fn normalize_lexically(path: &Path) -> PathBuf {
    // anchor a relative path at the cwd so a leading `..` has a parent to climb into
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut normalized = PathBuf::new();
    // replay the components, dropping `.` and popping the previous component on `..`
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

/// Extracts the final directory component of a path as a string
///
/// A path with no literal final component (`.`, `..`, or one ending in `..`) is resolved to
/// the directory it names first, so `init image .` takes the current directory's name.
///
/// # Arguments
///
/// * `path` - The path to take the directory name from
fn dir_name(path: &Path) -> Result<String, Error> {
    // take the trailing path component directly when the path spells one out
    let name = match path.file_name() {
        Some(name) => Some(name.to_os_string()),
        // `.`/`..` have no file name, so resolve the directory they refer to: canonicalize
        // when it exists, else normalize lexically against the cwd
        None => std::fs::canonicalize(path)
            .unwrap_or_else(|_| normalize_lexically(path))
            .file_name()
            .map(std::ffi::OsStr::to_os_string),
    };
    // error on a path with no final component even after resolution (e.g. `/`) or non-UTF-8 bytes
    name.and_then(|name| name.to_str().map(String::from))
        .ok_or_else(|| {
            Error::new(format!(
                "Cannot determine directory name for '{}'",
                path.display()
            ))
        })
}

/// Decide whether an init subcommand may prompt and open an editor
///
/// Interactive mode needs a terminal on stdin (to read answers) and stderr (where the prompts
/// draw); without one, the prompts would fail or an editor would be launched blind, so this
/// errors and points at `-n` rather than guessing.
///
/// # Arguments
///
/// * `non_interactive` - Whether `--non-interactive` was passed
/// * `required` - The flags this subcommand requires with `-n` (e.g. `--group`)
fn resolve_interactive(non_interactive: bool, required: &str) -> Result<bool, Error> {
    // an explicit -n never prompts
    if non_interactive {
        return Ok(false);
    }
    // interactive mode requires a real terminal for both input and the prompt UI
    if std::io::stdin().is_terminal() && std::io::stderr().is_terminal() {
        Ok(true)
    } else {
        Err(Error::new(no_terminal_message(required)))
    }
}

/// The error shown when an init subcommand needs a terminal but has none
///
/// # Arguments
///
/// * `required` - The flags this subcommand requires with `-n` (e.g. `--group`)
fn no_terminal_message(required: &str) -> String {
    format!(
        "toolbox init is interactive and needs a terminal; pass -n/--non-interactive (with \
         {required}) to scaffold the defaults without prompts or an editor"
    )
}

/// Resolve a tool's name from its directory basename, validating it before any editor opens
///
/// In interactive mode an invalid basename (e.g. uppercase letters) is replaced by prompting for a
/// valid name; in non-interactive mode it is an error.
///
/// # Arguments
///
/// * `kind` - The resource kind, for prompts and errors ("image" or "pipeline")
/// * `path` - The tool directory the name defaults from
/// * `interactive` - Whether the user can be prompted
fn resolve_tool_name(kind: &str, path: &Path, interactive: bool) -> Result<String, Error> {
    // the name defaults to the directory's basename
    let name = dir_name(path)?;
    // a valid basename is used as-is
    let Err(err) = validate_resource_name(kind, &name) else {
        return Ok(name);
    };
    // non-interactive can't ask for a replacement, so the invalid name is fatal
    if !interactive {
        return Err(err);
    }
    // interactive: explain why the basename can't be used and ask for a valid name instead
    progress::warn(format!(
        "{err}; choose a valid {kind} name for '{}'",
        path.display()
    ));
    dialoguer::Input::<String>::new()
        .with_prompt(format!("{} name", capitalize(kind)))
        .validate_with(|value: &String| prompt::validate_name(value, prompt::RESOURCE_NAME_MAX))
        .interact_text()
        .map_err(|err| Error::new(format!("Failed to read input: {err}")))
}

/// Uppercase the first character of a word, for prompt labels
///
/// # Arguments
///
/// * `word` - The word to capitalize
fn capitalize(word: &str) -> String {
    // split off the first character and uppercase just that one
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Reject a resource name that isn't a valid identifier before it is interpolated
/// into a TOML manifest template
///
/// The interactive wizard validates through `prompt_name`; the `--non-interactive`
/// path takes names verbatim (directory names, `--image`, `--group`), so it must
/// run the same check or a crafted name could break out of the template.
///
/// # Arguments
///
/// * `kind` - The resource kind, for the error message ("group", "image", …)
/// * `name` - The name to validate
fn validate_resource_name(kind: &str, name: &str) -> Result<(), Error> {
    // groups allow a longer name than images/pipelines, matching the API's own per-kind caps
    let max = if kind == "group" {
        prompt::GROUP_NAME_MAX
    } else {
        prompt::RESOURCE_NAME_MAX
    };
    // reuse the wizard's name check so interactive and non-interactive paths enforce the same
    // rule, then prefix the error with the resource kind for context
    prompt::validate_name(name, max)
        .map_err(|err| Error::new(format!("Invalid {kind} name '{name}': {err}")))
}

/// Reject an export-layout path that isn't a safe relative subpath of the toolbox root
///
/// A configured layout dir (`export_image_path`/`export_pipeline_path`) or a per-resource
/// `=destpath` must stay inside the toolbox, so an absolute path or one escaping via `..` is
/// rejected before it is written into `config.toml` or used to place files.
///
/// # Arguments
///
/// * `kind` - The setting's name, for the error message
/// * `path` - The path to validate
pub(crate) fn validate_relative_subpath(kind: &str, path: &str) -> Result<(), Error> {
    let candidate = Path::new(path);
    // an empty path would resolve to the toolbox root itself rather than a subdirectory
    if path.trim().is_empty() {
        return Err(Error::new(format!(
            "{kind} must be a non-empty relative path inside the toolbox"
        )));
    }
    // an absolute or rooted path would place files outside the toolbox root entirely. The string
    // checks catch forms the host platform doesn't parse as rooted but Windows does (`\dir`, `C:dir`,
    // `C:\dir`), since a config.toml written on one OS may be used on another
    let bytes = path.as_bytes();
    let drive_prefixed = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    if candidate.is_absolute()
        || path.starts_with('/')
        || path.starts_with('\\')
        || drive_prefixed
        || candidate
            .components()
            .any(|component| matches!(component, Component::Prefix(_) | Component::RootDir))
    {
        return Err(Error::new(format!(
            "{kind} '{path}' must be a relative path inside the toolbox, not an absolute path"
        )));
    }
    // a `..` segment would climb out of the toolbox root; split on both separators so a
    // Windows-style `..\dir` is caught on every platform
    if path.split(['/', '\\']).any(|segment| segment == "..") {
        return Err(Error::new(format!(
            "{kind} '{path}' must stay inside the toolbox (no '..' components)"
        )));
    }
    Ok(())
}

/// A minimal view of a `manifest.toml` for discovering a toolbox's existing images
///
/// Only the fields needed to identify image entries are deserialized; everything else in the
/// manifest is ignored. Used by [`collect_toolbox_images`] to validate `init pipeline -c`
/// references and to detect an `init image -c` duplicate identity.
#[derive(Deserialize)]
struct ManifestProbe {
    /// The resource name
    name: String,
    /// `"image"` or `"pipeline"`
    #[serde(rename = "type")]
    manifest_type: String,
    /// The version label; defaults to `latest` when the manifest omits it
    #[serde(default = "default_version")]
    version: String,
}

/// Walk a toolbox (the directory of its `config.toml`) for image manifests, mapping each image
/// name to the versions found for it
///
/// Used by `init -c` as the toolbox's resolution source: it lets `init pipeline` confirm a
/// referenced image exists and pin its real version, and `init image` detect a duplicate
/// name+version. The config itself must exist and parse (so a typo'd `-c` path is reported as
/// such); unreadable/unparsable manifests are skipped with a warning (a best-effort discovery,
/// not a build).
///
/// # Arguments
///
/// * `config` - The path to the toolbox's `config.toml`
fn collect_toolbox_images(config: &Path) -> Result<HashMap<String, Vec<String>>, Error> {
    // load the config first so a missing/unreadable/invalid `-c` path fails with a clear message
    // instead of silently yielding an empty toolbox
    load_config(config)?;
    // the toolbox root is the config's directory (a bare `config.toml` means the cwd)
    let root = config_base_dir(config);
    let mut images: HashMap<String, Vec<String>> = HashMap::new();
    // walk every manifest.toml under the toolbox root, recording image (name, version) pairs
    for entry in WalkDir::new(&root).into_iter().filter_map(Result::ok) {
        // only manifest.toml files describe tools
        if entry.file_name() != "manifest.toml" {
            continue;
        }
        // skip a manifest that won't read or parse, but say so since it may be the image the
        // caller expects to find
        let probe = std::fs::read_to_string(entry.path())
            .map_err(|err| err.to_string())
            .and_then(|text| toml::from_str::<ManifestProbe>(&text).map_err(|err| err.to_string()));
        let probe = match probe {
            Ok(probe) => probe,
            Err(err) => {
                progress::warn(format!(
                    "skipping unreadable manifest '{}': {err}",
                    entry.path().display()
                ));
                continue;
            }
        };
        // only images are a resolution source for a pipeline's references
        if probe.manifest_type == "image" {
            images.entry(probe.name).or_default().push(probe.version);
        }
    }
    Ok(images)
}

/// Pick the version to pin for a referenced image found in the toolbox: prefer `latest`, else the
/// first discovered version
///
/// # Arguments
///
/// * `versions` - The versions discovered for the image in the toolbox
fn pin_version(versions: &[String]) -> String {
    // prefer `latest`, else the first discovered version, else the manifest default
    versions
        .iter()
        .find(|version| version.as_str() == "latest")
        .or_else(|| versions.first())
        .cloned()
        .unwrap_or_else(default_version)
}

/// Escape a value for use inside a TOML basic (double-quoted) string
///
/// `config.toml`'s `name`/`registry` are free-form (spaces, slashes), so they
/// can't go through [`validate_resource_name`]; escaping instead keeps a stray
/// quote or newline from corrupting the generated TOML.
///
/// # Arguments
///
/// * `value` - The raw string to escape
pub(crate) fn toml_escape(value: &str) -> String {
    // backslash must be escaped first so the escapes introduced for the other
    // characters below aren't themselves doubled by a later pass
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

/// Render a resource name as a single TOML table-key segment
///
/// A non-empty name composed only of TOML bare-key characters (ASCII letters, digits, `-`, `_`) is
/// emitted unquoted — which every API-validated image/pipeline name is, since those are bounded to
/// 1–25 lowercase alphanumeric or `-` characters. Anything else (e.g. a hand-edited manifest with a
/// dot, space, or other special character) falls back to a quoted, escaped basic-string key so the
/// generated TOML stays valid and unambiguous (an unquoted dot would be parsed as a table path).
///
/// # Arguments
///
/// * `name` - The resource name to render as a key segment
pub(crate) fn toml_key(name: &str) -> String {
    // a non-empty name of only bare-key characters can be written without quotes
    let bare = !name.is_empty()
        && name
            .chars()
            .all(|chr| chr.is_ascii_alphanumeric() || chr == '-' || chr == '_');
    if bare {
        name.to_string()
    } else {
        // fall back to a quoted basic-string key, escaping anything that would break the quoting
        format!("\"{}\"", toml_escape(name))
    }
}

/// Render a toolbox `config.toml` from its toolbox-wide settings
///
/// Shared by `toolbox init` and `toolbox export` so the two can't drift. An unset
/// `registry`, empty `registries`, an unset `image_path_prefix`, and unset
/// `export_image_path`/`export_pipeline_path` are emitted as commented-out placeholders to
/// document the available knobs; `bundled_images` is only written when true.
///
/// # Arguments
///
/// * `name` - The toolbox name
/// * `registry` - The primary container registry, or `None` to leave it unset
/// * `registries` - Extra registries to additionally tag for
/// * `image_path_prefix` - The default bundled-image registry base path, if any
/// * `export_image_path` - The dir `export` writes image tool dirs under, or `None` for `images`
/// * `export_pipeline_path` - The dir `export` writes pipeline tool dirs under, or `None` for `pipelines`
/// * `bundled_images` - Whether the toolbox bundles image tarballs
/// * `base_image` - The toolbox-wide default base-image configuration, if any
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_config_toml(
    name: &str,
    registry: Option<&str>,
    registries: &[String],
    image_path_prefix: Option<&str>,
    export_image_path: Option<&str>,
    export_pipeline_path: Option<&str>,
    bundled_images: bool,
    base_image: Option<&BaseImage>,
) -> String {
    // name is the one always-present required key, so it anchors the top of the file
    let mut out = format!("name = \"{}\"\n", toml_escape(name));
    // a set registry is written out; an unset one is a commented placeholder so the
    // generated config documents the knob without forcing a (possibly wrong) value
    match registry {
        Some(registry) => out.push_str(&format!("registry = \"{}\"\n", toml_escape(registry))),
        None => out.push_str("# registry = \"\"\n"),
    }
    // emit extra registries as a real array only when present; otherwise a commented
    // empty-array placeholder documents the knob
    if registries.is_empty() {
        out.push_str("# registries = []\n");
    } else {
        let quoted: Vec<String> = registries
            .iter()
            .map(|registry| format!("\"{}\"", toml_escape(registry)))
            .collect();
        out.push_str(&format!("registries = [{}]\n", quoted.join(", ")));
    }
    // only write bundled_images when true; the false default is left implicit rather
    // than spelled out as a placeholder
    if bundled_images {
        out.push_str("bundled_images = true\n");
    }
    // a set prefix is written out; an unset one is a commented placeholder
    match image_path_prefix {
        Some(prefix) => out.push_str(&format!(
            "image_path_prefix = \"{}\"\n",
            toml_escape(prefix)
        )),
        None => out.push_str("# image_path_prefix = \"\"\n"),
    }
    // export layout dirs: a set value is written out; an unset one is a commented placeholder
    // documenting the default (export writes under `images`/`pipelines` when unset)
    match export_image_path {
        Some(path) => out.push_str(&format!("export_image_path = \"{}\"\n", toml_escape(path))),
        None => out.push_str("# export_image_path = \"images\"\n"),
    }
    match export_pipeline_path {
        Some(path) => out.push_str(&format!(
            "export_pipeline_path = \"{}\"\n",
            toml_escape(path)
        )),
        None => out.push_str("# export_pipeline_path = \"pipelines\"\n"),
    }
    // the base-image config is a TOML table, so it must come after every scalar key; an unset one
    // is a commented placeholder documenting the knobs
    match base_image {
        Some(base) => {
            // open the table; the blank line keeps it visually separate from the scalars above
            out.push_str("\n[base_image]\n");
            // each base-image field is optional, so only emit the ones that are set
            if let Some(image) = &base.image {
                out.push_str(&format!("image = \"{}\"\n", toml_escape(image)));
            }
            if let Some(image_arg) = &base.image_arg {
                out.push_str(&format!("image_arg = \"{}\"\n", toml_escape(image_arg)));
            }
            if let Some(token) = &base.token {
                out.push_str(&format!("token = \"{}\"\n", toml_escape(token)));
            }
            if let Some(user) = &base.user {
                out.push_str(&format!("user = \"{}\"\n", toml_escape(user)));
            }
            // allow_override is a bool, not a string, so it is written without quoting/escaping
            if let Some(allow) = base.allow_override {
                out.push_str(&format!("allow_override = {allow}\n"));
            }
        }
        None => out.push_str(
            "\n# [base_image]\n\
             # image = \"\"\n\
             # image_arg = \"IMAGE\"\n\
             # token = \"\"\n\
             # user = \"\"\n\
             # allow_override = true\n",
        ),
    }
    out
}

// ─── Config Builders ─────────────────────────────────────────────────────────

/// Renders the default image config JSON, with every `ImageRequest` field present
///
/// Emits all fields (including ones carrying default values) so the scaffolded
/// `<name>.json` is a complete, editable starting point.
///
/// # Arguments
///
/// * `answers` - The wizard answers seeding the config's identity and key fields
fn build_image_config(answers: &ImageConfigAnswers) -> String {
    // build the full ImageRequest shape with every field spelled out (even defaulted
    // ones) so the scaffolded file is a complete, editable reference; the wizard answers
    // seed identity and the few interactively chosen fields
    serde_json::to_string_pretty(&serde_json::json!({
        "group": answers.group,
        "name": answers.name,
        "version": null,
        "scaler": answers.scaler,
        "image": answers.image_tag,
        "lifetime": null,
        "modifiers": null,
        "timeout": answers.timeout,
        "resources": {
            "cpu": answers.cpu,
            "memory": answers.memory,
            "ephemeral_storage": "0Mi",
            "nvidia_gpu": 0,
            "amd_gpu": 0
        },
        "spawn_limit": "Unlimited",
        "volumes": [],
        "env": {},
        "args": {
            "entrypoint": null,
            "command": null,
            "reaction": null,
            "repo": null,
            "commit": null,
            "output": "None"
        },
        "description": answers.description,
        "security_context": {
            "user": null,
            "group": null,
            "allow_privilege_escalation": false
        },
        "collect_logs": true,
        "generator": answers.generator,
        "dependencies": {
            "samples": {
                "location": "/tmp/thorium/samples",
                "kwarg": null,
                "strategy": "Paths"
            },
            "ephemeral": {
                "location": "/tmp/thorium/ephemeral",
                "kwarg": null,
                "strategy": "Paths",
                "names": []
            },
            "results": {
                "images": [],
                "location": "/tmp/thorium/prior-results",
                "kwarg": "None",
                "strategy": "Paths",
                "names": []
            },
            "repos": {
                "location": "/tmp/thorium/repos",
                "kwarg": null,
                "strategy": "Paths"
            },
            "tags": {
                "enabled": false,
                "location": "/tmp/thorium/prior-tags",
                "kwarg": null,
                "strategy": "Paths"
            },
            "children": {
                "enabled": false,
                "images": [],
                "location": "/tmp/thorium/prior-children",
                "kwarg": null,
                "strategy": "Paths"
            }
        },
        "display_type": answers.display_type,
        "output_collection": {
            "handler": "Files",
            "files": {
                "results": "/tmp/thorium/results",
                "result_files": "/tmp/thorium/result-files",
                "tags": "/tmp/thorium/tags",
                "names": []
            },
            "children": "/tmp/thorium/children",
            "auto_tag": {},
            "groups": []
        },
        "child_filters": {
            "mime": [],
            "file_name": [],
            "file_extension": [],
            "submit_non_matches": false
        },
        "clean_up": null,
        "kvm": null,
        "network_policies": []
    }))
    // the template is a fixed shape built from owned strings, so serialization cannot fail
    .expect("static JSON template must serialize")
}

/// Renders the default pipeline config JSON, with every `PipelineRequest` field present
///
/// # Arguments
///
/// * `answers` - The wizard answers seeding the config's identity and order/sla
fn build_pipeline_config(answers: &PipelineConfigAnswers) -> String {
    // emit the full PipelineRequest shape; triggers starts empty for the user to fill in
    serde_json::to_string_pretty(&serde_json::json!({
        "group": answers.group,
        "name": answers.name,
        "order": answers.order,
        "sla": answers.sla,
        "triggers": {},
        "description": answers.description
    }))
    // the template is a fixed shape built from owned values, so serialization cannot fail
    .expect("static JSON template must serialize")
}

// ─── Manifest Generators ────────────────────────────────────────────────────

/// Renders an image's `manifest.toml` from its identity and build settings
///
/// # Arguments
///
/// * `name` - The tool name
/// * `image_name_field` - The `image_name` manifest field (already TOML-escaped)
/// * `version` - The image version
/// * `no_build` - Whether to mark the image as not built by CI (`build = false`)
/// * `policy_files` - Bundled network policy definition files to reference
/// * `exported_image_path` - The real registry url an export captured, if any
pub(crate) fn generate_image_manifest(
    name: &str,
    image_name_field: &str,
    version: &str,
    no_build: bool,
    policy_files: &[String],
    exported_image_path: Option<&str>,
) -> String {
    // a Thorium version is a free-form Custom(String) on export, so escape it before it
    // goes into a TOML basic string; a stray quote/newline would otherwise corrupt the
    // generated manifest (name/image_name_field are already validated/escaped by callers)
    let version = toml_escape(version);
    // lay down the required scalar keys first; config_from points at the sibling JSON
    // named after the tool, and build_path defaults to the manifest's own directory
    let mut manifest = format!(
        "name = \"{name}\"\n\
         type = \"image\"\n\
         config_from = \"{name}.json\"\n\
         build_path = \"./\"\n\
         image_name = \"{image_name_field}\"\n\
         version = \"{version}\"\n"
    );
    // record the real registry url an export captured, so a build that doesn't
    // rebuild this image (build = false) keeps it instead of deriving a path
    if let Some(path) = exported_image_path.filter(|path| !path.is_empty()) {
        manifest.push_str(&format!(
            "exported_image_path = \"{}\"\n",
            toml_escape(path)
        ));
    }
    // reference any bundled network policy definition files
    if !policy_files.is_empty() {
        let quoted: Vec<String> = policy_files
            .iter()
            .map(|file| format!("\"{file}\""))
            .collect();
        manifest.push_str(&format!(
            "network_policies_from = [{}]\n",
            quoted.join(", ")
        ));
    }
    // write an explicit `build = false` when the image is reference-only; otherwise leave
    // the `true` default as a commented hint documenting how to flip it
    if no_build {
        manifest.push_str("build = false\n");
    } else {
        manifest.push_str("# build = true\n");
    }
    // a commented per-tool base-image config (a TOML table, so it trails the scalar keys); token
    // and user are CI/CD variable names, not used by `build-images`
    manifest.push_str(
        "\n\
        # [base_image]\n\
        # image = \"\"\n\
        # image_arg = \"IMAGE\"\n\
        # token = \"\"\n\
        # user = \"\"\n\
        # allow_override = true\n",
    );
    manifest
}

/// Renders a pipeline's `manifest.toml` from its name and referenced images
///
/// # Arguments
///
/// * `name` - The pipeline name
/// * `images` - The (image name, version) pairs the pipeline references
pub(crate) fn generate_pipeline_manifest(name: &str, images: &[(String, String)]) -> String {
    // lay down the required scalar keys; config_from points at the sibling JSON config
    let mut manifest = format!(
        "name = \"{name}\"\n\
         type = \"pipeline\"\n\
         version = \"latest\"\n\
         config_from = \"{name}.json\"\n"
    );
    // append an [images.<name>] table per referenced image so the manifest's image map mirrors the
    // images the pipeline's order runs. The name is rendered as a bare key when it is TOML-bare-safe
    // (every API-validated name is — 1–25 lowercase alphanumeric or '-'), only falling back to a
    // quoted/escaped key for a hand-edited name with a special character; the version is always a
    // basic string, escaped defensively against a stray quote/newline
    for (image_name, version) in images {
        manifest.push_str(&format!(
            "\n[images.{}]\nversion = \"{}\"\n",
            toml_key(image_name),
            toml_escape(version)
        ));
    }
    manifest
}

// ─── Shared Write Helpers ────────────────────────────────────────────────────

/// The result of scaffolding one tool directory
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteOutcome {
    /// The scaffold was written (individual files that already existed may still have been
    /// skipped, each reported as it happened)
    Written,
    /// Every target file already existed and `--overwrite` wasn't set, so nothing was written
    /// and no editor was opened
    Skipped,
    /// The user cancelled in the editor, so nothing was written
    Cancelled,
}

/// Check a scaffold's target files before any editing happens
///
/// Returns `false` when every target already exists and would be skipped (so the caller
/// shouldn't open an editor whose changes would be thrown away). When only some exist, warns up
/// front that those will be kept.
///
/// # Arguments
///
/// * `path` - The tool directory being scaffolded
/// * `name` - The tool name the `<name>.json` config is named after
/// * `overwrite` - Whether `--overwrite` was passed (existing files are replaced)
async fn precheck_targets(path: &Path, name: &str, overwrite: bool) -> Result<bool, Error> {
    // with --overwrite every target is (re)written, so there's nothing to check
    if overwrite {
        return Ok(true);
    }
    // the three files a scaffold writes
    let files = [
        "manifest.toml".to_string(),
        format!("{name}.json"),
        "description.md".to_string(),
    ];
    let mut existing = Vec::new();
    // stat each target, surfacing a real IO error rather than treating it as absent
    for file in &files {
        let target = path.join(file);
        let exists = tokio::fs::try_exists(&target)
            .await
            .map_err(|err| Error::new(format!("Failed to stat '{}': {err}", target.display())))?;
        if exists {
            existing.push(file.as_str());
        }
    }
    // every target exists: report the skip now instead of after an editor session
    if existing.len() == files.len() {
        println!(
            "{} {} ({} already exist; pass --overwrite to replace)",
            "Skipped".bright_yellow(),
            path.display(),
            existing.join(", ")
        );
        return Ok(false);
    }
    // some targets exist: warn before editing that those files won't take the edits
    if !existing.is_empty() {
        progress::warn(format!(
            "{} already exist in '{}' and will be kept (pass --overwrite to replace)",
            existing.join(", "),
            path.display()
        ));
    }
    Ok(true)
}

/// Ask whether to reopen the editor after the saved config failed validation
///
/// Returns `true` to reopen the editor with the user's content, `false` to cancel.
fn prompt_reopen_editor() -> Result<bool, Error> {
    // offer the same Edit/Cancel choice the editor loop uses for parse errors
    let items = &[
        "Edit   - Reopen the editor with your changes to fix the issue",
        "Cancel - Discard this config and write nothing",
    ];
    let selection = dialoguer::Select::new()
        .items(items)
        .default(0)
        .interact()
        .map_err(|err| Error::new(format!("Failed to read user input: {err}")))?;
    Ok(selection == 0)
}

/// Fill in a tool's config (in the editor when interactive) and validate it before anything is
/// written
///
/// `validate` runs on the default config before the editor opens, so a bad name/group/reference
/// fails before the user invests any editing time, and again on the saved config; when the saved
/// config fails, the user can reopen the editor with their own content instead of losing it.
/// Returns `None` when the user cancels.
///
/// # Arguments
///
/// * `config_json` - The default config, as a JSON string
/// * `label` - A label used to name the temporary edit file
/// * `open_editor` - Whether to open the config in the editor (interactive mode)
/// * `editor` - The editor command to open the config with
/// * `order` - The curated top-level key order the config is written in
/// * `validate` - Checks a config value, returning what the caller needs to write it
async fn fill_config<T, R>(
    config_json: &str,
    label: &str,
    open_editor: bool,
    editor: &str,
    order: &[&str],
    validate: impl Fn(&serde_json::Value) -> Result<R, Error>,
) -> Result<Option<(String, serde_json::Value, R)>, Error>
where
    T: DeserializeOwned,
{
    // parse the default so it can be validated and rendered in curated order
    let seed: serde_json::Value = serde_json::from_str(config_json)
        .map_err(|err| Error::new(format!("Invalid default config: {err}")))?;
    // validate the default up front so nothing the user edits is lost to a problem that
    // existed before the editor opened
    let checked = validate(&seed)?;
    // non-interactive: emit the default in curated key order with all fields present
    if !open_editor {
        let json = crate::utils::curated_json(&seed, order)?;
        return Ok(Some((json, seed, checked)));
    }
    // interactive: present the config as curated YAML, re-seeding from the user's own content
    // whenever a saved config fails validation and they choose to fix it
    let mut yaml = crate::utils::curated_yaml(&seed, order)?;
    loop {
        // the editor loop validates the text as `T`; None means the user cancelled
        let Some(edited) = editor::editor_loop_validated::<T>(&yaml, label, editor).await? else {
            return Ok(None);
        };
        // run the caller's checks on what was saved (names/group/references may have changed)
        match validate(&edited) {
            Ok(checked) => {
                let json = crate::utils::curated_json(&edited, order)?;
                return Ok(Some((json, edited, checked)));
            }
            Err(err) => {
                // explain the problem, then reopen with the user's content or cancel
                eprintln!("{} {err}", "Error:".bright_red().bold());
                if !prompt_reopen_editor()? {
                    return Ok(None);
                }
                yaml = crate::utils::curated_yaml(&edited, order)?;
            }
        }
    }
}

/// Read and validate a config's `name` and `group`, which are interpolated into the manifest
///
/// # Arguments
///
/// * `value` - The config value to read identity from
/// * `kind` - The resource kind, for error messages ("image" or "pipeline")
/// * `path` - The tool directory, for error messages
fn config_identity(
    value: &serde_json::Value,
    kind: &str,
    path: &Path,
) -> Result<(String, String), Error> {
    // both identity fields are required string fields
    let name = json_str_field(value, "name").ok_or_else(|| {
        Error::new(format!(
            "{kind} config for '{}' is missing a 'name' field",
            path.display()
        ))
    })?;
    let group = json_str_field(value, "group").ok_or_else(|| {
        Error::new(format!(
            "{kind} config for '{}' is missing a 'group' field",
            path.display()
        ))
    })?;
    // reject names that aren't valid identifiers before they are interpolated into the TOML
    // manifest template
    validate_resource_name(kind, &name)?;
    validate_resource_name("group", &group)?;
    Ok((name, group))
}

/// Writes an image's `manifest.toml`, `<name>.json`, and `description.md`
///
/// Skips up front when every file already exists, fills in and validates the config (see
/// [`fill_config`]), then writes the files keyed on the saved config's identity so the manifest
/// and filename stay consistent with whatever the user kept. Nothing is written on cancel.
///
/// # Arguments
///
/// * `path` - The image directory to write into
/// * `answers` - The wizard answers seeding the config
/// * `overwrite` - Overwrite existing files instead of skipping them
/// * `open_editor` - Open the config in the editor before writing (interactive mode)
/// * `editor` - The editor command to open the config with
/// * `toolbox` - When `-c` is set, the toolbox's `config.toml` path and its discovered images; a
///   saved name whose `latest` version already exists there is rejected (unless `overwrite`)
async fn write_image_files(
    path: &Path,
    answers: &ImageConfigAnswers,
    overwrite: bool,
    open_editor: bool,
    editor: &str,
    toolbox: Option<(&Path, &HashMap<String, Vec<String>>)>,
) -> Result<WriteOutcome, Error> {
    // don't open an editor whose changes would all be discarded by skipped writes
    if !precheck_targets(path, &answers.name, overwrite).await? {
        return Ok(WriteOutcome::Skipped);
    }
    // identity must be valid, and with -c the scaffolded `latest` must not duplicate an image
    // already in the toolbox (checked against the saved name, which the editor may change)
    let validate = |value: &serde_json::Value| {
        let (name, _group) = config_identity(value, "image", path)?;
        if let Some((config, available)) = toolbox
            && !overwrite
            && available
                .get(&name)
                .is_some_and(|versions| versions.iter().any(|version| version == "latest"))
        {
            return Err(Error::new(format!(
                "image '{name}:latest' already exists in the toolbox at '{}'; choose another name \
                 or pass --overwrite to replace it",
                config.display()
            )));
        }
        Ok(name)
    };
    // fill in the config (editor when interactive), validating before and after editing
    let filled = fill_config::<ImageRequest, String>(
        &build_image_config(answers),
        &format!("init-image-{}", answers.name),
        open_editor,
        editor,
        IMAGE_FIELD_ORDER,
        validate,
    )
    .await?;
    let Some((final_json, value, name)) = filled else {
        return Ok(WriteOutcome::Cancelled);
    };
    let manifest = generate_image_manifest(
        &name,
        // image_name may be a path (slashes), so escape it before it lands in the
        // `image_name = "…"` TOML template
        &toml_escape(&answers.image_name),
        "latest",
        answers.no_build,
        &[],
        // newly scaffolded images are built, not exported, so no pinned registry path
        None,
    );
    // write the manifest and the JSON config under the config's own name so the two stay
    // in lockstep with whatever identity the user saved
    write_file(&path.join("manifest.toml"), &manifest, overwrite).await?;
    write_file(&path.join(format!("{name}.json")), &final_json, overwrite).await?;
    // description.md is the source of truth toolbox build injects, so seed it from
    // the (possibly edited) config description; an empty description yields a bare stub
    let description = json_str_field(&value, "description").filter(|d| !d.is_empty());
    let description_md = description_stub(&name, description.as_deref());
    write_file(&path.join("description.md"), &description_md, overwrite).await?;
    Ok(WriteOutcome::Written)
}

/// Read a string field from a JSON config value, if present and a string
///
/// # Arguments
///
/// * `value` - The JSON config value to read from
/// * `field` - The name of the field to read
fn json_str_field(value: &serde_json::Value, field: &str) -> Option<String> {
    // None unless the key exists and holds a string; a missing key or non-string value
    // both collapse to None so callers can treat "absent" and "wrong type" alike
    value
        .get(field)
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// Collect the unique image names referenced across a pipeline config's `order`,
/// preserving first-seen order. Used to keep the manifest's image map in sync with
/// an order edited in the editor.
///
/// Both order forms are accepted: the flat form (`["a", "b"]`, a single implicit
/// stage) and the staged form (`[["a", "b"], ["c"]]`). A hand-edited pipeline config
/// can legitimately use the flat form, and silently treating it as no images would
/// leave the generated manifest's `[images.*]` map empty so the rebuilt pipeline
/// wouldn't declare the images it actually runs.
///
/// # Arguments
///
/// * `value` - The pipeline config value whose `order` is scanned for image names
fn unique_order_images(value: &serde_json::Value) -> Vec<String> {
    // `seen` dedupes while `images` preserves first-seen order, since a HashSet alone
    // would lose the ordering the manifest map should reflect
    let mut seen = std::collections::HashSet::new();
    let mut images = Vec::new();
    // record an image name the first time it appears; insert() is false on a repeat so
    // duplicates within or across stages are dropped while order is preserved
    let mut record = |name: &str| {
        if seen.insert(name.to_string()) {
            images.push(name.to_string());
        }
    };
    // a non-array order (or an absent one) simply yields no images rather than erroring
    if let Some(order) = value.get("order").and_then(|o| o.as_array()) {
        for entry in order {
            match entry {
                // a flat entry is itself an image name
                serde_json::Value::String(name) => record(name),
                // a staged entry is an array of image names; non-string members are skipped
                serde_json::Value::Array(stage) => {
                    for image in stage {
                        if let Some(name) = image.as_str() {
                            record(name);
                        }
                    }
                }
                // anything else is malformed; skip it rather than aborting
                _ => {}
            }
        }
    }
    images
}

/// Resolve the editor for an init subcommand
///
/// Uses the shared precedence (an explicit `--editor`, then a customized `default_editor`, then
/// `$VISUAL`, then `$EDITOR`, then the built-in default). Init is offline, so the config is
/// loaded best-effort; without one, the environment and built-in default still apply.
///
/// # Arguments
///
/// * `editor_override` - The explicit `--editor` value, if given
/// * `args` - The top-level thorctl args (used to locate the config)
fn resolve_editor(editor_override: Option<&str>, args: &Args) -> String {
    // with a usable config, defer to the shared resolution used by every other editor command
    if let Ok(conf) = thorium::CtlConf::from_path(&args.config) {
        return editor::resolve_editor(editor_override, &conf).to_string();
    }
    // an explicit --editor always wins
    if let Some(editor) = editor_override {
        return editor.to_string();
    }
    // no config: honor $VISUAL then $EDITOR, falling back to the built-in default
    ["VISUAL", "EDITOR"]
        .into_iter()
        .filter_map(|var| std::env::var(var).ok())
        .map(|value| value.trim().to_string())
        .find(|value| !value.is_empty())
        .unwrap_or_else(thorium::client::conf::default_default_editor)
}

/// Resolve the group for an init subcommand from `--group` or an interactive prompt
///
/// An explicit `--group` is validated here, before any editor opens, so a bad group can't throw
/// away a finished editing session.
///
/// # Arguments
///
/// * `group` - The explicit `--group` value, if given
/// * `interactive` - Whether the user can be prompted (errors instead of prompting when not)
fn resolve_group(group: Option<&str>, interactive: bool) -> Result<String, Error> {
    match group {
        // an explicit --group must be a valid group name
        Some(group) => {
            validate_resource_name("group", group)?;
            Ok(group.to_string())
        }
        // non-interactive can't prompt, so a missing group is a hard error rather than
        // silently defaulting to some group the user didn't choose
        None if !interactive => Err(Error::new(
            "--group is required in non-interactive mode".to_string(),
        )),
        // interactive: ask the user for the group (the prompt validates the name)
        None => prompt::prompt_group_name("Group name"),
    }
}

/// Build the starting contents for a scaffolded description.md
///
/// Seeds from the wizard's description answer when one was given so the
/// markdown file (the source of truth for `toolbox build`) starts in sync.
///
/// # Arguments
///
/// * `name` - The tool's name
/// * `description` - The description entered in the wizard, if any
fn description_stub(name: &str, description: Option<&str>) -> String {
    // treat a blank description as absent so an empty wizard answer doesn't add a stray
    // empty line under the heading
    match description.filter(|d| !d.is_empty()) {
        // an existing description goes under the Overview heading
        Some(description) => format!("# {name}\n\n# Overview\n\n{description}\n"),
        // no description: emit just the title and an empty Overview section for the user
        None => format!("# {name}\n\n# Overview\n"),
    }
}

/// Writes a pipeline's `manifest.toml`, `<name>.json`, and `description.md`
///
/// Skips up front when every file already exists, fills in and validates the config (see
/// [`fill_config`]), then derives the manifest's image map from the (possibly edited) config
/// order. Nothing is written on cancel.
///
/// # Arguments
///
/// * `path` - The pipeline directory to write into
/// * `answers` - The wizard answers seeding the config
/// * `overwrite` - Overwrite existing files instead of skipping them
/// * `open_editor` - Open the config in the editor before writing (interactive mode)
/// * `editor` - The editor command to open the config with
/// * `toolbox` - When `-c` is set, the toolbox's `config.toml` path and its discovered images;
///   every referenced image must exist there (else an error) and is version-pinned from it.
///   `None` pins each referenced image to `latest` (no validation source).
async fn write_pipeline_files(
    path: &Path,
    answers: &PipelineConfigAnswers,
    overwrite: bool,
    open_editor: bool,
    editor: &str,
    toolbox: Option<(&Path, &HashMap<String, Vec<String>>)>,
) -> Result<WriteOutcome, Error> {
    // don't open an editor whose changes would all be discarded by skipped writes
    if !precheck_targets(path, &answers.name, overwrite).await? {
        return Ok(WriteOutcome::Skipped);
    }
    // identity must be valid, and the manifest's image map is derived from the config's order so
    // editing the order keeps the manifest's referenced images in sync. With a toolbox (-c) each
    // image must exist there (init never creates images) and is pinned to the toolbox's version;
    // without one, each is pinned to "latest" since the order carries names only.
    let validate = |value: &serde_json::Value| {
        let (name, _group) = config_identity(value, "pipeline", path)?;
        let mut images = Vec::new();
        for image in unique_order_images(value) {
            // image names are interpolated into TOML table headers, so they must be valid too
            validate_resource_name("image", &image)?;
            let version = match toolbox {
                Some((config, available)) => match available.get(&image) {
                    Some(versions) => pin_version(versions),
                    None => {
                        return Err(Error::new(format!(
                            "pipeline references image '{image}', which is not in the toolbox at \
                             '{}'; add it (e.g. with `thorctl toolbox init image`) before \
                             building — init does not create it",
                            config.display()
                        )));
                    }
                },
                None => default_version(),
            };
            images.push((image, version));
        }
        Ok((name, images))
    };
    // fill in the config (editor when interactive), validating before and after editing
    let filled = fill_config::<PipelineRequest, (String, Vec<(String, String)>)>(
        &build_pipeline_config(answers),
        &format!("init-pipeline-{}", answers.name),
        open_editor,
        editor,
        PIPELINE_FIELD_ORDER,
        validate,
    )
    .await?;
    let Some((final_json, value, (name, images))) = filled else {
        return Ok(WriteOutcome::Cancelled);
    };
    // write the manifest and JSON config keyed on the saved name so they stay in lockstep
    let manifest = generate_pipeline_manifest(&name, &images);
    write_file(&path.join("manifest.toml"), &manifest, overwrite).await?;
    write_file(&path.join(format!("{name}.json")), &final_json, overwrite).await?;
    // seed description.md from the (possibly edited) config description, blank treated as absent
    let description = json_str_field(&value, "description").filter(|d| !d.is_empty());
    let description_md = description_stub(&name, description.as_deref());
    write_file(&path.join("description.md"), &description_md, overwrite).await?;
    Ok(WriteOutcome::Written)
}

/// Parse a `--order` value into pipeline stages
///
/// Accepts the same shapes the editor path does: the staged form (`[["a", "b"], ["c"]]`) and
/// the flat form (`["a", "b"]`, a single stage).
///
/// # Arguments
///
/// * `order` - The raw `--order` JSON
fn parse_order_arg(order: &str) -> Result<Vec<Vec<String>>, Error> {
    // the shape error shared by every malformed case
    let shape_err = || {
        Error::new(
            "Invalid --order: expected a JSON array of stages (e.g. [[\"a\",\"b\"],[\"c\"]]) or \
             of image names (e.g. [\"a\",\"b\"])"
                .to_string(),
        )
    };
    // parse the raw JSON first so a syntax error is reported as such
    let value: serde_json::Value = serde_json::from_str(order)
        .map_err(|err| Error::new(format!("Invalid --order JSON: {err}")))?;
    let serde_json::Value::Array(entries) = value else {
        return Err(shape_err());
    };
    // an empty order has no stages
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    // the flat form is one stage holding every listed image
    if entries.iter().all(serde_json::Value::is_string) {
        let stage = entries
            .iter()
            .filter_map(|entry| entry.as_str().map(String::from))
            .collect();
        return Ok(vec![stage]);
    }
    // the staged form: every entry must be an array of image-name strings
    entries
        .iter()
        .map(|stage| {
            stage
                .as_array()
                .ok_or_else(shape_err)?
                .iter()
                .map(|image| image.as_str().map(String::from).ok_or_else(shape_err))
                .collect()
        })
        .collect()
}

/// Reject a list of tool names containing duplicates
///
/// Two tools with the same name scaffold the same `name:latest` identity, which `toolbox build`
/// rejects later; catching it here fails before anything is written.
///
/// # Arguments
///
/// * `kind` - The resource kind, for the error message ("image" or "pipeline")
/// * `names` - The tool names to check
fn ensure_unique_names(kind: &str, names: &[String]) -> Result<(), Error> {
    // collect every name seen more than once, sorted for a deterministic message
    let mut seen = HashSet::new();
    let mut duplicates: Vec<&str> = names
        .iter()
        .filter(|name| !seen.insert(name.as_str()))
        .map(String::as_str)
        .collect();
    duplicates.sort_unstable();
    duplicates.dedup();
    if duplicates.is_empty() {
        Ok(())
    } else {
        Err(Error::new(format!(
            "duplicate {kind} name(s): {}; each {kind} directory must have a unique name",
            duplicates.join(", ")
        )))
    }
}

/// Resolve a tool directory given to `init toolbox` against the toolbox root
///
/// Relative paths are placed under `--toolbox-dir` so the tools land in the toolbox that
/// `toolbox build` will crawl; absolute paths are used as given.
///
/// # Arguments
///
/// * `toolbox_dir` - The toolbox root (`--toolbox-dir`)
/// * `path` - The tool directory as given on the command line
fn resolve_under_toolbox(toolbox_dir: &Path, path: &Path) -> PathBuf {
    // an absolute path, or a toolbox root of the cwd, needs no joining
    if path.is_absolute() || toolbox_dir == Path::new(".") {
        path.to_path_buf()
    } else {
        toolbox_dir.join(path)
    }
}

/// Report the outcome of scaffolding a single image or pipeline
///
/// # Arguments
///
/// * `kind` - The resource kind ("image" or "pipeline")
/// * `path` - The tool directory that was scaffolded
/// * `outcome` - What happened
fn report_single_outcome(kind: &str, path: &Path, outcome: WriteOutcome) -> Result<(), Error> {
    match outcome {
        // point the user at the real next step: build discovers every manifest.toml under the
        // toolbox root, so there's nothing to register in config.toml
        WriteOutcome::Written => {
            println!(
                "\n{} Keep '{}' under a toolbox root (the directory holding config.toml) and run \
                 {} there; build picks up every manifest.toml beneath the root",
                "Init complete!".bright_green(),
                path.display(),
                "thorctl toolbox build".bright_cyan()
            );
            Ok(())
        }
        // everything already existed; the skip line was printed by the precheck
        WriteOutcome::Skipped => {
            println!(
                "\n{} nothing was written for the {kind} at '{}'",
                "Init complete!".bright_yellow(),
                path.display()
            );
            Ok(())
        }
        // a cancel writes nothing, which the exit status should reflect
        WriteOutcome::Cancelled => Err(Error::new(format!(
            "Init stopped early: the {kind} config was cancelled in the editor, so nothing was \
             written to '{}'",
            path.display()
        ))),
    }
}

// ─── Subcommand Dispatch ─────────────────────────────────────────────────────

/// Dispatches an `init` subcommand to its scaffolding handler
///
/// # Arguments
///
/// * `cmd` - The init subcommand (toolbox, image, or pipeline)
/// * `args` - The top-level thorctl args
pub async fn handle(cmd: &Init, args: &Args) -> Result<(), Error> {
    // route each init variant to its dedicated scaffolder
    match cmd {
        Init::Toolbox(cmd) => init_toolbox(cmd, args).await,
        Init::Image(cmd) => init_image(cmd, args).await,
        Init::Pipeline(cmd) => init_pipeline(cmd, args).await,
    }
}

/// Scaffolds a single image directory from `init image` args
///
/// # Arguments
///
/// * `cmd` - The `init image` args
/// * `args` - The top-level thorctl args
async fn init_image(cmd: &InitImage, args: &Args) -> Result<(), Error> {
    // decide once whether prompts and the editor are available
    let interactive = resolve_interactive(cmd.non_interactive, "--group")?;
    // the tool name defaults to the target directory's basename, validated before any editing
    let name = resolve_tool_name("image", &cmd.path, interactive)?;
    // the manifest image_name defaults to the tool name, overridable via --image-name
    let default_image_name = cmd.image_name.clone().unwrap_or_else(|| name.clone());
    // resolve and validate the group up front (prompt or --group) so it seeds the config answers
    let group = resolve_group(cmd.group.as_deref(), interactive)?;
    // with -c, discover the toolbox's images so a duplicate name+version is caught before writing
    // (unless --overwrite). `-c` is a resolution source only — it never moves files.
    let toolbox_images = match &cmd.config {
        Some(config) => Some((config.as_path(), collect_toolbox_images(config)?)),
        None => None,
    };
    let toolbox = toolbox_images
        .as_ref()
        .map(|(config, images)| (*config, images));
    // seed the wizard answers from the resolved defaults; no_build flows into build=false
    let answers = ImageConfigAnswers::defaults(&name, &group, cmd.no_build, &default_image_name);
    // resolve the editor; it is only used in interactive mode but resolving it has no side effects
    let editor = resolve_editor(cmd.editor.as_deref(), args);
    // fill in, validate, and write the image's files
    let outcome = write_image_files(
        &cmd.path,
        &answers,
        cmd.overwrite,
        interactive,
        &editor,
        toolbox,
    )
    .await?;
    // tell the user what happened and what to do next
    report_single_outcome("image", &cmd.path, outcome)
}

/// Scaffolds a single pipeline directory from `init pipeline` args
///
/// # Arguments
///
/// * `cmd` - The `init pipeline` args
/// * `args` - The top-level thorctl args
async fn init_pipeline(cmd: &InitPipeline, args: &Args) -> Result<(), Error> {
    // decide once whether prompts and the editor are available
    let interactive = resolve_interactive(cmd.non_interactive, "--group and --images")?;
    // the pipeline name defaults to the target directory's basename, validated before any editing
    let name = resolve_tool_name("pipeline", &cmd.path, interactive)?;
    // resolve and validate the group (prompt or --group) before building the answers
    let group = resolve_group(cmd.group.as_deref(), interactive)?;
    // non-interactive can't prompt for images and the default order is built from them,
    // so an empty --images would scaffold an empty pipeline — reject it instead
    if !interactive && cmd.images.is_empty() {
        return Err(Error::new(
            "--images is required in non-interactive mode".to_string(),
        ));
    }
    // defaults put every --images entry into a single parallel stage as the order
    let mut answers = PipelineConfigAnswers::defaults(&name, &group, &cmd.images);
    // an explicit --order replaces that default ordering
    if let Some(order_str) = &cmd.order {
        answers.order = parse_order_arg(order_str)?;
        // flatten collapses the staged order to the set of all named images
        let ordered: HashSet<&str> = answers.order.iter().flatten().map(String::as_str).collect();
        let provided: HashSet<&str> = cmd.images.iter().map(String::as_str).collect();
        // every ordered image must be declared in --images, so the manifest never carries a
        // dangling, version-less image entry; a stray order entry is a hard error
        let mut unlisted: Vec<&str> = ordered.difference(&provided).copied().collect();
        if !unlisted.is_empty() {
            unlisted.sort_unstable();
            return Err(Error::new(format!(
                "--order references image(s) not in --images: {}; add them to --images (a pipeline \
                 may only reference images it declares)",
                unlisted.join(", ")
            )));
        }
        // an --images entry missing from the order never reaches the manifest, so say so
        let mut unordered: Vec<&str> = provided.difference(&ordered).copied().collect();
        if !unordered.is_empty() {
            unordered.sort_unstable();
            progress::warn(format!(
                "--images entries not in --order are left out of the pipeline: {}",
                unordered.join(", ")
            ));
        }
    }
    // with -c, the toolbox is the resolution source: referenced images must exist in it (else an
    // error) and are version-pinned from it. Walk it once up front and announce the source.
    let toolbox_images = match &cmd.config {
        Some(config) => {
            println!(
                "Resolving pipeline images against toolbox '{}'",
                config.display()
            );
            Some((config.as_path(), collect_toolbox_images(config)?))
        }
        None => None,
    };
    let toolbox = toolbox_images
        .as_ref()
        .map(|(config, images)| (*config, images));
    // resolve the editor; it is only used in interactive mode but resolving it has no side effects
    let editor = resolve_editor(cmd.editor.as_deref(), args);
    // fill in, validate, and write the pipeline's files
    let outcome = write_pipeline_files(
        &cmd.path,
        &answers,
        cmd.overwrite,
        interactive,
        &editor,
        toolbox,
    )
    .await?;
    // tell the user what happened and what to do next
    report_single_outcome("pipeline", &cmd.path, outcome)
}

/// Render the contents of a new toolbox `config.toml` for `init toolbox`
///
/// Seeds from a `--config` template (whose export paths must be safe relative subpaths), or
/// from `--name`/`--registry` (prompting for them in interactive mode).
///
/// # Arguments
///
/// * `cmd` - The `init toolbox` args
/// * `interactive` - Whether the user can be prompted
fn toolbox_config_contents(cmd: &InitToolbox, interactive: bool) -> Result<String, Error> {
    // seed the new toolbox from an existing config.toml (mutually exclusive with
    // --name/--registry); carries name, registry, registries, image_path_prefix,
    // export paths, bundled_images, and base_image forward verbatim
    if let Some(template_path) = &cmd.config {
        let template = load_config(template_path)?;
        // the template's export paths get the same containment check as the CLI flags
        if let Some(path) = &template.export_image_path {
            validate_relative_subpath(
                &format!("export_image_path in '{}'", template_path.display()),
                path,
            )?;
        }
        if let Some(path) = &template.export_pipeline_path {
            validate_relative_subpath(
                &format!("export_pipeline_path in '{}'", template_path.display()),
                path,
            )?;
        }
        return Ok(render_config_toml(
            &template.name,
            template.registry.as_deref(),
            &template.registries,
            template.image_path_prefix.as_deref(),
            template.export_image_path.as_deref(),
            template.export_pipeline_path.as_deref(),
            template.bundled_images,
            template.base_image.as_ref(),
        ));
    }
    // no --config: take name/registry from flags non-interactively, else prompt for them
    let (name, registry) = if interactive {
        let answers = prompt::prompt_toolbox_config(&cmd.name, cmd.registry.as_deref())?;
        (answers.name, answers.registry)
    } else {
        (cmd.name.clone(), cmd.registry.clone())
    };
    // a from-scratch config has no extra registries, prefix, bundling, or base image; the
    // export-layout dirs come from --image-path/--pipeline-path (commented defaults when unset)
    Ok(render_config_toml(
        &name,
        registry.as_deref(),
        &[],
        None,
        cmd.image_path.as_deref(),
        cmd.pipeline_path.as_deref(),
        false,
        None,
    ))
}

/// Write the toolbox's `config.toml` for `init toolbox`, or keep an existing one
///
/// `config.toml` is sticky: an existing one is preserved unless `--overwrite-config`, so
/// re-running init in a toolbox doesn't clobber its settings (per-tool files use `--overwrite`).
/// The contents are only built (and prompted for) when they will actually be written. Returns
/// the config's path.
///
/// # Arguments
///
/// * `cmd` - The `init toolbox` args
/// * `interactive` - Whether the user can be prompted
async fn write_toolbox_config(cmd: &InitToolbox, interactive: bool) -> Result<PathBuf, Error> {
    // the config lives at the toolbox root
    let config_path = cmd.toolbox_dir.join("config.toml");
    let config_exists = tokio::fs::try_exists(&config_path)
        .await
        .map_err(|err| Error::new(format!("Failed to stat '{}': {err}", config_path.display())))?;
    if config_exists && !cmd.overwrite_config {
        println!(
            "{} {} (already exists; pass --overwrite-config to replace)",
            "Skipped".bright_yellow(),
            config_path.display()
        );
        // warn that the settings flags are ignored while the existing config is kept
        if cmd.config.is_some()
            || cmd.registry.is_some()
            || cmd.image_path.is_some()
            || cmd.pipeline_path.is_some()
        {
            progress::warn(
                "keeping the existing config.toml; --config/--registry/--image-path/\
                 --pipeline-path are ignored (pass --overwrite-config to apply them)",
            );
        }
    } else {
        // only build (and prompt for) the config contents when they will actually be written;
        // the write is forced since the sticky check above already governs whether we get here
        let config_toml = toolbox_config_contents(cmd, interactive)?;
        write_file(&config_path, &config_toml, true).await?;
    }
    Ok(config_path)
}

/// Warn when a colon-bound `-p` pipeline binds images the toolbox isn't scaffolding
///
/// Such a pipeline references an image that won't exist in this toolbox; this warns rather than
/// fails so the user can wire it up themselves. Only the explicit colon-bound case can name a
/// stray image.
///
/// # Arguments
///
/// * `spec` - The parsed `-p` pipeline spec
/// * `pipeline_name` - The pipeline's name, for the warning
/// * `image_names` - The names of the images the toolbox is scaffolding
fn warn_unbound_images(spec: &PipelineSpec, pipeline_name: &str, image_names: &[String]) {
    // without a colon binding the pipeline runs every scaffolded image, so nothing can be stray
    let Some(bound) = &spec.images else {
        return;
    };
    // bound images with no matching --images entry, sorted so the warning is deterministic
    let mut unlisted: Vec<&str> = bound
        .iter()
        .filter(|image| !image_names.contains(image))
        .map(String::as_str)
        .collect();
    unlisted.sort_unstable();
    if !unlisted.is_empty() {
        progress::warn(format!(
            "pipeline '{pipeline_name}' binds image(s) not in --images: {}",
            unlisted.join(", ")
        ));
    }
}

/// Scaffolds a full toolbox directory (config.toml plus image/pipeline subdirs)
///
/// Every argument is validated and every prompt answered before anything is written, so a bad
/// name, group, or path can't leave a half-initialized toolbox behind. Relative `-i`/`-p` paths
/// are placed under `--toolbox-dir`.
///
/// # Arguments
///
/// * `cmd` - The `init toolbox` args
/// * `args` - The top-level thorctl args
async fn init_toolbox(cmd: &InitToolbox, args: &Args) -> Result<(), Error> {
    // decide once whether prompts and the editor are available
    let interactive = resolve_interactive(cmd.non_interactive, "--group")?;
    // place relative image dirs under the toolbox root so build's crawl finds them
    let image_paths: Vec<PathBuf> = cmd
        .images
        .iter()
        .map(|path| resolve_under_toolbox(&cmd.toolbox_dir, path))
        .collect();
    // parse each --pipeline string into its path (placed under the toolbox root) and optional
    // colon-bound image list
    let pipeline_specs: Vec<PipelineSpec> = cmd
        .pipelines
        .iter()
        .map(|spec| {
            let mut spec = PipelineSpec::parse(spec);
            spec.path = resolve_under_toolbox(&cmd.toolbox_dir, &spec.path);
            spec
        })
        .collect();
    // derive and validate each tool's name from its directory basename before anything is
    // written; the image names double as the default pipeline binding
    let image_names: Vec<String> = image_paths
        .iter()
        .map(|path| resolve_tool_name("image", path, interactive))
        .collect::<Result<_, _>>()?;
    let pipeline_names: Vec<String> = pipeline_specs
        .iter()
        .map(|spec| resolve_tool_name("pipeline", &spec.path, interactive))
        .collect::<Result<_, _>>()?;
    // two tools with the same name would collide at build time, so reject them now
    ensure_unique_names("image", &image_names)?;
    ensure_unique_names("pipeline", &pipeline_names)?;
    // colon-bound image names are interpolated into the pipeline manifest, so validate them too
    for image in pipeline_specs
        .iter()
        .filter_map(|spec| spec.images.as_ref())
        .flatten()
    {
        validate_resource_name("image", image)?;
    }
    // the export-layout dirs are written into config.toml and later used to place files, so
    // reject anything that would escape the toolbox root
    if let Some(path) = &cmd.image_path {
        validate_relative_subpath("--image-path", path)?;
    }
    if let Some(path) = &cmd.pipeline_path {
        validate_relative_subpath("--pipeline-path", path)?;
    }
    // one group is resolved (and validated) once and shared by every scaffolded tool
    let group = resolve_group(cmd.group.as_deref(), interactive)?;
    // write (or keep) the toolbox's config.toml now that every argument and prompt is settled
    let config_path = write_toolbox_config(cmd, interactive).await?;
    // the editor is resolved once and threaded into each write
    let editor = resolve_editor(cmd.editor.as_deref(), args);
    // tools the user cancelled in the editor; the rest of the toolbox is still scaffolded
    let mut cancelled: Vec<String> = Vec::new();
    // scaffold each image dir, pairing its resolved path with its tool name
    for (image_path, image_name) in image_paths.iter().zip(&image_names) {
        // the manifest image_name defaults to the image's name (matching `init image`)
        let answers = ImageConfigAnswers::defaults(image_name, &group, false, image_name);
        let outcome = write_image_files(
            image_path,
            &answers,
            cmd.overwrite,
            interactive,
            &editor,
            None,
        )
        .await?;
        if outcome == WriteOutcome::Cancelled {
            cancelled.push(image_path.display().to_string());
        }
    }
    for (spec, pipeline_name) in pipeline_specs.iter().zip(&pipeline_names) {
        // colon-bound images from the spec take precedence; with no colon the pipeline
        // runs every image the toolbox is scaffolding
        let pipeline_images = spec.images.clone().unwrap_or_else(|| image_names.clone());
        // flag colon-bound images the toolbox isn't scaffolding
        warn_unbound_images(spec, pipeline_name, &image_names);
        // seed the pipeline's order from its bound images and scaffold its dir. No toolbox
        // resolution source here: init toolbox scaffolds the images itself in the same run, so the
        // pipeline's references pin "latest" (the scaffolded images' version).
        let answers = PipelineConfigAnswers::defaults(pipeline_name, &group, &pipeline_images);
        let outcome = write_pipeline_files(
            &spec.path,
            &answers,
            cmd.overwrite,
            interactive,
            &editor,
            None,
        )
        .await?;
        if outcome == WriteOutcome::Cancelled {
            cancelled.push(spec.path.display().to_string());
        }
    }
    // a cancelled tool wasn't written, so the run didn't fully succeed
    if !cancelled.is_empty() {
        return Err(Error::new(format!(
            "Init finished with errors: {} tool(s) were cancelled in the editor and not written: {}",
            cancelled.len(),
            cancelled.join(", ")
        )));
    }
    // point the user at the next step now that the whole toolbox skeleton exists; outside the
    // cwd, build needs -c to find the new config.toml
    let build_cmd = if cmd.toolbox_dir == Path::new(".") {
        "thorctl toolbox build".to_string()
    } else {
        format!("thorctl toolbox build -c {}", config_path.display())
    };
    println!(
        "\n{} Run {} to produce a toolbox.json",
        "Init complete!".bright_green(),
        build_cmd.bright_cyan()
    );
    Ok(())
}

/// Unit tests for the pure rendering helpers (config TOML, default configs, and the
/// description stub) that don't need filesystem or editor interaction
#[cfg(test)]
mod tests {
    use super::prompt::{ImageConfigAnswers, PipelineConfigAnswers};
    use super::{
        BaseImage, build_image_config, build_pipeline_config, description_stub,
        generate_image_manifest, generate_pipeline_manifest, no_terminal_message,
        render_config_toml, unique_order_images,
    };

    /// The no-terminal error names the flags the subcommand needs with -n
    #[test]
    fn no_terminal_message_names_required_flags() {
        let msg = no_terminal_message("--group and --images");
        assert!(msg.contains("(with --group and --images)"));
        assert!(msg.starts_with("toolbox init is interactive and needs a terminal"));
    }

    /// The scaffolded description.md is just the tool name + an Overview section (no
    /// placeholder prose); an existing description is placed under Overview
    #[test]
    fn description_stub_uses_overview_section() {
        // an absent description yields just the title and an empty Overview, no placeholder prose
        let empty = description_stub("clamav", None);
        assert_eq!(empty, "# clamav\n\n# Overview\n");
        assert!(!empty.contains("Describe what this tool"));
        // a present description is placed under the Overview heading
        let with_desc = description_stub("clamav", Some("scans files"));
        assert_eq!(with_desc, "# clamav\n\n# Overview\n\nscans files\n");
    }

    /// The scaffolded image default must deserialize into the real `ImageRequest`
    /// (the type `toolbox import` and the editor validation parse it as) and include
    /// the `version`/`lifetime`/`modifiers` fields so the saved config is complete
    #[test]
    fn image_template_deserializes_into_request() {
        // the scaffolded default must parse as the real ImageRequest the importer/editor use
        let answers = ImageConfigAnswers::defaults("clamav", "static", false, "clamav");
        let json = build_image_config(&answers);
        serde_json::from_str::<thorium::models::ImageRequest>(&json)
            .expect("default image config must deserialize into ImageRequest");
        // re-parse as untyped JSON to assert the explicitly-null fields are present, since
        // ImageRequest deserialization alone wouldn't catch a dropped key
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        for key in ["version", "lifetime", "modifiers"] {
            assert!(value.get(key).is_some(), "image template missing '{key}'");
        }
    }

    /// The scaffolded pipeline default must deserialize into the real `PipelineRequest`
    #[test]
    fn pipeline_template_deserializes_into_request() {
        // the scaffolded default must parse as the real PipelineRequest the importer/editor use
        let answers = PipelineConfigAnswers::defaults("triage", "static", &["clamav".to_string()]);
        let json = build_pipeline_config(&answers);
        serde_json::from_str::<thorium::models::PipelineRequest>(&json)
            .expect("default pipeline config must deserialize into PipelineRequest");
    }

    /// Version pinning for a referenced toolbox image prefers `latest`, else the first discovered
    #[test]
    fn pin_version_prefers_latest() {
        use super::pin_version;
        // latest wins even when listed after another version
        assert_eq!(
            pin_version(&["1.0".to_string(), "latest".to_string()]),
            "latest"
        );
        // with no latest, the first discovered version is pinned
        assert_eq!(pin_version(&["2.1".to_string(), "2.0".to_string()]), "2.1");
        // an empty set falls back to latest (defensive; callers pass non-empty)
        assert_eq!(pin_version(&[]), "latest");
    }

    /// The order is scanned for image names in both the staged and the flat form, so a
    /// hand-edited pipeline config using either shape keeps the generated manifest's
    /// image map populated (first-seen order, duplicates dropped)
    #[test]
    fn unique_order_images_accepts_flat_and_staged() {
        use serde_json::json;
        // the staged form: an array of stages
        let staged = json!({ "order": [["a", "b"], ["a", "c"]] });
        assert_eq!(unique_order_images(&staged), vec!["a", "b", "c"]);
        // the flat form: a single implicit stage of image names
        let flat = json!({ "order": ["a", "b", "a", "c"] });
        assert_eq!(unique_order_images(&flat), vec!["a", "b", "c"]);
        // an absent or non-array order yields no images rather than erroring
        assert!(unique_order_images(&json!({})).is_empty());
        assert!(unique_order_images(&json!({ "order": "nope" })).is_empty());
    }

    /// A free-form version carrying TOML metacharacters (a Custom version captured on
    /// export) must be escaped so the generated image manifest stays valid TOML and the
    /// version round-trips intact rather than breaking out into injected keys
    #[test]
    fn image_manifest_escapes_version() {
        // a version with an embedded quote and newline plus an injected key assignment
        let nasty = "1.0\"\nmalicious = \"pwned";
        let manifest = generate_image_manifest("clamav", "clamav", nasty, false, &[], None);
        // the whole manifest must still parse as TOML
        let parsed: toml::Value =
            toml::from_str(&manifest).expect("escaped image manifest must be valid TOML");
        // the version decodes back to exactly the original string
        assert_eq!(parsed["version"].as_str(), Some(nasty));
        // the injected assignment never became a real top-level key
        assert!(parsed.get("malicious").is_none());
    }

    /// A bare-key-safe name is emitted unquoted while a name with a special character (a dot, which
    /// would otherwise parse as a nested table) falls back to a quoted/escaped key; the version is
    /// always escaped — so the image map stays valid and faithful to the referenced (name, version)
    /// pairs
    #[test]
    fn pipeline_manifest_quotes_only_when_needed() {
        // a normal dashed name is bare-key-safe; a dotted name is not (it would nest as a bare key);
        // the dotted entry's version carries an embedded quote/newline and an injected assignment
        let images = vec![
            ("detect-it-easy".to_string(), "latest".to_string()),
            ("clam.av".to_string(), "1\"\nx = \"y".to_string()),
        ];
        let manifest = generate_pipeline_manifest("triage", &images);
        // the dashed name is written bare; only the dotted name is quoted
        assert!(manifest.contains("[images.detect-it-easy]"));
        assert!(manifest.contains("[images.\"clam.av\"]"));
        // the whole manifest must still parse as TOML
        let parsed: toml::Value =
            toml::from_str(&manifest).expect("escaped pipeline manifest must be valid TOML");
        let imgs = parsed["images"].as_table().expect("images must be a table");
        // the bare name resolves to a single key with its version
        assert_eq!(imgs["detect-it-easy"]["version"].as_str(), Some("latest"));
        // the dotted name is a single key, not a nested images.clam.av sub-table
        assert!(imgs.contains_key("clam.av"));
        assert_eq!(imgs["clam.av"]["version"].as_str(), Some("1\"\nx = \"y"));
        // the injected assignment never escaped into the images table
        assert!(imgs.get("x").is_none());
    }

    /// `toml_key` emits a bare key for bare-key-safe names and a quoted/escaped key otherwise
    #[test]
    fn toml_key_quotes_only_non_bare_names() {
        // ascii alphanumerics, '-', and '_' are valid TOML bare-key characters
        assert_eq!(super::toml_key("detect-it-easy"), "detect-it-easy");
        assert_eq!(super::toml_key("under_score"), "under_score");
        assert_eq!(super::toml_key("Mixed123"), "Mixed123");
        // a dot, a space, or an embedded quote forces a quoted, escaped key
        assert_eq!(super::toml_key("clam.av"), "\"clam.av\"");
        assert_eq!(super::toml_key("two words"), "\"two words\"");
        assert_eq!(super::toml_key("a\"b"), "\"a\\\"b\"");
        // an empty name is never a valid bare key
        assert_eq!(super::toml_key(""), "\"\"");
    }

    /// With no extras, registries and image_path_prefix are emitted as commented
    /// placeholders and bundled_images is omitted
    #[test]
    fn render_config_minimal() {
        // a name + registry with no extras: registries/prefix become commented placeholders
        let toml = render_config_toml(
            "My TB",
            Some("ghcr.io/o/r"),
            &[],
            None,
            None,
            None,
            false,
            None,
        );
        assert!(toml.contains("name = \"My TB\""));
        assert!(toml.contains("registry = \"ghcr.io/o/r\""));
        assert!(toml.contains("# registries = []"));
        assert!(toml.contains("# image_path_prefix = \"\""));
        // unset export-layout dirs are commented placeholders documenting the defaults
        assert!(toml.contains("# export_image_path = \"images\""));
        assert!(toml.contains("# export_pipeline_path = \"pipelines\""));
        assert!(!toml.contains("bundled_images"));
        // an unset base image is a commented placeholder
        assert!(toml.contains("# [base_image]"));
    }

    /// An unset registry is emitted as a commented placeholder, not `registry = ""`
    #[test]
    fn render_config_no_registry() {
        // an unset registry must be a commented placeholder, never an active empty value
        let toml = render_config_toml("My TB", None, &[], None, None, None, false, None);
        assert!(toml.contains("# registry = \"\""));
        // the active (uncommented) registry line must not be present
        assert!(!toml.contains("\nregistry = "));
    }

    /// Extra registries, an image_path_prefix, bundled_images, and a base image are
    /// written out
    #[test]
    fn render_config_full() {
        // every optional knob set: each must be written out as an active key, not a placeholder
        let base = BaseImage {
            image: Some("ubuntu:22.04".to_string()),
            image_arg: Some("IMAGE".to_string()),
            token: Some("BASE_TOKEN".to_string()),
            user: Some("BASE_USER".to_string()),
            allow_override: Some(true),
        };
        let toml = render_config_toml(
            "TB",
            Some("reg"),
            &["reg".to_string(), "reg2".to_string()],
            Some("prefix/path"),
            Some("tools/images"),
            Some("tools/pipelines"),
            true,
            Some(&base),
        );
        assert!(toml.contains("registries = [\"reg\", \"reg2\"]"));
        assert!(toml.contains("bundled_images = true"));
        assert!(toml.contains("image_path_prefix = \"prefix/path\""));
        assert!(!toml.contains("# image_path_prefix"));
        // the export-layout dirs are written out as active keys, not placeholders
        assert!(toml.contains("export_image_path = \"tools/images\""));
        assert!(toml.contains("export_pipeline_path = \"tools/pipelines\""));
        assert!(!toml.contains("# export_image_path"));
        // the base image table is written out, not a commented placeholder
        assert!(toml.contains("[base_image]"));
        assert!(toml.contains("image = \"ubuntu:22.04\""));
        assert!(toml.contains("image_arg = \"IMAGE\""));
        assert!(toml.contains("token = \"BASE_TOKEN\""));
        assert!(toml.contains("user = \"BASE_USER\""));
        assert!(toml.contains("allow_override = true"));
        assert!(!toml.contains("# [base_image]"));
    }

    /// Free-form values with quotes are escaped so the TOML stays valid
    #[test]
    fn render_config_escapes() {
        // a quote in the free-form name must be backslash-escaped so the TOML stays valid
        let toml = render_config_toml("a\"b", Some("r"), &[], None, None, None, false, None);
        assert!(toml.contains("name = \"a\\\"b\""));
    }

    /// `dir_name` takes a literal trailing component, and resolves `.`/`..` to the directory they
    /// name instead of failing
    #[test]
    fn dir_name_resolves_dot_paths() {
        use super::dir_name;
        use std::path::Path;
        // a literal trailing component is used directly, even when the path doesn't exist
        assert_eq!(dir_name(Path::new("images/clamav")).unwrap(), "clamav");
        assert_eq!(dir_name(Path::new("/no/such/dir/yara")).unwrap(), "yara");
        // a trailing `..` on a missing path is normalized lexically
        assert_eq!(dir_name(Path::new("/no/such/a/b/..")).unwrap(), "a");
        // `.` resolves to the current directory's own name
        let cwd = std::env::current_dir().unwrap();
        let expected = cwd.file_name().unwrap().to_str().unwrap();
        assert_eq!(dir_name(Path::new(".")).unwrap(), expected);
        // the filesystem root has no name to take
        assert!(dir_name(Path::new("/")).is_err());
    }

    /// `validate_relative_subpath` accepts plain relative subpaths and rejects empty, absolute,
    /// rooted, drive-prefixed, and escaping paths in both separator styles
    #[test]
    fn validate_relative_subpath_rejects_escapes() {
        use super::validate_relative_subpath;
        // plain relative subpaths (including a leading `./`) are fine
        for ok in ["images", "tools/images", "./images", "a/./b"] {
            assert!(
                validate_relative_subpath("--image-path", ok).is_ok(),
                "{ok}"
            );
        }
        // empty, absolute, Windows-rooted, drive-relative, and `..` paths all escape the root
        for bad in [
            "",
            " ",
            "/abs",
            "\\images",
            "\\\\server\\share",
            "C:foo",
            "C:\\foo",
            "c:/foo",
            "..",
            "../x",
            "a/../../x",
            "..\\x",
            "a\\..\\..",
        ] {
            assert!(
                validate_relative_subpath("--image-path", bad).is_err(),
                "{bad}"
            );
        }
    }

    /// `--order` accepts both the staged and the flat form and rejects other shapes
    #[test]
    fn parse_order_arg_accepts_flat_and_staged() {
        use super::parse_order_arg;
        // the staged form is kept stage by stage
        assert_eq!(
            parse_order_arg(r#"[["a","b"],["c"]]"#).unwrap(),
            vec![
                vec!["a".to_string(), "b".to_string()],
                vec!["c".to_string()]
            ]
        );
        // the flat form becomes a single stage
        assert_eq!(
            parse_order_arg(r#"["a","b"]"#).unwrap(),
            vec![vec!["a".to_string(), "b".to_string()]]
        );
        // an empty order has no stages
        assert_eq!(parse_order_arg("[]").unwrap(), Vec::<Vec<String>>::new());
        // non-arrays, mixed shapes, non-string names, and bad JSON are rejected
        for bad in [r#""a""#, r#"["a",["b"]]"#, "[[1]]", "[[", "{}"] {
            assert!(parse_order_arg(bad).is_err(), "{bad}");
        }
    }

    /// Duplicate tool names are reported once each; unique names pass
    #[test]
    fn ensure_unique_names_reports_duplicates() {
        use super::ensure_unique_names;
        let names = |list: &[&str]| list.iter().map(ToString::to_string).collect::<Vec<_>>();
        // unique names pass
        assert!(ensure_unique_names("image", &names(&["a", "b"])).is_ok());
        // a repeated name is an error naming the duplicate once
        let err = ensure_unique_names("image", &names(&["a", "b", "a", "a"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("duplicate image name(s): a;"), "{err}");
    }

    /// Relative tool dirs are placed under `--toolbox-dir`; absolute dirs and a cwd root are not
    #[test]
    fn resolve_under_toolbox_joins_relative_paths() {
        use super::resolve_under_toolbox;
        use std::path::{Path, PathBuf};
        // a relative path lands under a non-cwd toolbox root
        assert_eq!(
            resolve_under_toolbox(Path::new("tb"), Path::new("images/a")),
            PathBuf::from("tb/images/a")
        );
        // a cwd toolbox root leaves the path untouched
        assert_eq!(
            resolve_under_toolbox(Path::new("."), Path::new("images/a")),
            PathBuf::from("images/a")
        );
        // an absolute path is used as given
        assert_eq!(
            resolve_under_toolbox(Path::new("tb"), Path::new("/abs/a")),
            PathBuf::from("/abs/a")
        );
    }
}
