mod gateway;
mod tools;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rmcp::ServiceExt;
use std::io::{self, BufRead, Write};
use tokio::io::{stdin, stdout};
use tools::MailServer;

// ── CLI definition ────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "claude-mail", about = "claude-mail MCP server")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Gateway base URL
    #[arg(long, env = "GATEWAY_URL")]
    url: Option<String>,

    /// Gateway API key
    #[arg(long, env = "GATEWAY_API_KEY")]
    api_key: Option<String>,

    /// Default project identity (skips set_identity tool call)
    #[arg(long, env = "DEFAULT_PROJECT_IDENT")]
    default_project: Option<String>,

    /// HTTP timeout in milliseconds
    #[arg(long, env = "GATEWAY_TIMEOUT_MS", default_value = "5000")]
    timeout_ms: u64,
}

#[derive(Subcommand)]
enum Command {
    /// Interactive setup — creates ~/.claude/claude-mail.conf
    Init,
    /// Check for a newer version and update the binary in place
    Update,
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn home_dir() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
        .expect("Could not determine home directory")
}

fn prompt(label: &str) -> Result<String> {
    print!("{label}");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

// ── `init` subcommand ─────────────────────────────────────────────────────────

fn run_init() -> Result<()> {
    println!("claude-mail setup\n");

    let url = {
        let v = prompt("Gateway URL [http://localhost:7913]: ")?;
        if v.is_empty() {
            "http://localhost:7913".to_string()
        } else {
            v
        }
    };

    let api_key = rpassword::prompt_password("Gateway API key: ").context("read API key")?;
    if api_key.is_empty() {
        anyhow::bail!("API key cannot be empty");
    }

    let default_project = prompt("Default project identity (leave blank to skip): ")?;

    let timeout_ms = {
        let v = prompt("HTTP timeout ms [5000]: ")?;
        if v.is_empty() {
            "5000".to_string()
        } else {
            v
        }
    };

    // Write config file.
    let claude_dir = home_dir().join(".claude");
    std::fs::create_dir_all(&claude_dir).context("create ~/.claude directory")?;
    let conf_path = claude_dir.join("claude-mail.conf");

    let mut conf =
        format!("GATEWAY_URL={url}\nGATEWAY_API_KEY={api_key}\nGATEWAY_TIMEOUT_MS={timeout_ms}\n");
    if !default_project.is_empty() {
        conf.push_str(&format!("DEFAULT_PROJECT_IDENT={default_project}\n"));
    }

    std::fs::write(&conf_path, conf).context("write config file")?;

    println!("\nConfig written to {}", conf_path.display());
    println!("\nAdd the MCP server to Claude Code:");
    println!("  claude mcp add claude-mail -- /path/to/claude-mail");
    println!("\nOr with an explicit URL override:");
    println!("  claude mcp add claude-mail -- /path/to/claude-mail --url={url}");

    Ok(())
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    // Config loading priority (lowest to highest):
    //   1. ~/.claude/claude-mail.conf  (written by `claude-mail init`)
    //   2. .env in current directory
    //   3. Environment variables already set in the shell
    //   4. CLI flags
    //
    // dotenvy::from_path / dotenvy::dotenv do NOT override variables that are
    // already set in the environment, so env vars and CLI flags always win.
    let conf_path = home_dir().join(".claude").join("claude-mail.conf");
    let _ = dotenvy::from_path(&conf_path);
    let _ = dotenvy::dotenv();

    let cli = Cli::parse();

    if let Some(Command::Update) = cli.command {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .context("build http client")?;
        let current = env!("CARGO_PKG_VERSION");
        match updater::check_update(&client, current).await? {
            None => {
                println!("Already up to date (v{}).", current);
            }
            Some(version) => {
                println!("Updating claude-mail {} -> {}...", current, version);
                updater::perform_update(&client, &version, "claude-mail").await?;
            }
        }
        return Ok(());
    }

    if let Some(Command::Init) = cli.command {
        return run_init();
    }

    // Log to stderr so it doesn't corrupt the stdio MCP stream.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_env("RUST_LOG")
                .add_directive("claude_mail=info".parse()?),
        )
        .init();

    let url = cli.url.unwrap_or_else(|| {
        eprintln!("Missing --url / GATEWAY_URL (run `claude-mail init` to configure)");
        std::process::exit(1);
    });
    let api_key = cli.api_key.unwrap_or_else(|| {
        eprintln!("Missing --api-key / GATEWAY_API_KEY (run `claude-mail init` to configure)");
        std::process::exit(1);
    });

    let gw = gateway::GatewayClient::new(url, api_key, cli.timeout_ms)
        .context("build gateway client")?;

    let server = MailServer::new(gw.clone());

    // Auto-register default identity if configured.
    if let Some(ident) = cli.default_project {
        match gw.register_project(&ident, None).await {
            Ok(resp) => {
                server.set_default_ident(resp.ident, resp.channel_name);
            }
            Err(e) => {
                tracing::warn!("Failed to auto-register default identity '{ident}': {e}");
            }
        }
    }

    // ── Background update check (non-blocking) ────────────────────────────────
    {
        let check_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_default();
        let current = env!("CARGO_PKG_VERSION");
        tokio::spawn(async move {
            if let Ok(Some(v)) = updater::check_update(&check_client, current).await {
                eprintln!("claude-mail update available: {} (current: {})", v, current);
            }
        });
    }

    let transport = (stdin(), stdout());
    let running = server.serve(transport).await.context("serve MCP")?;
    running.waiting().await.context("MCP server closed")?;

    Ok(())
}
