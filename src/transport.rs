use std::{net::SocketAddr, sync::Arc};

use anyhow::{Result, bail};
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
/// Host validation accepts the canonical localhost/127.0.0.1/::1 names; browser
/// Origins are rejected. Use http_router_for_addr for an explicit loopback bind.
pub fn http_router(server: QmcpServer) -> Router {
    build_http_router(server, StreamableHttpServerConfig::default())
}

/// Build HTTP routing for a validated loopback listener, retaining Host/Origin
/// checks while accepting the exact IP chosen by the operator (including 127.0.0.2).
pub fn http_router_for_addr(server: QmcpServer, bind: SocketAddr) -> Result<Router> {
    if !bind.ip().is_loopback() {
        bail!(
            "HTTP must bind to a loopback address; use an authenticated TLS reverse proxy for remote access"
        );
    }
    let mut config = StreamableHttpServerConfig::default();
    let host = bind.ip().to_string();
    if !config.allowed_hosts.contains(&host) {
        config.allowed_hosts.push(host);
    }
    Ok(build_http_router(server, config))
}

fn build_http_router(server: QmcpServer, config: StreamableHttpServerConfig) -> Router {
    let config = config
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BridgeConfig, policy::PermissionPolicy};

    #[test]
    fn explicit_http_router_accepts_only_loopback_bind_addresses() {
        let server = QmcpServer::new(
            BridgeConfig::new("127.0.0.1:1", false, PermissionPolicy::Cancel).unwrap(),
        )
        .unwrap();
        for address in ["127.0.0.1:3031", "127.0.0.2:3031", "[::1]:3031"] {
            assert!(
                http_router_for_addr(server.clone(), address.parse().unwrap()).is_ok(),
                "{address}"
            );
        }
        for address in [
            "0.0.0.0:3031",
            "192.0.2.1:3031",
            "[::]:3031",
            "[2001:db8::1]:3031",
        ] {
            assert!(
                http_router_for_addr(server.clone(), address.parse().unwrap()).is_err(),
                "{address}"
            );
        }
    }
}
