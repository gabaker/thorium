//! Scaffolding for `thorctl toolbox init tool`: a manifest.toml and the spec or document file it
//! points at, for a tool, toolset, agent, or workflow

use colored::Colorize;
use thorium::Error;

use super::{
    description_stub, resolve_group, resolve_interactive, resolve_tool_name, toml_escape,
    write_file,
};
use crate::args::toolbox::{InitTool, InitToolKind};
use crate::handlers::toolbox::build::spec_json;

/// The scaffolded spec of an image tool, run as Thorium jobs from a container image
const IMAGE_SPEC: &str = "\
# The spec of this tool: what Thorium runs and how. `toolbox build` fills in `image` with the
# registry in config.toml when the manifest builds the image.
Image:
  environment:
    K8s:
      image: \"\"
      scheduling:
        resources:
          cpu: 1000
          memory: 1024
        spawn_limit: Unlimited
";

/// The scaffolded spec of a harness tool, a swappable agent harness
const HARNESS_SPEC: &str = "\
# The spec of this harness: where it runs and how agents use it. `toolbox build` fills in `image`
# with the registry in config.toml when the manifest builds the image.
Harness:
  workload:
    environment:
      K8s:
        image: \"\"
        scheduling:
          resources:
            cpu: 1000
            memory: 2048
          spawn_limit: Unlimited
";

/// The scaffolded toolset document
const TOOLSET_DOCUMENT: &str = "\
# The toolset: the tools its agents can use. Name each tool, with a version to pin one.
read_only: false
auto_instance: true
tools: []
";

/// The scaffolded agent document
const AGENT_DOCUMENT: &str = "\
# The agent: how its model is prompted and which tools, toolsets, and sub-agents it can use.
config:
  system_prompt: Describe what this agent does and how it should work.
  max_turns: 20
tools: []
toolsets: []
sub_agents: []
";

/// The scaffolded workflow document
const WORKFLOW_DOCUMENT: &str = "\
# The workflow: the agents a run can use, and the agent a run starts with. Name agents that are in
# this toolbox or already in Thorium.
agents:
  - replace-with-an-agent
entry_agent: replace-with-an-agent
auto_instance: true
";

/// Get the scaffolded spec of an agentic tool
///
/// # Arguments
///
/// * `name` - The tool's name, which is also the name a model calls it by
fn agentic_spec(name: &str) -> String {
    // a model-callable tool backed by one of Thorium's own MCP tools until another backing is set
    format!(
        "\
# The spec of this tool: how a model calls it and what runs when it does. The backing is
# ThoriumMcp (a tool Thorium serves), Container (an image tool), or HarnessNative.
Agentic:
  function_name: {name}
  title: {name}
  model_description: Describe what this tool does and when a model should call it.
  input_schema:
    type: object
    properties:
      sha256:
        type: string
        description: The SHA256 of the sample to run on
    required: [sha256]
  backing:
    ThoriumMcp:
      tool: replace_with_a_thorium_mcp_tool
  timeout: 60
"
    )
}

/// Render a scaffolded tool spec as the JSON a tool's spec file holds
///
/// The scaffolds are written as YAML so their comments document them here; the file a tool's
/// manifest points at is JSON, as `toolbox build --rewrite-manifests` and `toolbox export` write it.
///
/// # Arguments
///
/// * `spec` - The scaffolded spec as YAML
fn spec_file_text(spec: &str) -> Result<String, Error> {
    // read the YAML scaffold, then write it as the spec file's JSON
    let value: serde_json::Value = serde_norway::from_str(spec)
        .map_err(|err| Error::new(format!("Failed to read a scaffolded spec: {err}")))?;
    spec_json(&value)
}

