use std::sync::Arc;

use anyhow::{Context, Result, bail};
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, Implementation, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool, ToolAnnotations,
    },
    service::RequestContext,
};
use schemars::generate::SchemaSettings;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use qmacp::{client::AcpClient, error::CliError, policy::PermissionPolicy, url};

use crate::{
    commands::{self, Command},
    output::EventLog,
};

/// ACP connection settings shared by both MCP transports.
#[derive(Debug, Clone)]
pub struct BridgeConfig {
    url: String,
    endpoint: String,
    permission: PermissionPolicy,
}

impl BridgeConfig {
    /// Normalize and validate the ACP endpoint before starting either transport.
    pub fn new(endpoint: &str, allow_insecure: bool, permission: PermissionPolicy) -> Result<Self> {
        let url = url::normalize_acp_ws_url(endpoint, allow_insecure)?;
        let endpoint = url::safe_endpoint_label(&url)?;
        Ok(Self {
            url,
            endpoint,
            permission,
        })
    }
}

/// MCP server exposing qmacp operations over native ACP WebSocket connections.
#[derive(Clone)]
pub struct QmcpServer {
    config: Arc<BridgeConfig>,
    tools: Arc<Vec<Tool>>,
}

impl QmcpServer {
    /// Build the tool catalog without connecting to or mutating the ACP server.
    pub fn new(config: BridgeConfig) -> Result<Self> {
        Ok(Self {
            config: Arc::new(config),
            tools: Arc::new(tool_catalog()?),
        })
    }

    async fn execute(&self, command: Command, context: &RequestContext<RoleServer>) -> Value {
        let (event_tx, mut event_rx) = mpsc::channel(64);
        let client = match AcpClient::connect_with_events(
            &self.config.url,
            &self.config.endpoint,
            self.config.permission,
            event_tx,
        )
        .await
        {
            Ok(client) => client,
            Err(error) => {
                return failure(CliError::connection_with_cause(
                    &self.config.endpoint,
                    error,
                ));
            }
        };
        // Each invocation owns its ACP connection and event history. A long prompt
        // cannot block runtime/cancel/queue calls or mix their assistant text.
        let mut log = EventLog::default();
        let operation = commands::run(&client, command);
        tokio::pin!(operation);
        let result = loop {
            tokio::select! {
                result = &mut operation => break result.unwrap_or_else(failure),
                Some(event) = event_rx.recv() => log.push(event),
                _ = context.ct.cancelled() => break failure(CliError::Timeout(
                    "MCP request cancelled; the remote turn is not automatically cancelled; inspect runtime before retrying".into()
                )),
            }
        };
        while let Ok(event) = event_rx.try_recv() {
            log.push(event);
        }
        let mut result = result;
        result["textTruncated"] = json!(client.text_truncated().await);
        log.attach(result)
    }
}

impl ServerHandler for QmcpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("qmcp", env!("CARGO_PKG_VERSION")))
            .with_instructions("Control an existing QueryMT ACP WebSocket server. Discover capabilities, profiles, models, and session ids rather than inventing values. Keep returned sessionId values. Pass cwd for session operations. Timeouts and MCP cancellation do not cancel remote work: check runtime before retrying. Only close sessions when explicitly requested. Streaming ACP events are collected in each tool result.")
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        if request.and_then(|r| r.cursor).is_some() {
            return Err(ErrorData::invalid_params(
                "the tool catalog is not paginated",
                None,
            ));
        }
        Ok(ListToolsResult {
            tools: self.tools.as_ref().clone(),
            ..Default::default()
        })
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.tools.iter().find(|tool| tool.name == name).cloned()
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        if self.get_tool(&request.name).is_none() {
            return Err(ErrorData::method_not_found::<
                rmcp::model::CallToolRequestMethod,
            >());
        }
        let result = match commands::decode(&request.name, request.arguments.unwrap_or_default()) {
            Ok(command) => self.execute(command, &context).await,
            Err(error) => failure(CliError::Usage(format!("{error:#}"))),
        };
        let failed = result.get("ok") == Some(&Value::Bool(false))
            || result.get("error").is_some()
            || result.get("stopReason").and_then(Value::as_str) == Some("cancelled");
        let mut result = result;
        if failed {
            result["ok"] = json!(false);
        }
        let response = if failed {
            CallToolResult::structured_error(result)
        } else {
            CallToolResult::structured(result)
        };
        Ok(CallToolResponse::Complete(response))
    }
}

fn failure(error: CliError) -> Value {
    json!({ "ok": false, "error": error.to_string(), "code": error.exit_code().as_i32() })
}

fn tool_catalog() -> Result<Vec<Tool>> {
    // The same enum drives deserialization and tool schemas, so API changes
    // cannot silently leave tools/list out of sync with tools/call.
    let mut settings = SchemaSettings::draft2020_12();
    settings.inline_subschemas = true;
    let schema = settings
        .into_generator()
        .into_root_schema_for::<Command>()
        .to_value();
    let variants = schema
        .get("oneOf")
        .and_then(Value::as_array)
        .context("command schema must contain oneOf")?;
    let mut tools = Vec::with_capacity(variants.len());
    for variant in variants {
        let mut input = variant
            .as_object()
            .cloned()
            .context("command variant must be an object")?;
        let description = input
            .remove("description")
            .and_then(|v| v.as_str().map(str::to_owned))
            .context("tool must have a description")?;
        let properties = input
            .get_mut("properties")
            .and_then(Value::as_object_mut)
            .context("command variant must have properties")?;
        let tag = properties
            .remove("command")
            .context("command variant must have a discriminator")?;
        let name = tag
            .get("const")
            .and_then(Value::as_str)
            .context("command discriminator must be constant")?
            .to_owned();
        if let Some(required) = input.get_mut("required").and_then(Value::as_array_mut) {
            required.retain(|field| field != "command");
        }
        let read_only = matches!(
            name.as_str(),
            "caps"
                | "profiles"
                | "models"
                | "sessions"
                | "find"
                | "get_lifecycle"
                | "inspect"
                | "status"
                | "runtime"
                | "follow"
                | "watch"
        );
        let idempotent = read_only
            || matches!(
                name.as_str(),
                "set_lifecycle" | "pin" | "unpin" | "set_mode" | "set_model" | "set_effort"
            );
        let annotations = ToolAnnotations::new()
            .read_only(read_only)
            .idempotent(idempotent)
            .open_world(true);
        tools.push(Tool::new(name, description, input).with_annotations(annotations));
    }
    if tools.is_empty() {
        bail!("empty tool catalog");
    }
    Ok(tools)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_contains_self_contained_object_schemas_and_native_prompt_fields() {
        let tools = tool_catalog().unwrap();
        assert_eq!(tools.len(), 25);
        for tool in &tools {
            assert_eq!(tool.input_schema["type"], "object");
            assert!(
                !tool.input_schema["properties"]
                    .as_object()
                    .unwrap()
                    .contains_key("command")
            );
            assert!(
                !serde_json::to_string(&tool.input_schema)
                    .unwrap()
                    .contains("\"$ref\"")
            );
        }
        let prompt = tools.iter().find(|t| t.name == "prompt").unwrap();
        assert_eq!(prompt.input_schema["properties"]["text"]["type"], "string");
        assert_eq!(
            prompt.input_schema["properties"]["until_idle"]["default"],
            true
        );
    }
}
