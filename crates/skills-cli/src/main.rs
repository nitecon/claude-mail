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
    /// Upload a single markdown file as a command
    PushCmd {
        /// Path to the .md file
        file: PathBuf,
    },
    /// Upload a single markdown file as an agent
    PushAgent {
        /// Path to the .md file
        file: PathBuf,
    },
    /// Download and extract a skill, command, or agent from the gateway
    Pull {
        /// Name of the skill, command, or agent
        name: String,
        /// Directory to extract into (default: current directory)
        #[arg(long, default_value = ".")]
        to: PathBuf,
    },
    /// List all skills, commands, and agents on the gateway
    List,
    /// Delete a skill, command, or agent from the gateway
    Delete {
        /// Name of the skill, command, or agent
        name: String,
    },
    /// Check for a newer version and update the binary in place
    Update,
    /// Bidirectional sync: push new/changed local skills, commands, and agents; pull new remote ones
    Sync {
        /// Root directory containing skill subdirectories and command .md files (default: current directory)
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

async fn cmd_push_command(client: &SkillsClient, file: PathBuf) -> Result<()> {
    let file = file.canonicalize().context("resolve command file")?;
    if !file.is_file() {
        bail!("'{}' is not a file", file.display());
    }
    let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("");
    if ext != "md" {
        bail!("command file must have .md extension, got '.{}'", ext);
    }

    let stem = file
        .file_stem()
        .and_then(|n| n.to_str())
        .context("file has no name")?;
    let name = sanitize_name(stem);
    if name.is_empty() {
        bail!(
            "could not derive a valid command name from '{}'",
            file.display()
        );
    }

    let markdown = std::fs::read_to_string(&file).context("read command file")?;
    if markdown.is_empty() {
        bail!("command file is empty");
    }
    let size = markdown.len();
    client.upload_command(&name, markdown).await?;
    println!("Pushed command '{}' ({} bytes)", name, size);
    Ok(())
}

async fn cmd_push_agent(client: &SkillsClient, file: PathBuf) -> Result<()> {
    let file = file.canonicalize().context("resolve agent file")?;
    if !file.is_file() {
        bail!("'{}' is not a file", file.display());
    }
    let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("");
    if ext != "md" {
        bail!("agent file must have .md extension, got '.{}'", ext);
    }

    let stem = file
        .file_stem()
        .and_then(|n| n.to_str())
        .context("file has no name")?;
    let name = sanitize_name(stem);
    if name.is_empty() {
        bail!(
            "could not derive a valid agent name from '{}'",
            file.display()
        );
    }

    let markdown = std::fs::read_to_string(&file).context("read agent file")?;
    if markdown.is_empty() {
        bail!("agent file is empty");
    }
    let size = markdown.len();
    client.upload_agent(&name, markdown).await?;
    println!("Pushed agent '{}' ({} bytes)", name, size);
    Ok(())
}

async fn cmd_pull(client: &SkillsClient, name: String, to: PathBuf) -> Result<()> {
    let result = client.download(&name).await?;
    match result.kind.as_str() {
        "command" | "agent" => {
            let dest = to.join(format!("{}.md", name));
            std::fs::write(&dest, &result.bytes).context("write file")?;
            println!("Pulled {} '{}' → {}", result.kind, name, dest.display());
        }
        _ => {
            let dest = zip_util::unzip_skill(&name, &result.bytes, &to)?;
            println!("Pulled skill '{}' → {}", name, dest.display());
        }
    }
    Ok(())
}

async fn cmd_list(client: &SkillsClient) -> Result<()> {
    let skills = client.list().await?;
    if skills.is_empty() {
        println!("No skills or commands on gateway.");
        return Ok(());
    }
    println!(
        "{:<30} {:<8} {:>10}  {:<14}  uploaded",
        "NAME", "KIND", "SIZE", "CHECKSUM"
    );
    println!("{}", "-".repeat(80));
    for s in &skills {
        let ts = chrono_or_raw(s.uploaded_at);
        println!(
            "{:<30} {:<8} {:>10}  {:<14}  {}",
            s.name,
            s.kind,
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
    use sha2::{Digest, Sha256};
    use std::collections::{HashMap, HashSet};

    let dir = dir.canonicalize().context("resolve sync directory")?;

    // Discover local skills (subdirs with SKILL.md).
    let local_skills: Vec<(String, PathBuf)> = std::fs::read_dir(&dir)
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

    let skill_names: HashSet<&str> = local_skills.iter().map(|(n, _)| n.as_str()).collect();

    // Discover local commands (top-level .md files, excluding uppercase-prefixed like README.md).
    let local_commands: Vec<(String, PathBuf)> = std::fs::read_dir(&dir)
        .context("read sync directory")?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext == "md")
                .unwrap_or(false)
        })
        .filter(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            !name.starts_with(|c: char| c.is_uppercase())
        })
        .filter_map(|e| {
            let stem = e.path().file_stem()?.to_string_lossy().to_string();
            let name = sanitize_name(&stem);
            if name.is_empty() {
                return None;
            }
            if skill_names.contains(name.as_str()) {
                eprintln!(
                    "  warning: command '{}' conflicts with skill directory; skipping command",
                    name
                );
                return None;
            }
            Some((name, e.path()))
        })
        .collect();

    let remote: HashMap<String, _> = client
        .list()
        .await?
        .into_iter()
        .map(|s| (s.name.clone(), s))
        .collect();

    let mut pushed = 0usize;
    let mut pulled = 0usize;

    // Push local skills that are new or changed.
    for (name, path) in &local_skills {
        let local_checksum = zip_util::checksum_skill_dir(path)?;
        let needs_push = match remote.get(name) {
            Some(r) => r.checksum != local_checksum,
            None => true,
        };
        if needs_push {
            let (zip_bytes, checksum) = zip_util::zip_skill_dir(path)?;
            let size = zip_bytes.len();
            client.upload(name, zip_bytes).await?;
            println!(
                "  pushed skill '{}' ({} bytes, {})",
                name,
                size,
                &checksum[..12]
            );
            pushed += 1;
        }
    }

    // Push local commands that are new or changed.
    for (name, path) in &local_commands {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read command file {}", path.display()))?;
        let local_checksum = {
            let mut hasher = Sha256::new();
            hasher.update(text.as_bytes());
            hex::encode(hasher.finalize())
        };
        let needs_push = match remote.get(name) {
            Some(r) => r.checksum != local_checksum,
            None => true,
        };
        if needs_push {
            let size = text.len();
            client.upload_command(name, text).await?;
            println!(
                "  pushed command '{}' ({} bytes, {})",
                name,
                size,
                &local_checksum[..12]
            );
            pushed += 1;
        }
    }

    // Discover local agents (~/.claude/agents/*.md).
    let agents_dir = home_dir().join(".claude").join("agents");
    let cmd_names: HashSet<&str> = local_commands.iter().map(|(n, _)| n.as_str()).collect();
    let local_agents: Vec<(String, PathBuf)> = if agents_dir.is_dir() {
        std::fs::read_dir(&agents_dir)
            .context("read agents directory")?
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
            .filter(|e| {
                e.path()
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .map(|ext| ext == "md")
                    .unwrap_or(false)
            })
            .filter_map(|e| {
                let stem = e.path().file_stem()?.to_string_lossy().to_string();
                let name = sanitize_name(&stem);
                if name.is_empty() {
                    return None;
                }
                if skill_names.contains(name.as_str()) || cmd_names.contains(name.as_str()) {
                    eprintln!(
                        "  warning: agent '{}' conflicts with existing skill/command; skipping",
                        name
                    );
                    return None;
                }
                Some((name, e.path()))
            })
            .collect()
    } else {
        vec![]
    };

    // Push local agents that are new or changed.
    for (name, path) in &local_agents {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read agent file {}", path.display()))?;
        let local_checksum = {
            let mut hasher = Sha256::new();
            hasher.update(text.as_bytes());
            hex::encode(hasher.finalize())
        };
        let needs_push = match remote.get(name) {
            Some(r) => r.checksum != local_checksum,
            None => true,
        };
        if needs_push {
            let size = text.len();
            client.upload_agent(name, text).await?;
            println!(
                "  pushed agent '{}' ({} bytes, {})",
                name,
                size,
                &local_checksum[..12]
            );
            pushed += 1;
        }
    }

    // Pull remote entries that are not local.
    let local_names: HashSet<&str> = local_skills
        .iter()
        .map(|(n, _)| n.as_str())
        .chain(local_commands.iter().map(|(n, _)| n.as_str()))
        .chain(local_agents.iter().map(|(n, _)| n.as_str()))
        .collect();
    for name in remote.keys() {
        if !local_names.contains(name.as_str()) {
            let result = client.download(name).await?;
            match result.kind.as_str() {
                "agent" => {
                    std::fs::create_dir_all(&agents_dir)?;
                    let dest = agents_dir.join(format!("{}.md", name));
                    std::fs::write(&dest, &result.bytes)
                        .with_context(|| format!("write agent {}", dest.display()))?;
                    println!("  pulled agent '{}' → {}", name, dest.display());
                }
                "command" => {
                    let dest = dir.join(format!("{}.md", name));
                    std::fs::write(&dest, &result.bytes)
                        .with_context(|| format!("write command {}", dest.display()))?;
                    println!("  pulled command '{}' → {}", name, dest.display());
                }
                _ => {
                    let dest = zip_util::unzip_skill(name, &result.bytes, &dir)?;
                    println!("  pulled skill '{}' → {}", name, dest.display());
                }
            }
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
        Command::PushCmd { file } => cmd_push_command(&client, file).await,
        Command::PushAgent { file } => cmd_push_agent(&client, file).await,
        Command::Pull { name, to } => cmd_pull(&client, name, to).await,
        Command::List => cmd_list(&client).await,
        Command::Delete { name } => cmd_delete(&client, name).await,
        Command::Sync { dir } => cmd_sync(&client, dir).await,
        Command::Update => unreachable!(),
    }
}
