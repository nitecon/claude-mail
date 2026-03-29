mod client;
mod zip_util;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use client::SkillsClient;
use std::path::PathBuf;

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name = "claude-mail-skills",
    about = "Manage shared Claude Code skills on the gateway"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Gateway base URL
    #[arg(long, env = "GATEWAY_URL", global = true)]
    url: Option<String>,

    /// Gateway API key
    #[arg(long, env = "GATEWAY_API_KEY", global = true)]
    api_key: Option<String>,

    /// HTTP timeout in milliseconds
    #[arg(
        long,
        env = "GATEWAY_TIMEOUT_MS",
        default_value = "10000",
        global = true
    )]
    timeout_ms: u64,
}

#[derive(Subcommand)]
enum Command {
    /// Zip and upload a skill directory (must contain SKILL.md)
    Push {
        /// Path to the skill directory
        skill_dir: PathBuf,
    },
    /// Download and extract a skill from the gateway
    Pull {
        /// Skill name
        name: String,
        /// Directory to extract into (default: current directory)
        #[arg(long, default_value = ".")]
        to: PathBuf,
    },
    /// List all skills on the gateway
    List,
    /// Delete a skill from the gateway
    Delete {
        /// Skill name
        name: String,
    },
    /// Check for a newer version and update the binary in place
    Update,
    /// Bidirectional sync: push new/changed local skills, pull new remote skills
    Sync {
        /// Root directory containing skill subdirectories (default: current directory)
        #[arg(long, default_value = ".")]
        dir: PathBuf,
    },
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .expect("could not determine home directory")
}

/// Sanitize a skill directory basename into a valid gateway skill name.
/// Same rules as gateway's sanitize_ident: lowercase, [a-z0-9_-] only,
/// no leading/trailing hyphens, max 100 chars.
fn sanitize_name(raw: &str) -> String {
    let lower = raw.to_lowercase();
    let replaced: String = lower
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();

    let mut collapsed = String::new();
    let mut prev_hyphen = false;
    for c in replaced.chars() {
        if c == '-' {
            if !prev_hyphen {
                collapsed.push(c);
            }
            prev_hyphen = true;
        } else {
            collapsed.push(c);
            prev_hyphen = false;
        }
    }

    let stripped = collapsed.trim_matches('-').to_string();
    if stripped.len() > 100 {
        stripped[..100].trim_matches('-').to_string()
    } else {
        stripped
    }
}

fn require_client(
    url: Option<String>,
    api_key: Option<String>,
    timeout_ms: u64,
) -> Result<SkillsClient> {
    let url = url.unwrap_or_else(|| {
        eprintln!("Missing --url / GATEWAY_URL (run `claude-mail init` to configure)");
        std::process::exit(1);
    });
    let api_key = api_key.unwrap_or_else(|| {
        eprintln!("Missing --api-key / GATEWAY_API_KEY (run `claude-mail init` to configure)");
        std::process::exit(1);
    });
    SkillsClient::new(url, api_key, timeout_ms)
}

// ── Commands ──────────────────────────────────────────────────────────────────

async fn cmd_push(client: &SkillsClient, skill_dir: PathBuf) -> Result<()> {
    let skill_dir = skill_dir
        .canonicalize()
        .context("resolve skill directory")?;
    if !skill_dir.join("SKILL.md").exists() {
        bail!(
            "'{}' does not contain SKILL.md — not a valid skill directory",
            skill_dir.display()
        );
    }
    let name = sanitize_name(
        skill_dir
            .file_name()
            .and_then(|n| n.to_str())
            .context("skill directory has no name")?,
    );
    if name.is_empty() {
        bail!(
            "could not derive a valid skill name from directory '{}'",
            skill_dir.display()
        );
    }

    let (zip_bytes, checksum) = zip_util::zip_skill_dir(&skill_dir).context("zip skill")?;
    let size = zip_bytes.len();
    client.upload(&name, zip_bytes).await?;
    println!(
        "Pushed '{}' ({} bytes, checksum: {})",
        name,
        size,
        &checksum[..12]
    );
    Ok(())
}

async fn cmd_pull(client: &SkillsClient, name: String, to: PathBuf) -> Result<()> {
    let bytes = client.download(&name).await?;
    let dest = zip_util::unzip_skill(&name, &bytes, &to)?;
    println!("Pulled '{}' → {}", name, dest.display());
    Ok(())
}

async fn cmd_list(client: &SkillsClient) -> Result<()> {
    let skills = client.list().await?;
    if skills.is_empty() {
        println!("No skills on gateway.");
        return Ok(());
    }
    println!(
        "{:<30} {:>10}  {:<14}  uploaded",
        "NAME", "SIZE", "CHECKSUM"
    );
    println!("{}", "-".repeat(70));
    for s in &skills {
        let ts = chrono_or_raw(s.uploaded_at);
        println!(
            "{:<30} {:>10}  {:<14}  {}",
            s.name,
            s.size,
            &s.checksum[..12],
            ts
        );
    }
    Ok(())
}