/// Get the file the scaffolded manifest points at, and its contents
///
/// # Arguments
///
/// * `cmd` - The `init tool` command
/// * `name` - The item's name
fn document(cmd: &InitTool, name: &str) -> Result<(String, String), Error> {
    // each kind has its own file suffix and starting contents
    Ok(match cmd.kind {
        InitToolKind::Image => (format!("{name}.json"), spec_file_text(IMAGE_SPEC)?),
        InitToolKind::Harness => (format!("{name}.json"), spec_file_text(HARNESS_SPEC)?),
        InitToolKind::Agentic => (format!("{name}.json"), spec_file_text(&agentic_spec(name))?),
        InitToolKind::Toolset => (format!("{name}.toolset.yaml"), TOOLSET_DOCUMENT.to_owned()),
        InitToolKind::Agent => (format!("{name}.agent.yaml"), AGENT_DOCUMENT.to_owned()),
        InitToolKind::Workflow => (
            format!("{name}.workflow.yaml"),
            WORKFLOW_DOCUMENT.to_owned(),
        ),
    })
}

/// Render the manifest.toml of a scaffolded item
///
/// # Arguments
///
/// * `cmd` - The `init tool` command
/// * `name` - The item's name
/// * `group` - The group the item is in
/// * `file` - The spec or document file the manifest points at
fn manifest(cmd: &InitTool, name: &str, group: &str, file: &str) -> String {
    // the type, name, and group every kind has
    let (kind, version, tool_kind) = match cmd.kind {
        InitToolKind::Image => ("tool", true, Some("image")),
        InitToolKind::Agentic => ("tool", true, Some("agentic")),
        InitToolKind::Harness => ("tool", true, Some("harness")),
        InitToolKind::Toolset => ("toolset", false, None),
        InitToolKind::Agent => ("agent", false, None),
        InitToolKind::Workflow => ("workflow", false, None),
    };
    let mut lines = vec![format!("type = \"{kind}\""), format!("name = \"{name}\"")];
    // tools are versioned and say what kind of tool they are
    if version {
        lines.push("version = \"latest\"".to_owned());
        lines.push("latest = true".to_owned());
    }
    if let Some(tool_kind) = tool_kind {
        lines.push(format!("kind = \"{tool_kind}\""));
    }
    lines.push(format!("groups = [\"{}\"]", toml_escape(group)));
    lines.push(format!("spec_from = \"{file}\""));
    // container tools are built from their own directory unless told not to
    if matches!(cmd.kind, InitToolKind::Image | InitToolKind::Harness) {
        let image_name = cmd.image_name.as_deref().unwrap_or(name);
        lines.push("build_path = \"./\"".to_owned());
        lines.push(format!("image_name = \"{}\"", toml_escape(image_name)));
        // a tool whose image is already published is marked, otherwise the default is documented
        lines.push(if cmd.no_build {
            "build_image = false".to_owned()
        } else {
            "# build_image = true".to_owned()
        });
        lines.extend(
            [
                "",
                "# [base_image]",
                "# image = \"\"",
                "# image_arg = \"IMAGE\"",
                "# token = \"\"",
                "# user = \"\"",
                "# allow_override = true",
            ]
            .map(str::to_owned),
        );
    }
    format!("{}\n", lines.join("\n"))
}

