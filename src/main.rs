use std::net::SocketAddr;

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use qmcp::{BridgeConfig, QmcpServer, policy::PermissionPolicy, transport};
use tracing_subscriber::EnvFilter;

const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("QMCP_GIT_SHA"), ")");

#[derive(Debug, Clone, Copy, Default, ValueEnum)]
enum Transport {
    #[default]
    Stdio,
    Http,
}

#[derive(Parser)]
#[command(name = "qmcp", version = VERSION, about = "MCP server for a running QueryMT ACP WebSocket server")]
struct Cli {
    /// MCP transport: stdio or Streamable HTTP. No legacy SSE transport.
    #[arg(long, value_enum, default_value = "stdio")]
    transport: Transport,

    /// ACP WebSocket endpoint (or host[:port][/path]).
    #[arg(
        short,
        long,
        env = "QMCP_ACP_URL",
        default_value = "ws://127.0.0.1:3030/acp/ws"
    )]
    url: String,

    /// Permit plaintext ACP WebSockets to a non-loopback host.
    #[arg(long)]
    allow_insecure: bool,

    /// ACP tool permission policy, delegated to qmacp. Use cancel to fail closed.
    #[arg(long, value_enum, default_value = "allow-once")]
    permission: PermissionPolicy,

    /// Streamable HTTP listen address. Loopback only; use a secured reverse proxy for remote access.
    #[arg(long, default_value = "127.0.0.1:3031")]
    bind: SocketAddr,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
    let server = QmcpServer::new(BridgeConfig::new(
        &cli.url,
        cli.allow_insecure,
        cli.permission,
    )?)?;
    match cli.transport {
        Transport::Stdio => transport::serve_stdio(server).await,
        Transport::Http => {
            if !cli.bind.ip().is_loopback() {
                bail!(
                    "HTTP must bind to a loopback address; use an authenticated TLS reverse proxy for remote access"
                );
            }
            let listener = tokio::net::TcpListener::bind(cli.bind)
                .await
                .context("bind Streamable HTTP listener")?;
            tracing::info!(address = %listener.local_addr()?, "serving MCP at /mcp");
            axum::serve(listener, transport::http_router(server))
                .with_graceful_shutdown(async {
                    let _ = tokio::signal::ctrl_c().await;
                })
                .await
                .context("serve Streamable HTTP")
        }
    }
}
