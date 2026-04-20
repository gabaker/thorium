//! Interactive prompt wizards for toolbox init
//!
//! Provides guided step-by-step configuration for images, pipelines, and
//! toolboxes using `dialoguer` prompts with sane defaults.

use colored::Colorize;
use thorium::Error;

// ─── Prompt Helpers ──────────────────────────────────────────────────────────

fn prompt_input(label: &str, default: &str) -> Result<String, Error> {
    dialoguer::Input::new()
        .with_prompt(label)
        .default(default.to_string())
        .interact_text()
        .map_err(|e| Error::new(format!("Failed to read input: {e}")))
}

fn prompt_input_required(label: &str) -> Result<String, Error> {
    dialoguer::Input::new()
        .with_prompt(label)
        .interact_text()
        .map_err(|e| Error::new(format!("Failed to read input: {e}")))
}

fn prompt_u64(label: &str, default: u64) -> Result<u64, Error> {
    dialoguer::Input::new()
        .with_prompt(label)
        .default(default)
        .interact_text()
        .map_err(|e| Error::new(format!("Failed to read input: {e}")))
}

fn prompt_bool(label: &str, default: bool) -> Result<bool, Error> {
    dialoguer::Confirm::new()
        .with_prompt(label)
        .default(default)
        .interact()
        .map_err(|e| Error::new(format!("Failed to read input: {e}")))
}

fn prompt_select(label: &str, options: &[&str], default: usize) -> Result<usize, Error> {
    println!("\n{}", label.bright_cyan());
    dialoguer::Select::new()
        .items(options)
        .default(default)
        .interact()
        .map_err(|e| Error::new(format!("Failed to read selection: {e}")))
}

fn prompt_multi_select(label: &str, options: &[&str]) -> Result<Vec<usize>, Error> {
    println!("\n{}", label.bright_cyan());
    dialoguer::MultiSelect::new()
        .items(options)
        .interact()
        .map_err(|e| Error::new(format!("Failed to read selection: {e}")))
}

// ─── Answer Structs ──────────────────────────────────────────────────────────

pub struct ImageConfigAnswers {
    pub group: String,
    pub name: String,
    pub image_tag: String,
    pub timeout: u64,
    pub cpu: String,
    pub memory: String,
    pub scaler: String,
    pub display_type: String,
    pub generator: bool,
    pub no_build: bool,
    pub description: Option<String>,
    pub review_in_editor: bool,
}

impl ImageConfigAnswers {
    pub fn defaults(name: &str, group: &str, no_build: bool) -> Self {
        Self {
            group: group.to_string(),
            name: name.to_string(),
            image_tag: String::new(),
            timeout: 300,
            cpu: "1000m".to_string(),
            memory: "1024Mi".to_string(),
            scaler: "K8s".to_string(),
            display_type: "Json".to_string(),
            generator: false,
            no_build,
            description: None,
            review_in_editor: false,
        }
    }
}

pub struct PipelineConfigAnswers {
    pub group: String,
    pub name: String,
    pub images: Vec<String>,
    pub order: Vec<Vec<String>>,
    pub sla: u64,
    pub description: Option<String>,
    pub review_in_editor: bool,
}

impl PipelineConfigAnswers {
    pub fn defaults(name: &str, group: &str, images: &[String]) -> Self {
        Self {
            group: group.to_string(),
            name: name.to_string(),
            images: images.to_vec(),
            order: vec![images.to_vec()],
            sla: 640_800,
            description: None,
            review_in_editor: false,
        }
    }
}

pub struct ToolboxConfigAnswers {
    pub name: String,
    pub registry: String,
}

// ─── Image Wizard ────────────────────────────────────────────────────────────

const SCALERS: &[&str] = &["K8s", "BareMetal", "Windows", "Kvm", "External"];
const DISPLAY_TYPES: &[&str] = &["Json", "String", "Table", "Image", "Custom", "Disassembly", "Html"];

