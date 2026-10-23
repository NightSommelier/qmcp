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
    /// MCP transport: stdio or Streamable HTTP.
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

    /// Streamable HTTP listen address. Non-loopback binds require a bearer token.
    #[arg(long, default_value = "127.0.0.1:3031")]
    bind: SocketAddr,

    /// Shared HTTP bearer token. Prefer the environment variable to keep it out of argv.
    #[arg(long, env = "QMCP_BEARER_TOKEN", hide_env_values = true)]
    bearer_token: Option<String>,

    /// Additional allowed HTTP hostname or host:port (repeatable; requires a token).
    #[arg(long = "allowed-host", action = clap::ArgAction::Append)]
    allowed_hosts: Vec<String>,
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
        Transport::Stdio => {
            if cli.bearer_token.is_some() || !cli.allowed_hosts.is_empty() {
                bail!("HTTP authentication options require --transport http");
            }
            transport::serve_stdio(server).await
        }
        Transport::Http => {
            let token = cli
                .bearer_token
                .map(|value| transport::BearerToken::new(&value))
                .transpose()?;
            let router =
                transport::http_router_with_auth(server, cli.bind, token, &cli.allowed_hosts)?;
            let listener = tokio::net::TcpListener::bind(cli.bind)
                .await
                .context("bind Streamable HTTP listener")?;
            tracing::info!(address = %listener.local_addr()?, "serving MCP at /mcp");
            axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = tokio::signal::ctrl_c().await;
                })
                .await
                .context("serve Streamable HTTP")
        }
    }
}
