mod approvals;
mod config;
mod mcp;
mod sandbox;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::net::SocketAddr;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Start a session in the current directory and show its permission UI.
    Start {
        /// Session ID to use instead of generating a UUID.
        session_id: Option<String>,
    },
    /// Run the session-independent MCP server over stdin/stdout.
    Mcp,
    /// Run the session-independent MCP server over Streamable HTTP.
    McpHttp {
        /// Address on which to listen. Defaults to loopback for safety.
        #[arg(long, default_value = "127.0.0.1:3000")]
        bind: SocketAddr,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Start { session_id: None }) {
        Command::Start { session_id } => approvals::start(session_id.as_deref()).await,
        Command::Mcp => mcp::serve().await,
        Command::McpHttp { bind } => mcp::serve_http(bind).await,
    }
}