fn chrono_or_raw(ms: i64) -> String {
    // Format Unix ms as a simple date string without a chrono dep.
    // Divides down to seconds, produces YYYY-MM-DD HH:MM UTC.
    let secs = ms / 1000;
    // Simple calculation — good enough for a CLI timestamp display.
    let days_since_epoch = secs / 86400;
    let time_of_day = secs % 86400;
    let h = time_of_day / 3600;
    let m = (time_of_day % 3600) / 60;

    // Gregorian calendar calculation from days since Unix epoch.
    let (y, mo, d) = days_to_ymd(days_since_epoch);
    format!("{:04}-{:02}-{:02} {:02}:{:02} UTC", y, mo, d, h, m)
}

fn days_to_ymd(mut days: i64) -> (i64, i64, i64) {
    // Algorithm: https://howardhinnant.github.io/date_algorithms.html (civil_from_days)
    days += 719468;
    let era = if days >= 0 { days } else { days - 146096 } / 146097;
    let doe = days - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

async fn cmd_delete(client: &SkillsClient, name: String) -> Result<()> {
    client.delete(&name).await?;
    println!("Deleted '{}'", name);
    Ok(())
}

async fn cmd_sync(client: &SkillsClient, dir: PathBuf) -> Result<()> {
    let dir = dir.canonicalize().context("resolve sync directory")?;

    // Discover local skills (subdirs with SKILL.md).
    let local: Vec<(String, PathBuf)> = std::fs::read_dir(&dir)
        .context("read sync directory")?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter(|e| e.path().join("SKILL.md").exists())
        .map(|e| {
            let name = sanitize_name(&e.file_name().to_string_lossy());
            (name, e.path())
        })
        .filter(|(name, _)| !name.is_empty())
        .collect();

    let remote: std::collections::HashMap<String, _> = client
        .list()
        .await?
        .into_iter()
        .map(|s| (s.name.clone(), s))
        .collect();

    let mut pushed = 0usize;
    let mut pulled = 0usize;

    // Push local skills that are new or changed.
    for (name, path) in &local {
        let local_checksum = zip_util::checksum_skill_dir(path)?;
        let needs_push = match remote.get(name) {
            Some(r) => r.checksum != local_checksum,
            None => true,
        };
        if needs_push {
            let (zip_bytes, checksum) = zip_util::zip_skill_dir(path)?;
            let size = zip_bytes.len();
            client.upload(name, zip_bytes).await?;
            println!("  pushed '{}' ({} bytes, {})", name, size, &checksum[..12]);
            pushed += 1;
        }
    }

    // Pull remote skills that are not local.
    let local_names: std::collections::HashSet<&str> =
        local.iter().map(|(n, _)| n.as_str()).collect();
    for name in remote.keys() {
        if !local_names.contains(name.as_str()) {
            let bytes = client.download(name).await?;
            let dest = zip_util::unzip_skill(name, &bytes, &dir)?;
            println!("  pulled '{}' → {}", name, dest.display());
            pulled += 1;
        }
    }

    println!("Sync complete: {} pushed, {} pulled.", pushed, pulled);
    Ok(())
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    // Config loading: ~/.claude/claude-mail.conf → .env → env vars → CLI flags.
    let conf_path = home_dir().join(".claude").join("claude-mail.conf");
    let _ = dotenvy::from_path(&conf_path);
    let _ = dotenvy::dotenv();

    let cli = Cli::parse();

    // Handle update before building the gateway client (it doesn't need one).
    if let Command::Update = &cli.command {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .context("build http client")?;
        let current = env!("CARGO_PKG_VERSION");
        match updater::check_update(&http, current).await? {
            None => {
                println!("Already up to date (v{}).", current);
            }
            Some(version) => {
                println!("Updating claude-mail-skills {} -> {}...", current, version);
                updater::perform_update(&http, &version, "claude-mail-skills").await?;
            }
        }
        return Ok(());
    }

    let client = require_client(cli.url, cli.api_key, cli.timeout_ms)?;

    match cli.command {
        Command::Push { skill_dir } => cmd_push(&client, skill_dir).await,
        Command::Pull { name, to } => cmd_pull(&client, name, to).await,
        Command::List => cmd_list(&client).await,
        Command::Delete { name } => cmd_delete(&client, name).await,
        Command::Sync { dir } => cmd_sync(&client, dir).await,
        Command::Update => unreachable!(),
    }
}
