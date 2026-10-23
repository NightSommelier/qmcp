use std::sync::Arc;

use axum::Router;
use rmcp::{
    ServiceExt,
    transport::{
        stdio,
        streamable_http_server::{
            StreamableHttpServerConfig, StreamableHttpService, session::never::NeverSessionManager,
        },
    },
};

use crate::QmcpServer;

/// Serve MCP over stdio. Only MCP JSON-RPC messages are written to stdout.
pub async fn serve_stdio(server: QmcpServer) -> anyhow::Result<()> {
    server.serve(stdio()).await?.waiting().await?;
    Ok(())
}

/// Build the single Streamable HTTP endpoint, with JSON responses and no SSE stream.
///
/// HTTP is stateless: QueryMT, not the MCP transport, owns agent sessions.
/// Host validation rejects DNS rebinding; browser Origins are rejected by default.
pub fn http_router(server: QmcpServer) -> Router {
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_sse_keep_alive(None)
        .with_sse_retry(None)
        .enforce_origin_validation();
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(NeverSessionManager::default()),
        config,
    );
    Router::new().nest_service("/mcp", service)
}
