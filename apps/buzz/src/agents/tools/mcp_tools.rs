//! The MCP connector tools.

use ferrite_app::serde_json;
use ferrite_app::serde_json::Value;
use crate::agents::error::Result;
use super::{CloudToolRegistry, McpBinding};

impl<'a> CloudToolRegistry<'a> {
    /// Wire the MCP connector seam: `mcp_connectors`, `mcp_list_tools` and
    /// `mcp_call_tool` become dispatchable against whatever this workspace has
    /// connected. Called only where the `mcp-egress` handle exists, so the tools
    /// are never advertised on a deployment that cannot reach a connector.
    pub fn with_mcp(mut self) -> Self {
        self.mcp = Some(McpBinding);
        self
    }

    /// One MCP connector tool.
    ///
    /// Every failure is a SOFT tool result, like the web and Fabric seams: "that
    /// connector is not authenticated" is something the model should read and say
    /// in the channel, not something that should end the turn.
    pub(super) async fn invoke_mcp(&self, tool: &str, args: &[u8]) -> Result<Vec<u8>> {
        if self.mcp.is_none() {
            return Self::err(
                "MCP connectors are not available on this deployment: no mcp-egress handle \
                 is bound",
            );
        }
        let v: Value = serde_json::from_slice(args).unwrap_or(serde_json::json!({}));
        let outcome = match tool {
            "mcp_connectors" => crate::relay::mcp_surface::agent_connector_list(self.ctx)
                .await
                .map_err(|e| e.to_string()),
            "mcp_list_tools" => {
                let id = v
                    .get("connector")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim();
                if id.is_empty() {
                    return Self::err(
                        "mcp_list_tools requires a 'connector' id — call mcp_connectors first",
                    );
                }
                self.mcp_connector_tools(id).await
            }
            "mcp_call_tool" => {
                let id = v
                    .get("connector")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim();
                let name = v.get("tool").and_then(Value::as_str).unwrap_or("").trim();
                if id.is_empty() || name.is_empty() {
                    return Self::err(
                        "mcp_call_tool requires a 'connector' id and a 'tool' name — call \
                         mcp_list_tools for that connector's own tool names",
                    );
                }
                let arguments = v.get("arguments").cloned().unwrap_or_else(|| serde_json::json!({}));
                self.mcp_connector_call(id, name, arguments).await
            }
            other => return Self::err(format!("unknown mcp tool: {other}")),
        };
        match outcome {
            Ok(value) => Self::ok(serde_json::json!({ "ok": true, "result": value })),
            Err(e) => Self::ok(serde_json::json!({ "error": e, "kind": "mcp" })),
        }
    }

    pub(super) async fn mcp_connector_tools(&self, id: &str) -> std::result::Result<Value, String> {
        let (connector, credential) = crate::relay::mcp_surface::agent_connector(self.ctx, id)
            .await
            .map_err(|e| e.to_string())?;
        crate::relay::mcp_surface::connector_list_tools(self.ctx, connector, credential.as_ref())
            .await
            .map_err(|e| e.to_string())
    }

    pub(super) async fn mcp_connector_call(
        &self,
        id: &str,
        tool: &str,
        arguments: Value,
    ) -> std::result::Result<Value, String> {
        // The workspace credential is operator authority over the external
        // account; a persona spends it only for an operator, exactly as the
        // direct route does.
        crate::relay::mcp_surface::require_operator_requester(self.ctx, self.requester.as_deref())
            .await
            .map_err(|e| e.to_string())?;
        let (connector, credential) = crate::relay::mcp_surface::agent_connector(self.ctx, id)
            .await
            .map_err(|e| e.to_string())?;
        crate::relay::mcp_surface::connector_call_tool(
            self.ctx,
            connector,
            credential.as_ref(),
            tool,
            arguments,
        )
        .await
        .map_err(|e| e.to_string())
    }
}
