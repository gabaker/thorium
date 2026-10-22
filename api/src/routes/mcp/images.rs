//! The images related tools for the Thorium MCP server

use rmcp::ErrorData;
use rmcp::handler::server::tool::Extension as RmcpExtension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde_json::json;
use tracing::instrument;

use super::{ThoriumMCP, json_result};

/// The params needed to list images in a group
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ListImages {
    /// The name of the group to list images for
    pub group: String,
}

#[tool_router(router = images_router, vis = "pub")]
impl ThoriumMCP {
    /// Get the images in Thorium in a specific group
    ///
    /// # Arguments
    ///
    /// * `parts` - The request parts required to get a token for this tool
    #[tool(name = "list_images", description = "List the images in Thorium.")]
    #[instrument(name = "ThoriumMCP::list_images", skip(self, parts), err(Debug))]
    pub async fn list_images(
        &self,
        Parameters(params): Parameters<ListImages>,
        RmcpExtension(parts): RmcpExtension<axum::http::request::Parts>,
    ) -> Result<CallToolResult, ErrorData> {
        // get a thorium client
        let thorium = self.conf.client(&parts).await?;
        // list images in the static group for now
        let mut cursor = thorium.images.list(&params.group).details().limit(1000);
        // get this cursors data
        cursor.next().await?;
        // return the images as structured content and text
        json_result(&json!({"data": &cursor.details}))
    }
}
