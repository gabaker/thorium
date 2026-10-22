//! The run-scoped view of the Thorium MCP server
//!
//! With a run-scoped token, `tools/list` shows only the MCP tools the run's tools use, each
//! described from the run tool backed by it, and results of those tools are checked against the
//! describing run tool's output schema. Calls aren't narrowed: a tool left out of the listing
//! still answers with the token's normal limits.

use rmcp::ErrorData;
use rmcp::model::{CallToolResult, Tool};
use std::collections::HashMap;
use uuid::Uuid;

use crate::client::Thorium;
use crate::models::{AgenticTool, AgenticToolBacking, EntityMetadata, RunTokenScope, ToolSpec};

/// The tools that start and read tool runs, listed when a run has a `Container` tool
const TOOL_RUN_TOOLS: [&str; 3] = ["run_tool", "get_tool_run", "get_tool_run_results"];

/// The run tools of a run-scoped token's run, by the MCP tools they use
#[derive(Debug, Default)]
pub(super) struct RunTools {
    /// The run tool describing each MCP tool, by the MCP tool's name
    describers: HashMap<String, AgenticTool>,
    /// Whether the run has a `Container` tool, which it runs through the tool run tools
    container: bool,
}

impl RunTools {
    /// Collect the run tools of a run from its tool instances
    ///
    /// When several run tools are backed by one MCP tool, the one with the lowest tool
    /// instance id describes it.
    ///
    /// # Arguments
    ///
    /// * `instances` - The run's tool instances and their specs
    pub(super) fn new(mut instances: Vec<(Uuid, ToolSpec)>) -> Self {
        // visit the instances from the lowest id so the lowest describes a shared MCP tool
        instances.sort_by_key(|(id, _)| *id);
        let mut tools = RunTools::default();
        for (_, spec) in instances {
            // only agentic tools are run tools; the Image tools they pin aren't
            let ToolSpec::Agentic(tool) = spec else {
                continue;
            };
            match &tool.backing {
                // a Thorium MCP tool describes the MCP tool backing it unless a lower id does
                AgenticToolBacking::ThoriumMcp { tool: name } => {
                    tools.describers.entry(name.clone()).or_insert(tool);
                }
                // a container tool is run through the tool run tools
                AgenticToolBacking::Container(_) => tools.container = true,
                // harness-native tools never reach the MCP server
                AgenticToolBacking::HarnessNative { .. } => (),
            }
        }
        tools
    }

    /// Read the run tools of a run-scoped token's run
    ///
    /// # Arguments
    ///
    /// * `thorium` - A client authenticated with the run-scoped token
    /// * `scope` - The run the token is limited to
    pub(super) async fn load(thorium: &Thorium, scope: &RunTokenScope) -> Result<Self, ErrorData> {
        // read every tool instance in the token's scope at once
        let reads = futures::future::join_all(
            scope
                .tool_instances
                .iter()
                .map(|id| thorium.entities.get(*id)),
        )
        .await;
        // keep the spec of each tool instance
        let mut instances = Vec::with_capacity(reads.len());
        for read in reads {
            let entity = read?;
            if let EntityMetadata::ToolInstance(instance) = entity.metadata {
                instances.push((entity.id, instance.spec));
            }
        }
        Ok(Self::new(instances))
    }

    /// List the server's tools the run uses, each described from its run tool
    ///
    /// # Arguments
    ///
    /// * `server` - Every tool the server serves
    pub(super) fn list(&self, server: Vec<Tool>) -> Vec<Tool> {
        // keep only the tools the run uses
        server
            .into_iter()
            .filter_map(|tool| match self.describers.get(tool.name.as_ref()) {
                // a tool backing a run tool is described from it with the server's input schema
                Some(run_tool) => {
                    let mut described = run_tool.to_mcp_tool();
                    described.name = tool.name;
                    described.input_schema = tool.input_schema;
                    Some(described)
                }
                // the tool run tools serve the run's container tools as the server describes them
                None if self.container && TOOL_RUN_TOOLS.contains(&tool.name.as_ref()) => {
                    Some(tool)
                }
                // any other tool isn't used by the run
                None => None,
            })
            .collect()
    }

