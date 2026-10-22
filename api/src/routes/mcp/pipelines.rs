//! The pipelines related tools for the Thorium MCP server

use rmcp::ErrorData;
use rmcp::handler::server::tool::Extension as RmcpExtension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde_json::json;
use tracing::instrument;

use super::{ThoriumMCP, json_result};

/// The params needed to list pipelines in a group
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ListPipelines {
    /// The name of the group to list pipelines for
    pub group: String,
}

#[tool_router(router = pipelines_router, vis = "pub")]
impl ThoriumMCP {
    /// Get weather information for a city (returns structured data)
    ///
    /// # Arguments
    ///
    /// * `parts` - The request parts required to get a token for this tool
    #[tool(
        name = "list_pipelines",
        description = "List the pipelines or tools in Thorium."
    )]
    #[instrument(name = "ThoriumMCP::list_pipelines", skip(self, parts), err(Debug))]
    pub async fn list_pipelines(
        &self,
        Parameters(params): Parameters<ListPipelines>,
        RmcpExtension(parts): RmcpExtension<axum::http::request::Parts>,
    ) -> Result<CallToolResult, ErrorData> {
        // get a thorium client
        let thorium = self.conf.client(&parts).await?;
        // list pipelines in a single group
        let mut cursor = thorium.pipelines.list(&params.group).details().limit(1000);
        // get this cursors data
        cursor.next().await?;
        // return the pipelines as structured content and text
        json_result(&json!({"data": &cursor.details}))
    }
}
