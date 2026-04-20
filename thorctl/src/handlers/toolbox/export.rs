//! Export Thorium images and pipelines into a toolbox directory structure

use colored::Colorize;
use std::collections::{HashMap, HashSet};
use thorium::models::{Image, ImageRequest, Pipeline, PipelineRequest};
use thorium::{Error, Thorium};

use super::build;
use super::init::{
    generate_image_manifest, generate_pipeline_manifest, review_config_in_editor, write_if_absent,
};
use crate::args::toolbox::{BuildToolbox, ExportToolbox, ResourceSpec};

// ─── Resource Resolution ─────────────────────────────────────────────────────

async fn list_all_images(thorium: &Thorium, group: &str) -> Result<Vec<Image>, Error> {
    let mut images = Vec::new();
    let mut cursor = thorium.images.list(group).limit(1_000_000).details();
    loop {
        cursor.next().await.map_err(|e| {
            Error::new(format!("Failed to list images in group '{group}': {e}"))
        })?;
        images.append(&mut cursor.details);
        if cursor.exhausted {
            break;
        }
    }
    Ok(images)
}

async fn list_all_pipelines(thorium: &Thorium, group: &str) -> Result<Vec<Pipeline>, Error> {
    let mut pipelines = Vec::new();
    let mut cursor = thorium.pipelines.list(group).limit(1_000_000).details();
    loop {
        cursor.next().await.map_err(|e| {
            Error::new(format!("Failed to list pipelines in group '{group}': {e}"))
        })?;
        pipelines.append(&mut cursor.details);
        if cursor.exhausted {
            break;
        }
    }
    Ok(pipelines)
}

async fn resolve_resources(
    thorium: &Thorium,
    cmd: &ExportToolbox,
) -> Result<(Vec<Image>, Vec<Pipeline>), Error> {
    let mut images: Vec<Image> = Vec::new();
    let mut pipelines: Vec<Pipeline> = Vec::new();
    let mut seen_images: HashSet<(String, String)> = HashSet::new();

    // Full group export
    if let Some(group) = &cmd.group {
        if cmd.pipelines.is_empty() && cmd.images.is_empty() {
            let group_images = list_all_images(thorium, group).await?;
            let group_pipelines = list_all_pipelines(thorium, group).await?;
            for img in group_images {
                seen_images.insert((img.group.clone(), img.name.clone()));
                images.push(img);
            }
            pipelines.extend(group_pipelines);
        }
    }

    // Specific pipelines + auto-resolve referenced images
    for spec_str in &cmd.pipelines {
        let spec = ResourceSpec::parse(spec_str, cmd.group.as_deref())
            .map_err(|e| Error::new(e))?;
        let pipeline = thorium
            .pipelines
            .get(&spec.group, &spec.name)
            .await
            .map_err(|e| {
                Error::new(format!(
                    "Failed to get pipeline '{}:{}': {e}",
                    spec.group, spec.name
                ))
            })?;
        // Auto-resolve images referenced in pipeline order
        for stage in &pipeline.order {
            for image_name in stage {
                let key = (spec.group.clone(), image_name.clone());
                if seen_images.insert(key) {
                    let image = thorium
                        .images
                        .get(&spec.group, image_name)
                        .await
                        .map_err(|e| {
                            Error::new(format!(
                                "Failed to get image '{image_name}' (referenced by pipeline '{}'): {e}",
                                spec.name
                            ))
                        })?;
                    images.push(image);
                }
            }
        }
        pipelines.push(pipeline);
    }

    // Specific standalone images
    for spec_str in &cmd.images {
        let spec = ResourceSpec::parse(spec_str, cmd.group.as_deref())
            .map_err(|e| Error::new(e))?;
        let key = (spec.group.clone(), spec.name.clone());
        if seen_images.insert(key) {
            let image = thorium
                .images
                .get(&spec.group, &spec.name)
                .await
                .map_err(|e| {
                    Error::new(format!(
                        "Failed to get image '{}:{}': {e}",
                        spec.group, spec.name
                    ))
                })?;
            images.push(image);
        }
    }

    if images.is_empty() && pipelines.is_empty() {
        return Err(Error::new(
            "No resources to export. Specify --group, --pipeline, or --image.",
        ));
    }

    Ok((images, pipelines))
}

// ─── Collision Detection ─────────────────────────────────────────────────────

