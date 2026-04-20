//! Scaffolds toolbox, image, and pipeline files with default configs
//!
//! Supports both interactive (default) and non-interactive (`-n`) modes.
//! Interactive mode builds the default config and opens it in the user's editor
//! to fill in (via `review_config_in_editor`). Non-interactive mode writes the
//! defaults with no editor.

use colored::Colorize;
use std::path::Path;
use thorium::Error;
use thorium::models::{ImageRequest, PipelineRequest};

use super::build::BaseImage;
use super::prompt::{self, ImageConfigAnswers, PipelineConfigAnswers};
use crate::args::Args;
use crate::args::toolbox::{Init, InitImage, InitPipeline, InitToolbox, PipelineSpec};
use crate::handlers::imports::editor;
use crate::handlers::imports::merge::{IMAGE_FIELD_ORDER, PIPELINE_FIELD_ORDER};

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
pub(crate) async fn write_file(path: &Path, contents: &str, overwrite: bool) -> Result<bool, Error> {
    // stat once and surface a real IO error rather than treating it as "absent",
    // which would silently overwrite a file we couldn't read
    let exists = tokio::fs::try_exists(path)
        .await
        .map_err(|e| Error::new(format!("Failed to stat '{}': {e}", path.display())))?;
    if !overwrite && exists {
        println!(
            "{} {} (already exists)",
            "Skipping".bright_yellow(),
            path.display()
        );
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|e| {
            Error::new(format!(
                "Failed to create directory '{}': {e}",
                parent.display()
            ))
        })?;
    }
    let overwritten = overwrite && exists;
    tokio::fs::write(path, contents)
        .await
        .map_err(|e| Error::new(format!("Failed to write '{}': {e}", path.display())))?;
    if overwritten {
        println!("{} {}", "Overwrote".bright_yellow(), path.display());
    } else {
        println!("{} {}", "Created".bright_green(), path.display());
    }
    Ok(true)
}