/// Scaffold a tool, toolset, agent, or workflow from `init tool` args
///
/// # Arguments
///
/// * `cmd` - The `init tool` args
pub(super) async fn init_tool(cmd: &InitTool) -> Result<(), Error> {
    // decide once whether prompts are available, then name the item and pick its group
    let interactive = resolve_interactive(cmd.non_interactive, "--group")?;
    let name = resolve_tool_name("tool", &cmd.path, interactive)?;
    let group = resolve_group(cmd.group.as_deref(), interactive)?;
    // lay out the manifest, the spec or document it points at, and a description for tools
    let (file, contents) = document(cmd, &name)?;
    let manifest = manifest(cmd, &name, &group, &file);
    write_file(&cmd.path.join("manifest.toml"), &manifest, cmd.overwrite).await?;
    write_file(&cmd.path.join(&file), &contents, cmd.overwrite).await?;
    if matches!(
        cmd.kind,
        InitToolKind::Image | InitToolKind::Agentic | InitToolKind::Harness
    ) {
        write_file(
            &cmd.path.join("description.md"),
            &description_stub(&name, None),
            cmd.overwrite,
        )
        .await?;
    }
    // point at the next step: fill in the spec, then build
    println!(
        "\n{} Fill in '{}' and keep '{}' under a toolbox root; run {} there to include it",
        "Init complete!".bright_green(),
        cmd.path.join(&file).display(),
        cmd.path.display(),
        "thorctl toolbox build".bright_cyan()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use thorium::models::ToolSpec;

    /// Build an `init tool` command for a kind
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind to scaffold
    /// * `no_build` - Whether the image is marked as not built
    fn command(kind: InitToolKind, no_build: bool) -> InitTool {
        // only the fields the scaffold reads matter
        InitTool {
            path: PathBuf::from("tools/scan"),
            kind,
            group: Some("static".to_owned()),
            image_name: None,
            no_build,
            non_interactive: true,
            overwrite: false,
        }
    }

    /// Every scaffolded tool spec is a spec the API parses
    #[test]
    fn scaffolded_specs_parse_as_tool_specs() {
        for spec in [
            IMAGE_SPEC.to_owned(),
            HARNESS_SPEC.to_owned(),
            agentic_spec("scan"),
        ] {
            // the YAML is read as JSON the way `toolbox build` reads it
            let value: serde_json::Value = serde_norway::from_str(&spec).expect("valid YAML");
            let json = serde_json::to_string(&value).expect("valid JSON");
            ToolSpec::parse_request(&json).expect("a spec the API parses");
        }
    }

    /// An image tool's manifest names its spec, its build context, and its group
    #[test]
    fn image_manifest_names_spec_build_and_group() {
        let text = manifest(
            &command(InitToolKind::Image, false),
            "scan",
            "static",
            "scan.json",
        );
        let table: toml::Table = text.parse().expect("valid TOML");
        assert_eq!(table["type"].as_str(), Some("tool"));
        assert_eq!(table["kind"].as_str(), Some("image"));
        assert_eq!(table["spec_from"].as_str(), Some("scan.json"));
        assert_eq!(table["build_path"].as_str(), Some("./"));
        assert_eq!(table["groups"][0].as_str(), Some("static"));
        // building is the default, so the switch is only documented
        assert!(table.get("build_image").is_none());
    }

    /// `--no-build` marks the image as already published
    #[test]
    fn no_build_turns_the_build_off() {
        let text = manifest(
            &command(InitToolKind::Image, true),
            "scan",
            "static",
            "scan.json",
        );
        let table: toml::Table = text.parse().expect("valid TOML");
        assert_eq!(table["build_image"].as_bool(), Some(false));
    }

    /// A tool's spec file is `<name>.json` holding the scaffolded spec, while toolsets, agents, and
    /// workflows keep their YAML documents
    #[test]
    fn tool_specs_are_json_files() {
        for kind in [
            InitToolKind::Image,
            InitToolKind::Agentic,
            InitToolKind::Harness,
        ] {
            let (file, text) = document(&command(kind, false), "scan").expect("a spec");
            assert_eq!(file, "scan.json");
            let value: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
            ToolSpec::parse_request(&value.to_string()).expect("a spec the API parses");
            assert!(text.ends_with("}\n"), "{text}");
        }
        let (file, _) = document(&command(InitToolKind::Agent, false), "lead").expect("a doc");
        assert_eq!(file, "lead.agent.yaml");
    }

    /// Toolsets, agents, and workflows are not versioned and have no build context
    #[test]
    fn entity_manifests_have_no_version_or_build() {
        for (kind, name) in [
            (InitToolKind::Toolset, "toolset"),
            (InitToolKind::Agent, "agent"),
            (InitToolKind::Workflow, "workflow"),
        ] {
            let text = manifest(&command(kind, false), "item", "static", "item.yaml");
            let table: toml::Table = text.parse().expect("valid TOML");
            assert_eq!(table["type"].as_str(), Some(name));
            assert!(table.get("version").is_none() && table.get("build_path").is_none());
        }
    }
}