pub fn prompt_image_config(
    name: &str,
    default_group: Option<&str>,
) -> Result<ImageConfigAnswers, Error> {
    println!(
        "\n{}\n{}",
        format!("Image Configuration for '{name}'").bright_green().bold(),
        "─".repeat(40).bright_green(),
    );

    let group = match default_group {
        Some(g) => prompt_input("Group name", g)?,
        None => prompt_input_required("Group name")?,
    };
    let name = prompt_input("Image name", name)?;
    let image_tag = prompt_input("Container image tag", "")?;

    let desc_str = prompt_input("Description", "")?;
    let description = if desc_str.is_empty() {
        None
    } else {
        Some(desc_str)
    };

    let scaler_idx = prompt_select("Scaler:", SCALERS, 0)?;
    let scaler = SCALERS[scaler_idx].to_string();

    let timeout = prompt_u64("Timeout in seconds", 300)?;
    let cpu = prompt_input("CPU request", "1000m")?;
    let memory = prompt_input("Memory request", "1024Mi")?;

    let display_idx = prompt_select("Display type for results:", DISPLAY_TYPES, 0)?;
    let display_type = DISPLAY_TYPES[display_idx].to_string();

    let generator = prompt_bool("Is this a generator image?", false)?;
    let no_build = !prompt_bool("Build this image in CI/CD?", true)?;

    let review_in_editor = !prompt_bool(
        "Use defaults for remaining fields (dependencies, output collection, security, etc.)?",
        true,
    )?;

    Ok(ImageConfigAnswers {
        group,
        name,
        image_tag,
        timeout,
        cpu,
        memory,
        scaler,
        display_type,
        generator,
        no_build,
        description,
        review_in_editor,
    })
}

// ─── Pipeline Wizard ─────────────────────────────────────────────────────────

pub fn prompt_pipeline_config(
    name: &str,
    default_group: Option<&str>,
    default_images: &[String],
) -> Result<PipelineConfigAnswers, Error> {
    println!(
        "\n{}\n{}",
        format!("Pipeline Configuration for '{name}'").bright_green().bold(),
        "─".repeat(40).bright_green(),
    );

    let group = match default_group {
        Some(g) => prompt_input("Group name", g)?,
        None => prompt_input_required("Group name")?,
    };
    let name = prompt_input("Pipeline name", name)?;

    let images: Vec<String> = if default_images.is_empty() {
        let images_str = prompt_input_required("Image names (comma-separated)")?;
        images_str
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    } else {
        let default_str = default_images.join(", ");
        let images_str = prompt_input("Image names (comma-separated)", &default_str)?;
        images_str
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    };

    let order = if images.len() > 1 {
        prompt_stage_builder(&images)?
    } else {
        vec![images.clone()]
    };

    let sla = prompt_u64("SLA in seconds", 640_800)?;

    let desc_str = prompt_input("Description", "")?;
    let description = if desc_str.is_empty() {
        None
    } else {
        Some(desc_str)
    };

    let review_in_editor = !prompt_bool(
        "Use defaults for remaining fields (triggers, etc.)?",
        true,
    )?;

    Ok(PipelineConfigAnswers {
        group,
        name,
        images,
        order,
        sla,
        description,
        review_in_editor,
    })
}

// ─── Stage Builder ───────────────────────────────────────────────────────────

fn prompt_stage_builder(images: &[String]) -> Result<Vec<Vec<String>>, Error> {
    println!(
        "\n{}\n{}\n{}",
        "Pipeline Order".bright_cyan().bold(),
        "─".repeat(30).bright_cyan(),
        "Assign images to stages. Images in the same stage run in parallel.\n\
         Images in later stages wait for earlier stages to complete."
    );

    let mut remaining: Vec<String> = images.to_vec();
    let mut stages: Vec<Vec<String>> = Vec::new();

    while !remaining.is_empty() {
        let stage_num = stages.len() + 1;
        let options: Vec<&str> = remaining.iter().map(|s| s.as_str()).collect();
        let selected = prompt_multi_select(
            &format!("Stage {stage_num} — select images (space to toggle, enter to confirm):"),
            &options,
        )?;

        if selected.is_empty() {
            println!(
                "{} Please select at least one image for this stage.",
                "Warning:".bright_yellow()
            );
            continue;
        }

        let stage: Vec<String> = selected.iter().map(|&i| remaining[i].clone()).collect();
        // remove selected from remaining (reverse order to preserve indices)
        let mut to_remove: Vec<usize> = selected;
        to_remove.sort_unstable_by(|a, b| b.cmp(a));
        for i in to_remove {
            remaining.remove(i);
        }
        stages.push(stage);
    }

    // display the result
    let order_display: Vec<String> = stages
        .iter()
        .map(|s| format!("[{}]", s.join(", ")))
        .collect();
    println!(
        "\n{} [{}]",
        "Pipeline order:".bright_green(),
        order_display.join(", ")
    );

    Ok(stages)
}

// ─── Toolbox Wizard ──────────────────────────────────────────────────────────

pub fn prompt_toolbox_config(
    default_name: &str,
    default_registry: &str,
) -> Result<ToolboxConfigAnswers, Error> {
    println!(
        "\n{}\n{}",
        "Toolbox Configuration".bright_green().bold(),
        "─".repeat(30).bright_green(),
    );
    let name = prompt_input("Toolbox name", default_name)?;
    let registry = prompt_input("Container registry", default_registry)?;
    Ok(ToolboxConfigAnswers { name, registry })
}
