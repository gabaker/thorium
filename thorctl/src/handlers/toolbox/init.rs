//! Scaffolds toolbox, image, and pipeline files with default configs
//!
//! Supports both interactive (default) and non-interactive (`-n`) modes.
//! Interactive mode prompts for key fields and optionally opens the user's
//! editor for final review. Non-interactive mode writes stubs with defaults.

use colored::Colorize;
use std::path::Path;
use thorium::Error;

use super::editor;
use super::prompt::{self, ImageConfigAnswers, PipelineConfigAnswers};
use crate::args::toolbox::{Init, InitImage, InitPipeline, InitToolbox, PipelineSpec};

// ─── File Helpers ────────────────────────────────────────────────────────────

pub(crate) async fn write_if_absent(path: &Path, contents: &str) -> Result<bool, Error> {
    if tokio::fs::try_exists(path).await.unwrap_or(false) {
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
    tokio::fs::write(path, contents).await.map_err(|e| {
        Error::new(format!("Failed to write '{}': {e}", path.display()))
    })?;
    println!("{} {}", "Created".bright_green(), path.display());
    Ok(true)
}

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

fn require_group(group: &Option<String>) -> Result<&str, Error> {
    group
        .as_deref()
        .ok_or_else(|| Error::new("--group is required in non-interactive mode".to_string()))
}

// ─── Config Builders ─────────────────────────────────────────────────────────

fn build_image_config(answers: &ImageConfigAnswers) -> String {
    serde_json::to_string_pretty(&serde_json::json!({
        "group": answers.group,
        "name": answers.name,
        "scaler": answers.scaler,
        "image": answers.image_tag,
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

pub(crate) fn generate_image_manifest(name: &str, image_name_field: &str, no_build: bool) -> String {
    let mut manifest = format!(
        "name = \"{name}\"\n\
         type = \"image\"\n\
         config_from = \"{name}.json\"\n\
         build_path = \"./\"\n\
         image_name = \"{image_name_field}\"\n\
         version = \"latest\"\n"
    );
    if no_build {
        manifest.push_str("build = false\n");
    }
    manifest
}

pub(crate) fn generate_pipeline_manifest(name: &str, image_names: &[String]) -> String {
    let mut manifest = format!(
        "name = \"{name}\"\n\
         type = \"pipeline\"\n\
         description = \"\"\n\
         version = \"latest\"\n\
         config_from = \"{name}.json\"\n"
    );
    for image_name in image_names {
        manifest.push_str(&format!("\n[images.{image_name}]\nversion = \"latest\"\n"));
    }
    manifest
}

// ─── Editor Review ───────────────────────────────────────────────────────────

pub(crate) async fn review_config_in_editor(json_config: &str, label: &str) -> Result<String, Error> {
    let value: serde_json::Value = serde_json::from_str(json_config)
        .map_err(|e| Error::new(format!("Failed to parse config for editor review: {e}")))?;
    let yaml = serde_yaml::to_string(&value)
        .map_err(|e| Error::new(format!("Failed to convert config to YAML: {e}")))?;

    let editor_cmd = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
    let resolved: serde_json::Value =
        match editor::editor_loop(&yaml, label, &editor_cmd).await? {
            Some(v) => v,
            None => return Ok(json_config.to_string()),
        };

    serde_json::to_string_pretty(&resolved)
        .map_err(|e| Error::new(format!("Failed to serialize edited config: {e}")))
}

// ─── Shared Write Helpers ────────────────────────────────────────────────────

async fn write_image_files(
    path: &Path,
    image_name_field: &str,
    answers: &ImageConfigAnswers,
) -> Result<(), Error> {
    let manifest = generate_image_manifest(&answers.name, image_name_field, answers.no_build);
    write_if_absent(&path.join("manifest.toml"), &manifest).await?;

    let config_json = build_image_config(answers);
    let final_json = if answers.review_in_editor {
        review_config_in_editor(&config_json, &format!("init-image-{}", answers.name)).await?
    } else {
        config_json
    };
    write_if_absent(&path.join(format!("{}.json", answers.name)), &final_json).await?;
    Ok(())
}

async fn write_pipeline_files(
    path: &Path,
    answers: &PipelineConfigAnswers,
) -> Result<(), Error> {
    let manifest = generate_pipeline_manifest(&answers.name, &answers.images);
    write_if_absent(&path.join("manifest.toml"), &manifest).await?;

    let config_json = build_pipeline_config(answers);
    let final_json = if answers.review_in_editor {
        review_config_in_editor(&config_json, &format!("init-pipeline-{}", answers.name)).await?
    } else {
        config_json
    };
    write_if_absent(&path.join(format!("{}.json", answers.name)), &final_json).await?;
    Ok(())
}

// ─── Subcommand Dispatch ─────────────────────────────────────────────────────

pub async fn handle(cmd: &Init) -> Result<(), Error> {
    match cmd {
        Init::Toolbox(cmd) => init_toolbox(cmd).await,
        Init::Image(cmd) => init_image(cmd).await,
        Init::Pipeline(cmd) => init_pipeline(cmd).await,
    }
}

async fn init_image(cmd: &InitImage) -> Result<(), Error> {
    let dir = dir_name(&cmd.path)?;
    let answers = if cmd.non_interactive {
        let group = require_group(&cmd.group)?;
        ImageConfigAnswers::defaults(&dir, group, cmd.no_build)
    } else {
        prompt::prompt_image_config(&dir, cmd.group.as_deref())?
    };
    write_image_files(&cmd.path, &dir, &answers).await
}

async fn init_pipeline(cmd: &InitPipeline) -> Result<(), Error> {
    let dir = dir_name(&cmd.path)?;
    let answers = if cmd.non_interactive {
        let group = require_group(&cmd.group)?;
        if cmd.images.is_empty() {
            return Err(Error::new(
                "--image is required in non-interactive mode".to_string(),
            ));
        }
        let mut answers = PipelineConfigAnswers::defaults(&dir, group, &cmd.images);
        if let Some(order_str) = &cmd.order {
            answers.order = serde_json::from_str(order_str)
                .map_err(|e| Error::new(format!("Invalid --order JSON: {e}")))?;
        }
        answers
    } else {
        prompt::prompt_pipeline_config(&dir, cmd.group.as_deref(), &cmd.images)?
    };
    write_pipeline_files(&cmd.path, &answers).await
}

async fn init_toolbox(cmd: &InitToolbox) -> Result<(), Error> {
    let image_names: Vec<String> = cmd
        .images
        .iter()
        .map(|p| dir_name(p))
        .collect::<Result<_, _>>()?;
    let pipeline_specs: Vec<PipelineSpec> =
        cmd.pipelines.iter().map(|s| PipelineSpec::parse(s)).collect();

    let (tb_name, tb_registry) = if cmd.non_interactive {
        (cmd.name.clone(), cmd.registry.clone())
    } else {
        let tb = prompt::prompt_toolbox_config(&cmd.name, &cmd.registry)?;
        (tb.name, tb.registry)
    };
    let config_toml = format!("name = \"{tb_name}\"\nregistry = \"{tb_registry}\"\n");
    write_if_absent(&cmd.toolbox_dir.join("config.toml"), &config_toml).await?;

    let group: String = if cmd.non_interactive {
        require_group(&cmd.group)?.to_string()
    } else {
        match &cmd.group {
            Some(g) => g.clone(),
            None => dialoguer::Input::new()
                .with_prompt("Group name for all images and pipelines")
                .interact_text()
                .map_err(|e| Error::new(format!("Failed to read input: {e}")))?,
        }
    };

    for (image_path, image_name) in cmd.images.iter().zip(&image_names) {
        let image_name_field = image_path
            .strip_prefix(&cmd.toolbox_dir)
            .unwrap_or(image_path)
            .to_string_lossy();
        let answers = if cmd.non_interactive {
            ImageConfigAnswers::defaults(image_name, &group, false)
        } else {
            prompt::prompt_image_config(image_name, Some(&group))?
        };
        write_image_files(image_path, &image_name_field, &answers).await?;
    }

    for spec in &pipeline_specs {
        let pipeline_name = dir_name(&spec.path)?;
        let pipeline_images = spec
            .images
            .clone()
            .unwrap_or_else(|| image_names.clone());
        let answers = if cmd.non_interactive {
            PipelineConfigAnswers::defaults(&pipeline_name, &group, &pipeline_images)
        } else {
            prompt::prompt_pipeline_config(&pipeline_name, Some(&group), &pipeline_images)?
        };
        write_pipeline_files(&spec.path, &answers).await?;
    }

    println!(
        "\n{} Run {} to produce a toolbox.json",
        "Done!".bright_green(),
        "thorctl toolbox build".bright_cyan()
    );
    Ok(())
}
