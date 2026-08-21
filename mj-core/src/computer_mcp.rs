//! Model-facing MCP transport for local macOS observations.
//!
//! The server is created only for an explicitly opted-in primary session. It
//! owns no policy beyond that scope: the backend enforces image limits and
//! macOS decides Screen Recording access.

use std::future::Future;

use agent_client_protocol::schema::v1::McpServer;
use anyhow::{Context as _, Result};
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    handler::server::{router::tool::ToolRouter, tool::ToolCallContext, wrapper::Parameters},
    model::{
        CallToolRequestParams, CallToolResult, Content, Implementation, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
    },
    service::RequestContext,
    tool, tool_router,
};
use tokio_util::sync::CancellationToken;

use crate::{
    computer::{ComputerBackend, Observation, ObserveArgs},
    computer_macos::MacosComputerBackend,
};

pub const MCP_SERVER_NAME: &str = "mj-computer";

const SERVER_GUIDANCE: &str = "LOCAL COMPUTER OBSERVATION: `computer_observe` captures the \
visible desktop on this Mac. Use it only when the user asks you to inspect the local desktop or \
an application on it. The returned image may contain sensitive information. This session exposes \
observation only: it cannot click, type, or otherwise control the computer.";

#[derive(Clone)]
struct McpHandler {
    backend: MacosComputerBackend,
    tool_router: ToolRouter<Self>,
}

#[tool_router(router = tool_router)]
impl McpHandler {
    fn new() -> Self {
        Self {
            backend: MacosComputerBackend::default(),
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        name = "computer_observe",
        description = "Capture a current PNG observation of one local macOS display or a desktop-point region. Use only when the user asks you to inspect the visible local desktop. The response includes the image and JSON geometry metadata. This tool observes only; it cannot control the computer."
    )]
    async fn computer_observe(
        &self,
        Parameters(args): Parameters<ObserveArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        match self.backend.observe(args, CancellationToken::new()).await {
            Ok(observation) => Ok(render_observation(observation)),
            Err(error) => Ok(CallToolResult::error(vec![Content::text(format!(
                "computer_observe failed: {error}"
            ))])),
        }
    }
}

fn render_observation(observation: Observation) -> CallToolResult {
    let metadata = serde_json::to_string(&observation.metadata)
        .unwrap_or_else(|error| format!("could not serialize observation metadata: {error}"));
    CallToolResult::success(vec![
        Content::image(
            observation.image.data_base64,
            observation.metadata.mime_type,
        ),
        Content::text(metadata),
    ])
}

impl ServerHandler for McpHandler {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                MCP_SERVER_NAME,
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(SERVER_GUIDANCE)
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = std::result::Result<ListToolsResult, McpError>> + Send + '_ {
        std::future::ready(Ok(ListToolsResult::with_all_items(
            self.tool_router.list_all(),
        )))
    }

    fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = std::result::Result<CallToolResult, McpError>> + Send + '_ {
        self.tool_router
            .call(ToolCallContext::new(self, request, context))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.tool_router.get(name).cloned()
    }
}

/// In-process MCP endpoint advertised to an opted-in primary ACP session.
pub struct ToolServer {
    bridge: crate::mcp_bridge::BridgeServer,
}

impl ToolServer {
    pub async fn start() -> Result<Self> {
        let bridge = crate::mcp_bridge::BridgeServer::start(MCP_SERVER_NAME, McpHandler::new())
            .await
            .context("start computer MCP bridge")?;
        Ok(Self { bridge })
    }

    pub fn advertised(&self) -> &McpServer {
        self.bridge.advertised()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computer::{
        DesktopPoint, DisplayId, EncodedImage, ObservationId, ObservationMetadata, PixelSize,
        SourceRegion,
    };

    #[test]
    fn tool_router_exposes_computer_observe() {
        let names = McpHandler::tool_router()
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["computer_observe".to_string()]);
    }

    #[test]
    fn observation_response_contains_png_and_metadata() {
        let result = render_observation(Observation {
            metadata: ObservationMetadata {
                observation_id: ObservationId("obs-1".to_string()),
                display_id: DisplayId("display-1".to_string()),
                display_origin: DesktopPoint { x: 0, y: 0 },
                display_pixel_size: PixelSize {
                    width: 100,
                    height: 100,
                },
                display_scale_x: 1.0,
                display_scale_y: 1.0,
                source_region: SourceRegion {
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 100,
                },
                returned_image_size: PixelSize {
                    width: 100,
                    height: 100,
                },
                mime_type: "image/png".to_string(),
                created_at_unix_ms: 1,
                expires_at_unix_ms: 2,
            },
            image: EncodedImage {
                data_base64: "iVBORw0KGgo=".to_string(),
            },
        });

        assert!(!result.is_error.unwrap_or(true));
        assert!(result.content[0].raw.as_image().is_some());
        let metadata = result.content[1]
            .raw
            .as_text()
            .expect("second content block must contain metadata");
        assert!(metadata.text.contains("obs-1"));
    }

    #[tokio::test]
    async fn tool_server_advertises_the_computer_mcp_bridge() {
        let server = ToolServer::start().await.expect("start computer server");
        let McpServer::Stdio(advertised) = server.advertised() else {
            panic!("computer server must use the MCP bridge");
        };
        assert_eq!(advertised.name, MCP_SERVER_NAME);
        assert_eq!(
            advertised.args.first().map(String::as_str),
            Some("mcp-bridge")
        );
        assert!(
            advertised
                .env
                .iter()
                .any(|variable| variable.name == crate::mcp_bridge::TOKEN_ENV)
        );
    }
}