    /// Check a result of an MCP tool against the output schema of the run tool describing it
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the MCP tool that was called
    /// * `result` - The tool's result
    pub(super) fn check(&self, name: &str, result: CallToolResult) -> CallToolResult {
        // check the result with the run tool describing the tool, if one does
        match self.describers.get(name) {
            Some(run_tool) => run_tool.check_mcp_result(result),
            None => result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::RunTools;
    use crate::models::{
        AgenticTool, AgenticToolBacking, ContainerToolBacking, ToolIcon, ToolResultFormat, ToolSpec,
    };
    use crate::routes::mcp::ThoriumMCP;
    use rmcp::model::{CallToolResult, ContentBlock, Tool};
    use serde_json::json;
    use uuid::Uuid;

    /// Build a run tool backed by one of the server's MCP tools
    ///
    /// # Arguments
    ///
    /// * `backing` - The MCP tool backing the run tool
    /// * `title` - The run tool's title
    fn mcp_run_tool(backing: &str, title: &str) -> AgenticTool {
        // a run tool with its own description of the MCP tool
        AgenticTool {
            function_name: format!("{backing}_for_run"),
            title: Some(title.to_owned()),
            model_description: format!("{title} for this run"),
            backing: AgenticToolBacking::ThoriumMcp {
                tool: backing.to_owned(),
            },
            result_format: ToolResultFormat::Json,
            ..AgenticTool::default()
        }
    }

    /// A run tool whose results must carry a sha256
    fn described_sample_tool() -> AgenticTool {
        // a titled run tool with an output schema and an icon
        let mut tool = mcp_run_tool("get_sample", "Read the sample");
        tool.output_schema = Some(
            json!({"type": "object", "properties": {"sha256": {"type": "string"}}, "required": ["sha256"]})
                .as_object()
                .cloned()
                .unwrap_or_default(),
        );
        tool.icons = vec![ToolIcon {
            src: "https://example.com/sample.png".to_owned(),
            mime_type: Some("image/png".to_owned()),
            sizes: vec!["48x48".to_owned()],
            theme: None,
        }];
        tool
    }

    /// Find a listed tool by name
    ///
    /// # Arguments
    ///
    /// * `tools` - The listed tools
    /// * `name` - The name of the tool to find
    fn find<'a>(tools: &'a [Tool], name: &str) -> &'a Tool {
        // the tool must be listed
        tools
            .iter()
            .find(|tool| tool.name == name)
            .unwrap_or_else(|| panic!("{name} isn't listed"))
    }

    /// A run's listing holds only the MCP tools its run tools use, each described from the run
    /// tool with the lowest instance id and the server's input schema, plus the tool run tools
    /// when it has a container tool
    #[test]
    fn run_tools_list_only_the_runs_tools() {
        // two run tools share get_sample; the lower id describes it in any order
        let low = Uuid::from_u128(1);
        let high = Uuid::from_u128(2);
        let results = Uuid::from_u128(3);
        let container = Uuid::from_u128(4);
        let described = described_sample_tool();
        let instances = vec![
            (
                high,
                ToolSpec::Agentic(mcp_run_tool("get_sample", "Other sample")),
            ),
            (
                results,
                ToolSpec::Agentic(mcp_run_tool("get_sample_results", "Results")),
            ),
            (low, ToolSpec::Agentic(described.clone())),
            (
                container,
                ToolSpec::Agentic(AgenticTool {
                    backing: AgenticToolBacking::Container(ContainerToolBacking::default()),
                    ..mcp_run_tool("unused", "Scan")
                }),
            ),
        ];
        let server = ThoriumMCP::router().list_all();
        let listed = RunTools::new(instances.clone()).list(server.clone());
        // only the run's MCP tools and the tool run tools are listed
        let mut names = listed
            .iter()
            .map(|tool| tool.name.to_string())
            .collect::<Vec<String>>();
        names.sort();
        assert_eq!(
            names,
            vec![
                "get_sample",
                "get_sample_results",
                "get_tool_run",
                "get_tool_run_results",
                "run_tool"
            ]
        );
        // get_sample is described from the lower id's run tool with the server's input schema
        let sample = find(&listed, "get_sample");
        let expected = described.to_mcp_tool();
        assert_eq!(sample.title, expected.title);
        assert_eq!(sample.description, expected.description);
        assert_eq!(sample.annotations, expected.annotations);
        assert_eq!(sample.output_schema, expected.output_schema);
        assert_eq!(sample.icons, expected.icons);
        assert_eq!(
            sample.input_schema,
            find(&server, "get_sample").input_schema
        );
        // the tool run tools keep the server's own description
        assert_eq!(find(&listed, "run_tool"), find(&server, "run_tool"));
        // without the container tool the tool run tools aren't listed
        let without = instances
            .into_iter()
            .filter(|(id, _)| *id != container)
            .collect::<Vec<_>>();
        let listed = RunTools::new(without).list(server);
        assert_eq!(listed.len(), 2);
        assert!(listed.iter().all(|tool| !tool.name.contains("tool_run")));
    }

    /// A run tool's result is checked against its output schema, and results of tools no run
    /// tool describes are left alone
    #[test]
    fn run_tools_check_results_against_the_run_tool() {
        // a run whose get_sample results must carry a sha256
        let run_tools = RunTools::new(vec![(
            Uuid::from_u128(1),
            ToolSpec::Agentic(described_sample_tool()),
        )]);
        // a matching result is kept
        let matching = CallToolResult::structured(json!({"sha256": "abc"}));
        assert_eq!(run_tools.check("get_sample", matching.clone()), matching);
        // a result missing the property is an error naming it
        let missing = run_tools.check(
            "get_sample",
            CallToolResult::structured(json!({"md5": "x"})),
        );
        assert_eq!(missing.is_error, Some(true));
        let text = missing.content[0]
            .as_text()
            .map_or_else(String::new, |text| text.text.clone());
        assert!(text.contains("sha256"), "{text}");
        // a tool no run tool describes keeps its result
        let other = CallToolResult::success(vec![ContentBlock::text("listed")]);
        assert_eq!(run_tools.check("list_images", other.clone()), other);
    }
}