/// Extracts the final directory component of a path as a string
///
/// # Arguments
///
/// * `path` - The path to take the directory name from
fn dir_name(path: &Path) -> Result<String, Error> {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(String::from)
        .ok_or_else(|| {
            Error::new(format!(
                "Cannot determine directory name for '{}'",
                path.display()
            ))
        })
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
    prompt::validate_name(name)
        .map_err(|err| Error::new(format!("Invalid {kind} name '{name}': {err}")))
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
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

/// Render a toolbox `config.toml` from its toolbox-wide settings
///
/// Shared by `toolbox init` and `toolbox export` so the two can't drift. An unset
/// `registry`, empty `registries`, and an unset `image_path_prefix` are emitted as
/// commented-out placeholders to document the available knobs; `bundled_images` is
/// only written when true.
///
/// # Arguments
///
/// * `name` - The toolbox name
/// * `registry` - The primary container registry, or `None` to leave it unset
/// * `registries` - Extra registries to additionally tag for
/// * `image_path_prefix` - The default bundled-image registry base path, if any
/// * `bundled_images` - Whether the toolbox bundles image tarballs
/// * `base_image` - The toolbox-wide default base-image configuration, if any
pub(crate) fn render_config_toml(
    name: &str,
    registry: Option<&str>,
    registries: &[String],
    image_path_prefix: Option<&str>,
    bundled_images: bool,
    base_image: Option<&BaseImage>,
) -> String {
    let mut out = format!("name = \"{}\"\n", toml_escape(name));
    // a set registry is written out; an unset one is a commented placeholder so the
    // generated config documents the knob without forcing a (possibly wrong) value
    match registry {
        Some(registry) => out.push_str(&format!("registry = \"{}\"\n", toml_escape(registry))),
        None => out.push_str("# registry = \"\"\n"),
    }
    if registries.is_empty() {
        out.push_str("# registries = []\n");
    } else {
        let quoted: Vec<String> = registries
            .iter()
            .map(|registry| format!("\"{}\"", toml_escape(registry)))
            .collect();
        out.push_str(&format!("registries = [{}]\n", quoted.join(", ")));
    }
    if bundled_images {
        out.push_str("bundled_images = true\n");
    }
    match image_path_prefix {
        Some(prefix) => out.push_str(&format!("image_path_prefix = \"{}\"\n", toml_escape(prefix))),
        None => out.push_str("# image_path_prefix = \"\"\n"),
    }
    // the base-image config is a TOML table, so it must come after every scalar key; an unset one
    // is a commented placeholder documenting the knobs
    match base_image {
        Some(base) => {
            out.push_str("\n[base_image]\n");
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
    .expect("static JSON template must serialize")
}

/// Renders the default pipeline config JSON, with every `PipelineRequest` field present
///
/// # Arguments
///
/// * `answers` - The wizard answers seeding the config's identity and order/sla
fn build_pipeline_config(answers: &PipelineConfigAnswers) -> String {
    serde_json::to_string_pretty(&serde_json::json!({
        "group": answers.group,
        "name": answers.name,
        "order": answers.order,
        "sla": answers.sla,
        "triggers": {},
        "description": answers.description
    }))
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
        manifest.push_str(&format!("exported_image_path = \"{}\"\n", toml_escape(path)));
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
    let mut manifest = format!(
        "name = \"{name}\"\n\
         type = \"pipeline\"\n\
         description = \"\"\n\
         version = \"latest\"\n\
         config_from = \"{name}.json\"\n"
    );
    for (image_name, version) in images {
        manifest.push_str(&format!(
            "\n[images.{image_name}]\nversion = \"{version}\"\n"
        ));
    }
    manifest
}

// ─── Shared Write Helpers ────────────────────────────────────────────────────

/// Writes an image's `manifest.toml`, `<name>.json`, and `description.md`
///
/// Builds the default config, optionally opens it in the editor for review, then
/// re-reads identity from the saved config so the manifest and filename stay
/// consistent with whatever the user kept.
///
/// # Arguments
///
/// * `path` - The image directory to write into
/// * `answers` - The wizard answers seeding the config
/// * `overwrite` - Overwrite existing files instead of skipping them
/// * `open_editor` - Open the config in the editor before writing (interactive mode)
/// * `editor` - The editor command to open the config with
async fn write_image_files(
    path: &Path,
    answers: &ImageConfigAnswers,
    overwrite: bool,
    open_editor: bool,
    editor: &str,
) -> Result<(), Error> {
    // build the default config, then (interactively) let the user fill it in via
    // the editor — the editor edits exactly what is written to <name>.json
    let config_json = build_image_config(answers);
    let final_json = if open_editor {
        editor::review_config_in_editor::<ImageRequest>(
            &config_json,
            &format!("init-image-{}", answers.name),
            editor,
            IMAGE_FIELD_ORDER,
        )
        .await?
    } else {
        // non-interactive: emit the default in curated order with all fields present
        let value: serde_json::Value = serde_json::from_str(&config_json)
            .map_err(|e| Error::new(format!("Invalid default image config: {e}")))?;
        crate::utils::curated_json(&value, IMAGE_FIELD_ORDER)?
    };
    // re-read identity from the (possibly edited) config so the manifest + filename
    // stay consistent with whatever the user saved
    let value: serde_json::Value = serde_json::from_str(&final_json)
        .map_err(|e| Error::new(format!("Edited image config is not valid JSON: {e}")))?;
    let name = json_str_field(&value, "name")
        .ok_or_else(|| Error::new("image config is missing a 'name' field".to_string()))?;
    let group = json_str_field(&value, "group")
        .ok_or_else(|| Error::new("image config is missing a 'group' field".to_string()))?;
    validate_resource_name("image", &name)?;
    validate_resource_name("group", &group)?;

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
    write_file(&path.join("manifest.toml"), &manifest, overwrite).await?;
    write_file(&path.join(format!("{name}.json")), &final_json, overwrite).await?;
    // description.md is the source of truth toolbox build injects, so seed it from
    // the (possibly edited) config description
    let description = json_str_field(&value, "description").filter(|d| !d.is_empty());
    let description_md = description_stub(&name, description.as_deref());
    write_file(&path.join("description.md"), &description_md, overwrite).await?;
    Ok(())
}

/// Read a string field from a JSON config value, if present and a string
///
/// # Arguments
///
/// * `value` - The JSON config value to read from
/// * `field` - The name of the field to read
fn json_str_field(value: &serde_json::Value, field: &str) -> Option<String> {
    value.get(field).and_then(|v| v.as_str()).map(str::to_string)
}

/// Collect the unique image names referenced across a pipeline config's `order`,
/// preserving first-seen order. Used to keep the manifest's image map in sync with
/// an order edited in the editor.
///
/// # Arguments
///
/// * `value` - The pipeline config value whose `order` is scanned for image names
fn unique_order_images(value: &serde_json::Value) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut images = Vec::new();
    if let Some(order) = value.get("order").and_then(|o| o.as_array()) {
        for stage in order {
            let Some(stage) = stage.as_array() else {
                continue;
            };
            for image in stage {
                if let Some(image) = image.as_str()
                    && seen.insert(image.to_string())
                {
                    images.push(image.to_string());
                }
            }
        }
    }
    images
}

/// Resolve the editor for an init subcommand: an explicit `--editor` override,
/// else the configured `default_editor`, else the built-in default. Init is
/// offline, so the config is loaded best-effort.
///
/// # Arguments
///
/// * `editor_override` - The explicit `--editor` value, if given
/// * `args` - The top-level thorctl args (used to locate the config)
fn resolve_editor(editor_override: Option<&str>, args: &Args) -> String {
    if let Some(editor) = editor_override {
        return editor.to_string();
    }
    if let Ok(conf) = thorium::CtlConf::from_path(&args.config) {
        return conf.default_editor;
    }
    thorium::client::conf::default_default_editor()
}

/// Resolve the group for an init subcommand from `--group` or an interactive prompt
///
/// # Arguments
///
/// * `group` - The explicit `--group` value, if given
/// * `non_interactive` - Whether `--non-interactive` is set (errors instead of prompting)
fn resolve_group(group: &Option<String>, non_interactive: bool) -> Result<String, Error> {
    match group {
        Some(group) => Ok(group.clone()),
        None if non_interactive => Err(Error::new(
            "--group is required in non-interactive mode".to_string(),
        )),
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
    match description.filter(|d| !d.is_empty()) {
        // an existing description goes under the Overview heading
        Some(description) => format!("# {name}\n\n# Overview\n\n{description}\n"),
        None => format!("# {name}\n\n# Overview\n"),
    }
}

/// Writes a pipeline's `manifest.toml`, `<name>.json`, and `description.md`
///
/// Builds the default config, optionally opens it in the editor for review, then
/// derives the manifest's image map from the (possibly edited) config order.
///
/// # Arguments
///
/// * `path` - The pipeline directory to write into
/// * `answers` - The wizard answers seeding the config
/// * `overwrite` - Overwrite existing files instead of skipping them
/// * `open_editor` - Open the config in the editor before writing (interactive mode)
/// * `editor` - The editor command to open the config with
async fn write_pipeline_files(
    path: &Path,
    answers: &PipelineConfigAnswers,
    overwrite: bool,
    open_editor: bool,
    editor: &str,
) -> Result<(), Error> {
    let config_json = build_pipeline_config(answers);
    let final_json = if open_editor {
        editor::review_config_in_editor::<PipelineRequest>(
            &config_json,
            &format!("init-pipeline-{}", answers.name),
            editor,
            PIPELINE_FIELD_ORDER,
        )
        .await?
    } else {
        // non-interactive: emit the default in curated order with all fields present
        let value: serde_json::Value = serde_json::from_str(&config_json)
            .map_err(|e| Error::new(format!("Invalid default pipeline config: {e}")))?;
        crate::utils::curated_json(&value, PIPELINE_FIELD_ORDER)?
    };
    let value: serde_json::Value = serde_json::from_str(&final_json)
        .map_err(|e| Error::new(format!("Edited pipeline config is not valid JSON: {e}")))?;
    let name = json_str_field(&value, "name")
        .ok_or_else(|| Error::new("pipeline config is missing a 'name' field".to_string()))?;
    let group = json_str_field(&value, "group")
        .ok_or_else(|| Error::new("pipeline config is missing a 'group' field".to_string()))?;
    validate_resource_name("pipeline", &name)?;
    validate_resource_name("group", &group)?;
    // derive the manifest's image map from the config's order so editing the order
    // in the editor keeps the manifest's referenced images in sync
    let images: Vec<(String, String)> = unique_order_images(&value)
        .into_iter()
        .map(|image| (image, "latest".to_string()))
        .collect();
    for (image, _) in &images {
        validate_resource_name("image", image)?;
    }
    let manifest = generate_pipeline_manifest(&name, &images);
    write_file(&path.join("manifest.toml"), &manifest, overwrite).await?;
    write_file(&path.join(format!("{name}.json")), &final_json, overwrite).await?;
    let description = json_str_field(&value, "description").filter(|d| !d.is_empty());
    let description_md = description_stub(&name, description.as_deref());
    write_file(&path.join("description.md"), &description_md, overwrite).await?;
    Ok(())
}

// ─── Subcommand Dispatch ─────────────────────────────────────────────────────

/// Dispatches an `init` subcommand to its scaffolding handler
///
/// # Arguments
///
/// * `cmd` - The init subcommand (toolbox, image, or pipeline)
/// * `args` - The top-level thorctl args
pub async fn handle(cmd: &Init, args: &Args) -> Result<(), Error> {
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
    let dir = dir_name(&cmd.path)?;
    // the manifest image_name defaults to the build directory name, overridable
    // via --image-name
    let default_image_name = cmd.image_name.clone().unwrap_or_else(|| dir.clone());
    let group = resolve_group(&cmd.group, cmd.non_interactive)?;
    let answers = ImageConfigAnswers::defaults(&dir, &group, cmd.no_build, &default_image_name);
    let editor = resolve_editor(cmd.editor.as_deref(), args);
    write_image_files(&cmd.path, &answers, cmd.overwrite, !cmd.non_interactive, &editor).await
}

/// Scaffolds a single pipeline directory from `init pipeline` args
///
/// # Arguments
///
/// * `cmd` - The `init pipeline` args
/// * `args` - The top-level thorctl args
async fn init_pipeline(cmd: &InitPipeline, args: &Args) -> Result<(), Error> {
    let dir = dir_name(&cmd.path)?;
    let group = resolve_group(&cmd.group, cmd.non_interactive)?;
    if cmd.non_interactive && cmd.images.is_empty() {
        return Err(Error::new(
            "--images is required in non-interactive mode".to_string(),
        ));
    }
    let mut answers = PipelineConfigAnswers::defaults(&dir, &group, &cmd.images);
    if let Some(order_str) = &cmd.order {
        answers.order = serde_json::from_str(order_str)
            .map_err(|e| Error::new(format!("Invalid --order JSON: {e}")))?;
    }
    let editor = resolve_editor(cmd.editor.as_deref(), args);
    write_pipeline_files(&cmd.path, &answers, cmd.overwrite, !cmd.non_interactive, &editor).await
}

/// Scaffolds a full toolbox directory (config.toml plus image/pipeline subdirs)
///
/// # Arguments
///
/// * `cmd` - The `init toolbox` args
/// * `args` - The top-level thorctl args
async fn init_toolbox(cmd: &InitToolbox, args: &Args) -> Result<(), Error> {
    let image_names: Vec<String> = cmd
        .images
        .iter()
        .map(|p| dir_name(p))
        .collect::<Result<_, _>>()?;
    let pipeline_specs: Vec<PipelineSpec> = cmd
        .pipelines
        .iter()
        .map(|s| PipelineSpec::parse(s))
        .collect();

    let config_toml = if let Some(config_path) = &cmd.config {
        // seed the new toolbox from an existing config.toml (mutually exclusive with
        // --name/--registry); carries name, registry, registries, image_path_prefix,
        // and bundled_images forward verbatim
        let template = super::build::load_config(config_path)?;
        render_config_toml(
            &template.name,
            template.registry.as_deref(),
            &template.registries,
            template.image_path_prefix.as_deref(),
            template.bundled_images,
            template.base_image.as_ref(),
        )
    } else {
        let (tb_name, tb_registry) = if cmd.non_interactive {
            (cmd.name.clone(), cmd.registry.clone())
        } else {
            let tb = prompt::prompt_toolbox_config(&cmd.name, cmd.registry.as_deref())?;
            (tb.name, tb.registry)
        };
        render_config_toml(&tb_name, tb_registry.as_deref(), &[], None, false, None)
    };
    write_file(
        &cmd.toolbox_dir.join("config.toml"),
        &config_toml,
        cmd.overwrite,
    )
    .await?;

    let group = resolve_group(&cmd.group, cmd.non_interactive)?;
    let editor = resolve_editor(cmd.editor.as_deref(), args);
    let open_editor = !cmd.non_interactive;

    for (image_path, image_name) in cmd.images.iter().zip(&image_names) {
        // the manifest image_name defaults to the image's basename (matching `init image`)
        let answers = ImageConfigAnswers::defaults(image_name, &group, false, image_name);
        write_image_files(image_path, &answers, cmd.overwrite, open_editor, &editor).await?;
    }

    for spec in &pipeline_specs {
        let pipeline_name = dir_name(&spec.path)?;
        let pipeline_images = spec.images.clone().unwrap_or_else(|| image_names.clone());
        let answers = PipelineConfigAnswers::defaults(&pipeline_name, &group, &pipeline_images);
        write_pipeline_files(&spec.path, &answers, cmd.overwrite, open_editor, &editor).await?;
    }

    println!(
        "\n{} Run {} to produce a toolbox.json",
        "Done!".bright_green(),
        "thorctl toolbox build".bright_cyan()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::prompt::{ImageConfigAnswers, PipelineConfigAnswers};
    use super::{
        BaseImage, build_image_config, build_pipeline_config, description_stub, render_config_toml,
    };

    /// The scaffolded description.md is just the tool name + an Overview section (no
    /// placeholder prose); an existing description is placed under Overview
    #[test]
    fn description_stub_uses_overview_section() {
        let empty = description_stub("clamav", None);
        assert_eq!(empty, "# clamav\n\n# Overview\n");
        assert!(!empty.contains("Describe what this tool"));

        let with_desc = description_stub("clamav", Some("scans files"));
        assert_eq!(with_desc, "# clamav\n\n# Overview\n\nscans files\n");
    }

    /// The scaffolded image default must deserialize into the real `ImageRequest`
    /// (the type `toolbox import` and the editor validation parse it as) and include
    /// the `version`/`lifetime`/`modifiers` fields so the saved config is complete
    #[test]
    fn image_template_deserializes_into_request() {
        let answers = ImageConfigAnswers::defaults("clamav", "static", false, "clamav");
        let json = build_image_config(&answers);
        serde_json::from_str::<thorium::models::ImageRequest>(&json)
            .expect("default image config must deserialize into ImageRequest");
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        for key in ["version", "lifetime", "modifiers"] {
            assert!(value.get(key).is_some(), "image template missing '{key}'");
        }
    }

    /// The scaffolded pipeline default must deserialize into the real `PipelineRequest`
    #[test]
    fn pipeline_template_deserializes_into_request() {
        let answers =
            PipelineConfigAnswers::defaults("triage", "static", &["clamav".to_string()]);
        let json = build_pipeline_config(&answers);
        serde_json::from_str::<thorium::models::PipelineRequest>(&json)
            .expect("default pipeline config must deserialize into PipelineRequest");
    }

    /// With no extras, registries and image_path_prefix are emitted as commented
    /// placeholders and bundled_images is omitted
    #[test]
    fn render_config_minimal() {
        let toml = render_config_toml("My TB", Some("ghcr.io/o/r"), &[], None, false, None);
        assert!(toml.contains("name = \"My TB\""));
        assert!(toml.contains("registry = \"ghcr.io/o/r\""));
        assert!(toml.contains("# registries = []"));
        assert!(toml.contains("# image_path_prefix = \"\""));
        assert!(!toml.contains("bundled_images"));
        // an unset base image is a commented placeholder
        assert!(toml.contains("# [base_image]"));
    }

    /// An unset registry is emitted as a commented placeholder, not `registry = ""`
    #[test]
    fn render_config_no_registry() {
        let toml = render_config_toml("My TB", None, &[], None, false, None);
        assert!(toml.contains("# registry = \"\""));
        // the active (uncommented) registry line must not be present
        assert!(!toml.contains("\nregistry = "));
    }

    /// Extra registries, an image_path_prefix, bundled_images, and a base image are
    /// written out
    #[test]
    fn render_config_full() {
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
            true,
            Some(&base),
        );
        assert!(toml.contains("registries = [\"reg\", \"reg2\"]"));
        assert!(toml.contains("bundled_images = true"));
        assert!(toml.contains("image_path_prefix = \"prefix/path\""));
        assert!(!toml.contains("# image_path_prefix"));
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
        let toml = render_config_toml("a\"b", Some("r"), &[], None, false, None);
        assert!(toml.contains("name = \"a\\\"b\""));
    }
}
