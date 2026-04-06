mod client;
mod zip_util;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use client::SkillsClient;
use std::path::PathBuf;

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name = "agent-sync",
    about = "Sync skills, commands, and agents with the agent-comms gateway"
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
    /// Manage skills (directories or single files)
    Skills {
        #[command(subcommand)]
        action: ResourceAction,
    },
    /// Manage commands (.md files)
    Commands {
        #[command(subcommand)]
        action: ResourceAction,
    },
    /// Manage agents (.md files)
    Agents {
        #[command(subcommand)]
        action: ResourceAction,
    },
    /// Bidirectional sync: push new/changed local skills, commands, and agents; pull new remote ones
    Sync {
        /// Root directory containing skill subdirectories and command .md files (default: current directory)
        #[arg(long, default_value = ".")]
        dir: PathBuf,
    },
    /// Check for a newer version and update the binary in place
    Update,
}

#[derive(Subcommand)]
enum ResourceAction {
    /// Push a resource to the gateway
    Push {
        /// Path to skill directory, or .md file
        path: PathBuf,
    },
    /// Pull a resource from the gateway
    Pull {
        /// Name of the resource
        name: String,
        /// Destination directory
        #[arg(long, default_value = ".")]
        to: PathBuf,
    },
    /// List resources on the gateway
    List,
    /// Delete a resource from the gateway
    Delete {
        /// Name of the resource
        name: String,
    },
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .expect("could not determine home directory")
}

/// Sanitize a resource name into a valid gateway identifier.
/// Rules: lowercase, [a-z0-9_-] only, no leading/trailing hyphens,
/// collapsed consecutive hyphens, max 100 chars.
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
        eprintln!("Missing --url / GATEWAY_URL (run `agent-comms init` to configure)");
        std::process::exit(1);
    });
    let api_key = api_key.unwrap_or_else(|| {
        eprintln!("Missing --api-key / GATEWAY_API_KEY (run `agent-comms init` to configure)");
        std::process::exit(1);
    });
    SkillsClient::new(url, api_key, timeout_ms)
}

// ── Resource Commands ────────────────────────────────────────────────────────

/// Handle push for all resource types with smart file/directory detection.
async fn cmd_resource_push(client: &SkillsClient, path: PathBuf, kind: &str) -> Result<()> {
    let path = path.canonicalize().context("resolve path")?;

    if kind == "skill" {
        if path.is_dir() {
            // Directory push: zip and upload (requires SKILL.md).
            if !path.join("SKILL.md").exists() {
                bail!(
                    "'{}' does not contain SKILL.md — not a valid skill directory",
                    path.display()
                );
            }
            let name = sanitize_name(
                path.file_name()
                    .and_then(|n| n.to_str())
                    .context("skill directory has no name")?,
            );
            if name.is_empty() {
                bail!(
                    "could not derive a valid skill name from directory '{}'",
                    path.display()
                );
            }
            let (zip_bytes, checksum) = zip_util::zip_skill_dir(&path).context("zip skill")?;
            let size = zip_bytes.len();
            client.upload(&name, zip_bytes).await?;
            println!(
                "Pushed skill '{}' ({} bytes, checksum: {})",
                name,
                size,
                &checksum[..12]
            );
        } else if path.is_file() {
            // Single file push: upload as text/markdown with skill kind.
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if ext != "md" {
                bail!("skill file must have .md extension, got '.{}'", ext);
            }
            let stem = path
                .file_stem()
                .and_then(|n| n.to_str())
                .context("file has no name")?;
            let name = sanitize_name(stem);
            if name.is_empty() {
                bail!(
                    "could not derive a valid skill name from '{}'",
                    path.display()
                );
            }
            let content = std::fs::read_to_string(&path).context("read skill file")?;
            if content.is_empty() {
                bail!("skill file is empty");
            }
            let size = content.len();
            // Upload single-file skill using the skill upload (zip) path:
            // wrap the single file in a zip so the gateway treats it as a skill.
            let (zip_bytes, checksum) =
                zip_util::zip_single_file(&path).context("zip single skill file")?;
            client.upload(&name, zip_bytes).await?;
            println!(
                "Pushed skill '{}' ({} bytes raw, checksum: {})",
                name,
                size,
                &checksum[..12]
            );
        } else {
            bail!("'{}' is neither a file nor a directory", path.display());
        }
    } else {
        // Commands and agents: must be a .md file.
        if !path.is_file() {
            bail!("{} push requires a file, got directory", kind);
        }
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if ext != "md" {
            bail!("{} file must have .md extension, got '.{}'", kind, ext);
        }
        let stem = path
            .file_stem()
            .and_then(|n| n.to_str())
            .context("file has no name")?;
        let name = sanitize_name(stem);
        if name.is_empty() {
            bail!(
                "could not derive a valid {} name from '{}'",
                kind,
                path.display()
            );
        }
        let markdown = std::fs::read_to_string(&path).context("read file")?;
        if markdown.is_empty() {
            bail!("{} file is empty", kind);
        }
        let size = markdown.len();
        match kind {
            "command" => client.upload_command(&name, markdown).await?,
            "agent" => client.upload_agent(&name, markdown).await?,
            _ => unreachable!(),
        };
        println!("Pushed {} '{}' ({} bytes)", kind, name, size);
    }
    Ok(())
}

