mod gateway;
mod tools;

use anyhow::{Context, Result};
use rmcp::ServiceExt;
use tokio::io::{stdin, stdout};
use tools::MailServer;

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env (best-effort)
    let _ = dotenvy::dotenv();

    // Log to stderr so it doesn't corrupt the stdio MCP stream.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_env("RUST_LOG")
                .add_directive("claude_mail=info".parse()?),
        )
        .init();

    let gateway_url = require_env("GATEWAY_URL");
    let api_key = require_env("GATEWAY_API_KEY");
    let timeout_ms: u64 = std::env::var("GATEWAY_TIMEOUT_MS")
        .unwrap_or_else(|_| "5000".into())
        .parse()
        .context("GATEWAY_TIMEOUT_MS must be a u64")?;

    // If DEFAULT_PROJECT_IDENT is set, auto-register on startup.
    let default_ident = std::env::var("DEFAULT_PROJECT_IDENT").ok();

    let gw = gateway::GatewayClient::new(gateway_url, api_key, timeout_ms)
        .context("build gateway client")?;

    let server = MailServer::new(gw.clone());

    // Auto-register default identity if configured.
    if let Some(ident) = default_ident {
        match gw.register_project(&ident).await {
            Ok(resp) => {
                server.set_default_ident(resp.ident);
            }
            Err(e) => {
                tracing::warn!("Failed to auto-register default identity '{ident}': {e}");
            }
        }
    }

    let transport = (stdin(), stdout());
    let running = server.serve(transport).await.context("serve MCP")?;
    running.waiting().await.context("MCP server closed")?;

    Ok(())
}

fn require_env(key: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| {
        eprintln!("Missing required environment variable: {key}");
        std::process::exit(1);
    })
}
