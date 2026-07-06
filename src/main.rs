//! pty-mcp — low-footprint MCP server: persistent interactive PTY sessions and
//! passwordless sudo. stdio by default; `--http <addr>` for streamable HTTP.

mod askpass;
mod keys;
mod screen;
mod server;
mod session;
mod sudo;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};
use rmcp::ServiceExt;
use rmcp::transport::StreamableHttpService;
use rmcp::transport::stdio;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;

use crate::server::PtyServer;
use crate::session::SessionManager;

#[derive(Parser)]
#[command(name = "pty-mcp", version, about)]
struct Cli {
    /// Serve over streamable HTTP on this address instead of stdio (e.g. 127.0.0.1:8722).
    #[arg(long, value_name = "ADDR")]
    http: Option<String>,

    /// Kill sessions idle longer than this many seconds (0 disables). Default 1800.
    #[arg(long, default_value_t = 1800)]
    idle_timeout: u64,

    /// Default scrollback lines per session.
    #[arg(long, default_value_t = 1000)]
    scrollback: usize,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Internal: invoked by `sudo -A` as SUDO_ASKPASS to prompt for a password.
    #[command(hide = true)]
    Askpass {
        #[arg(default_value = "Password:")]
        prompt: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // askpass mode short-circuits before any logging/server setup.
    if let Some(Command::Askpass { prompt }) = &cli.command {
        askpass::run(prompt);
    }

    // Logs go to stderr so they never corrupt the stdio JSON-RPC stream.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "pty_mcp=info,rmcp=warn".into()),
        )
        .init();

    let mgr = SessionManager::new(Duration::from_secs(cli.idle_timeout), cli.scrollback);

    match cli.http {
        Some(addr) => serve_http(mgr, &addr).await,
        None => serve_stdio(mgr).await,
    }
}

async fn serve_stdio(mgr: Arc<SessionManager>) -> Result<()> {
    tracing::info!("pty-mcp serving on stdio");
    let service = PtyServer::new(mgr).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

async fn serve_http(mgr: Arc<SessionManager>, addr: &str) -> Result<()> {
    let service = StreamableHttpService::new(
        move || Ok(PtyServer::new(Arc::clone(&mgr))),
        Arc::new(LocalSessionManager::default()),
        Default::default(),
    );
    let app = axum::Router::new().fallback_service(service);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("pty-mcp serving streamable HTTP on {addr}");
    axum::serve(listener, app).await?;
    Ok(())
}