/// Pull a resource from the gateway, respecting its kind.
async fn cmd_resource_pull(
    client: &SkillsClient,
    name: String,
    to: PathBuf,
    kind: &str,
) -> Result<()> {
    let result = client.download(&name).await?;

    // Verify the downloaded resource matches the expected kind.
    let actual_kind = result.kind.as_str();
    let kind_matches = match kind {
        "skill" => actual_kind != "command" && actual_kind != "agent",
        other => actual_kind == other,
    };
    if !kind_matches {
        bail!(
            "'{}' is a {} on the gateway, not a {}",
            name,
            actual_kind,
            kind
        );
    }

    match kind {
        "skill" => {
            let dest = zip_util::unzip_skill(&name, &result.bytes, &to)?;
            println!("Pulled skill '{}' -> {}", name, dest.display());
        }
        "command" | "agent" => {
            let dest = to.join(format!("{}.md", name));
            std::fs::write(&dest, &result.bytes).context("write file")?;
            println!("Pulled {} '{}' -> {}", kind, name, dest.display());
        }
        _ => unreachable!(),
    }
    Ok(())
}

/// List resources on the gateway, filtered by kind.
async fn cmd_resource_list(client: &SkillsClient, kind: &str) -> Result<()> {
    let all = client.list().await?;
    let filtered: Vec<_> = all
        .into_iter()
        .filter(|s| match kind {
            "skill" => s.kind != "command" && s.kind != "agent",
            other => s.kind == other,
        })
        .collect();

    if filtered.is_empty() {
        println!("No {}s on gateway.", kind);
        return Ok(());
    }
    println!(
        "{:<30} {:<8} {:>10}  {:<14}  uploaded",
        "NAME", "KIND", "SIZE", "CHECKSUM"
    );
    println!("{}", "-".repeat(80));
    for s in &filtered {
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

/// Delete a resource from the gateway.
async fn cmd_resource_delete(client: &SkillsClient, name: String) -> Result<()> {
    client.delete(&name).await?;
    println!("Deleted '{}'", name);
    Ok(())
}

/// Dispatch a resource action to the appropriate handler.
async fn handle_resource(action: ResourceAction, client: &SkillsClient, kind: &str) -> Result<()> {
    match action {
        ResourceAction::Push { path } => cmd_resource_push(client, path, kind).await,
        ResourceAction::Pull { name, to } => cmd_resource_pull(client, name, to, kind).await,
        ResourceAction::List => cmd_resource_list(client, kind).await,
        ResourceAction::Delete { name } => cmd_resource_delete(client, name).await,
    }
}

// ── Timestamp Helpers ────────────────────────────────────────────────────────

fn chrono_or_raw(ms: i64) -> String {
    // Format Unix ms as a simple date string without a chrono dep.
    // Divides down to seconds, produces YYYY-MM-DD HH:MM UTC.
    let secs = ms / 1000;
    let days_since_epoch = secs / 86400;
    let time_of_day = secs % 86400;
    let h = time_of_day / 3600;
    let m = (time_of_day % 3600) / 60;

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

// ── Bidirectional Sync ───────────────────────────────────────────────────────

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
                    println!("  pulled agent '{}' -> {}", name, dest.display());
                }
                "command" => {
                    let dest = dir.join(format!("{}.md", name));
                    std::fs::write(&dest, &result.bytes)
                        .with_context(|| format!("write command {}", dest.display()))?;
                    println!("  pulled command '{}' -> {}", name, dest.display());
                }
                _ => {
                    let dest = zip_util::unzip_skill(name, &result.bytes, &dir)?;
                    println!("  pulled skill '{}' -> {}", name, dest.display());
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
    // Config loading: ~/.claude/agent-comms.conf -> .env -> env vars -> CLI flags.
    let conf_path = home_dir().join(".claude").join("agent-comms.conf");
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
                println!("Updating agent-sync {} -> {}...", current, version);
                updater::perform_update(&http, &version, "agent-sync").await?;
            }
        }
        return Ok(());
    }

    let client = require_client(cli.url, cli.api_key, cli.timeout_ms)?;

    match cli.command {
        Command::Skills { action } => handle_resource(action, &client, "skill").await,
        Command::Commands { action } => handle_resource(action, &client, "command").await,
        Command::Agents { action } => handle_resource(action, &client, "agent").await,
        Command::Sync { dir } => cmd_sync(&client, dir).await,
        Command::Update => unreachable!(),
    }
}
