//! Printing a tool version's spec in the form `create` and `publish` read

use thorium::models::ToolSpec;
use thorium::{Error, Thorium};

use super::target::resolve;
use crate::args::tools::SpecTool;
use crate::utils;
use crate::utils::output::OutputFormat;

/// The top-level keys of a spec document, one per kind of tool
pub const SPEC_KEYS: &[&str] = &["Agentic", "Image", "Harness"];

/// Render a spec as the document `tools create` and `tools publish` read
///
/// # Arguments
///
/// * `spec` - The spec to render
/// * `json` - Whether to write JSON instead of YAML
pub fn spec_text(spec: &ToolSpec, json: bool) -> Result<String, Error> {
    // the kind is the document's one top-level key
    let value = serde_json::to_value(spec)
        .map_err(|err| Error::new(format!("Failed to convert the spec: {err}")))?;
    // write it with the same key order every time
    if json {
        utils::curated_json(&value, SPEC_KEYS)
    } else {
        utils::curated_yaml(&value, SPEC_KEYS)
    }
}

/// Print or write a tool version's spec
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The spec command
pub async fn spec(thorium: &Thorium, cmd: &SpecTool) -> Result<(), Error> {
    // find the version, which is the latest unless the target names one
    let found = resolve(thorium, &cmd.tool, true).await?;
    let version = found.instance_meta("print")?;
    // render it as JSON when asked to and as YAML otherwise
    let text = spec_text(&version.spec, cmd.format.format == Some(OutputFormat::Json))?;
    // write it where the user asked, or print it
    match &cmd.output {
        Some(path) => {
            std::fs::write(path, &text)
                .map_err(|err| Error::new(format!("Failed to write {}: {err}", path.display())))?;
            println!(
                "Wrote {} version {} to {}",
                found.tool.name,
                version.version,
                path.display()
            );
        }
        None => print!("{text}"),
    }
    Ok(())
}