fn check_name_collisions(images: &[Image], pipelines: &[Pipeline]) {
    let mut image_groups: HashMap<&str, Vec<&str>> = HashMap::new();
    for img in images {
        image_groups.entry(&img.name).or_default().push(&img.group);
    }
    for (name, groups) in &image_groups {
        if groups.len() > 1 {
            eprintln!(
                "{} Image '{}' exists in groups {} — only the last will be kept",
                "Warning:".bright_yellow(),
                name.bright_cyan(),
                groups
                    .iter()
                    .map(|g| format!("'{g}'"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }
    let mut pipeline_groups: HashMap<&str, Vec<&str>> = HashMap::new();
    for pipe in pipelines {
        pipeline_groups
            .entry(&pipe.name)
            .or_default()
            .push(&pipe.group);
    }
    for (name, groups) in &pipeline_groups {
        if groups.len() > 1 {
            eprintln!(
                "{} Pipeline '{}' exists in groups {} — only the last will be kept",
                "Warning:".bright_yellow(),
                name.bright_cyan(),
                groups
                    .iter()
                    .map(|g| format!("'{g}'"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }
}

// ─── File Writing ────────────────────────────────────────────────────────────

async fn write_exported_image(
    output: &std::path::Path,
    image: &Image,
    group_override: Option<&str>,
    non_interactive: bool,
) -> Result<(), Error> {
    let image_dir = output.join("images").join(&image.name);
    let mut image_req = ImageRequest::from(image.clone());
    if let Some(group) = group_override {
        image_req.group = group.to_string();
    }

    let config_json = serde_json::to_string_pretty(&image_req)
        .map_err(|e| Error::new(format!("Failed to serialize image '{}': {e}", image.name)))?;
    let final_json = if non_interactive {
        config_json
    } else {
        review_config_in_editor(&config_json, &format!("export-image-{}", image.name)).await?
    };
    write_if_absent(
        &image_dir.join(format!("{}.json", image.name)),
        &final_json,
    )
    .await?;

    let manifest = generate_image_manifest(&image.name, &image.name, true);
    write_if_absent(&image_dir.join("manifest.toml"), &manifest).await?;
    Ok(())
}

async fn write_exported_pipeline(
    output: &std::path::Path,
    pipeline: &Pipeline,
    group_override: Option<&str>,
    non_interactive: bool,
) -> Result<(), Error> {
    let pipeline_dir = output.join("pipelines").join(&pipeline.name);
    let mut pipeline_req = PipelineRequest::from(pipeline.clone());
    if let Some(group) = group_override {
        pipeline_req.group = group.to_string();
    }

    let config_json = serde_json::to_string_pretty(&pipeline_req)
        .map_err(|e| {
            Error::new(format!(
                "Failed to serialize pipeline '{}': {e}",
                pipeline.name
            ))
        })?;
    let final_json = if non_interactive {
        config_json
    } else {
        review_config_in_editor(&config_json, &format!("export-pipeline-{}", pipeline.name))
            .await?
    };
    write_if_absent(
        &pipeline_dir.join(format!("{}.json", pipeline.name)),
        &final_json,
    )
    .await?;

    // Collect unique image names from pipeline order for the manifest
    let image_names: Vec<String> = pipeline
        .order
        .iter()
        .flatten()
        .cloned()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let manifest = generate_pipeline_manifest(&pipeline.name, &image_names);
    write_if_absent(&pipeline_dir.join("manifest.toml"), &manifest).await?;
    Ok(())
}

// ─── Main Export ─────────────────────────────────────────────────────────────

pub async fn export(thorium: Thorium, cmd: &ExportToolbox) -> Result<(), Error> {
    let (images, pipelines) = resolve_resources(&thorium, cmd).await?;

    println!(
        "Exporting {} images and {} pipelines to '{}'",
        images.len().to_string().bright_green(),
        pipelines.len().to_string().bright_green(),
        cmd.output.display().to_string().bright_cyan(),
    );

    if cmd.group_override.is_some() {
        check_name_collisions(&images, &pipelines);
    }

    for image in &images {
        write_exported_image(
            &cmd.output,
            image,
            cmd.group_override.as_deref(),
            cmd.non_interactive,
        )
        .await?;
    }

    for pipeline in &pipelines {
        write_exported_pipeline(
            &cmd.output,
            pipeline,
            cmd.group_override.as_deref(),
            cmd.non_interactive,
        )
        .await?;
    }

    // Write config.toml
    let config_toml = format!(
        "name = \"{}\"\nregistry = \"{}\"\n",
        cmd.name, cmd.registry
    );
    write_if_absent(&cmd.output.join("config.toml"), &config_toml).await?;

    // Auto-build toolbox.json
    let build_cmd = BuildToolbox {
        config: cmd.output.join("config.toml"),
        override_path: false,
        output: cmd.output.join("toolbox.json"),
        path: cmd.output.clone(),
    };
    build::build(&build_cmd)?;

    println!(
        "\n{} Toolbox exported to '{}'",
        "Done!".bright_green(),
        cmd.output.display()
    );
    Ok(())
}
