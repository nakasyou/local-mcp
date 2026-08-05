mod approvals;
mod config;
mod mcp;
mod sandbox;

use anyhow::{Result, ensure};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use clap::{Parser, Subcommand};
use std::net::SocketAddr;
use url::Url;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)]
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
        /// Require this bearer token for every HTTP request.
        #[arg(long, env = "LOCAL_MCP_BEARER_TOKEN", hide_env_values = true)]
        bearer_token: Option<String>,
        /// Generate and require a new cryptographically secure bearer token.
        #[arg(long, conflicts_with_all = ["bearer_token", "oauth_issuer"])]
        generate_bearer_token: bool,
        /// OAuth 2.0 authorization server issuer URL.
        #[arg(
            long,
            env = "LOCAL_MCP_OAUTH_ISSUER",
            requires_all = ["oauth_resource", "oauth_introspection_endpoint"],
            conflicts_with_all = ["bearer_token", "generate_bearer_token"]
        )]
        oauth_issuer: Option<Url>,
        /// Canonical public URI of this MCP resource (normally ending in /mcp).
        #[arg(long, env = "LOCAL_MCP_OAUTH_RESOURCE", requires = "oauth_issuer")]
        oauth_resource: Option<Url>,
        /// RFC 7662 token introspection endpoint.
        #[arg(
            long,
            env = "LOCAL_MCP_OAUTH_INTROSPECTION_ENDPOINT",
            requires = "oauth_issuer"
        )]
        oauth_introspection_endpoint: Option<Url>,
        /// Client ID used to authenticate to the introspection endpoint.
        #[arg(long, env = "LOCAL_MCP_OAUTH_CLIENT_ID", requires = "oauth_issuer")]
        oauth_client_id: Option<String>,
        /// Client secret used to authenticate to the introspection endpoint.
        #[arg(
            long,
            env = "LOCAL_MCP_OAUTH_CLIENT_SECRET",
            hide_env_values = true,
            requires = "oauth_client_id"
        )]
        oauth_client_secret: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Start { session_id: None }) {
        Command::Start { session_id } => approvals::start(session_id.as_deref()).await,
        Command::Mcp => mcp::serve().await,
        Command::McpHttp {
            bind,
            mut bearer_token,
            generate_bearer_token,
            oauth_issuer,
            oauth_resource,
            oauth_introspection_endpoint,
            oauth_client_id,
            oauth_client_secret,
        } => {
            if generate_bearer_token {
                let token = generate_bearer_token_value()?;
                eprintln!("Generated bearer token: {token}");
                bearer_token = Some(token);
            }
            if let Some(token) = bearer_token.as_deref() {
                ensure!(!token.is_empty(), "bearer token must not be empty");
            }
            let oauth = oauth_issuer.map(|issuer| mcp::OAuthConfig {
                issuer,
                resource: oauth_resource.expect("required by clap"),
                introspection_endpoint: oauth_introspection_endpoint.expect("required by clap"),
                client_id: oauth_client_id,
                client_secret: oauth_client_secret,
            });
            mcp::serve_http(bind, bearer_token, oauth).await
        }
    }
}

fn generate_bearer_token_value() -> Result<String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_distinct_url_safe_256_bit_tokens() {
        let first = generate_bearer_token_value().unwrap();
        let second = generate_bearer_token_value().unwrap();

        assert_eq!(first.len(), 43);
        assert!(
            first
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
        );
        assert_ne!(first, second);
    }
}
