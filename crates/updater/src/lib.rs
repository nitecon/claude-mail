use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::io::Read;

const REPO_OWNER: &str = "nitecon";
const REPO_NAME: &str = "claude-mail";

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
}

/// Return `Some(latest_version)` if the GitHub release is newer than `current`,
/// else `None`.
pub async fn check_update(client: &reqwest::Client, current: &str) -> Result<Option<String>> {
    let url = format!(
        "https://api.github.com/repos/{}/{}/releases/latest",
        REPO_OWNER, REPO_NAME
    );
    let resp = client
        .get(&url)
        .header("User-Agent", format!("claude-mail/{}", current))
        .send()
        .await
        .context("query GitHub releases")?;

    if !resp.status().is_success() {
        // Non-fatal -- network might be offline, rate limited, etc.
        return Ok(None);
    }

    let release: GithubRelease = resp.json().await.context("parse release JSON")?;
    let latest = release.tag_name.trim_start_matches('v').to_string();
    let curr = current.trim_start_matches('v');

    if is_newer(&latest, curr) {
        Ok(Some(release.tag_name))
    } else {
        Ok(None)
    }
}

/// Compare two semver-ish strings of the form `MAJOR.MINOR.PATCH`.
fn is_newer(latest: &str, current: &str) -> bool {
    fn parse(v: &str) -> (u32, u32, u32) {
        let mut parts = v.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
        (
            parts.next().unwrap_or(0),
            parts.next().unwrap_or(0),
            parts.next().unwrap_or(0),
        )
    }
    parse(latest) > parse(current)
}

/// Detect the current platform's Rust target triple.
pub fn current_target() -> Option<&'static str> {
    if cfg!(target_os = "linux") && cfg!(target_arch = "x86_64") {
        Some("x86_64-unknown-linux-gnu")
    } else if cfg!(target_os = "linux") && cfg!(target_arch = "aarch64") {
        Some("aarch64-unknown-linux-gnu")
    } else if cfg!(target_os = "macos") && cfg!(target_arch = "aarch64") {
        Some("aarch64-apple-darwin")
    } else if cfg!(target_os = "macos") && cfg!(target_arch = "x86_64") {
        Some("x86_64-apple-darwin")
    } else if cfg!(target_os = "windows") && cfg!(target_arch = "x86_64") {
        Some("x86_64-pc-windows-msvc")
    } else {
        None
    }
}

/// Download the release archive for `version` and `target`, extract `bin_name`,
/// write it to a temp file, then replace the running binary.
pub async fn perform_update(
    client: &reqwest::Client,
    version: &str,  // e.g. "v0.2.0"
    bin_name: &str, // e.g. "claude-mail-skills"
) -> Result<()> {
    let target = current_target().context("unsupported platform for auto-update")?;

    let ext = if cfg!(target_os = "windows") {
        "zip"
    } else {
        "tar.gz"
    };
    let archive_name = format!("claude-mail-{}-{}.{}", version, target, ext);
    let url = format!(
        "https://github.com/{}/{}/releases/download/{}/{}",
        REPO_OWNER, REPO_NAME, version, archive_name
    );

    eprintln!("Downloading {}...", url);
    let resp = client
        .get(&url)
        .header("User-Agent", "claude-mail/updater")
        .send()
        .await
        .context("download release archive")?;

    if !resp.status().is_success() {
        bail!("download failed: HTTP {}", resp.status());
    }

    let bytes = resp.bytes().await.context("read archive bytes")?;
    eprintln!("Downloaded {} bytes, extracting...", bytes.len());

    let binary_bytes = if cfg!(target_os = "windows") {
        extract_from_zip(&bytes, bin_name)?
    } else {
        extract_from_targz(&bytes, bin_name)?
    };

    // Write to a temp file next to the current binary.
    let current_exe = std::env::current_exe().context("locate current binary")?;
    let tmp_path = current_exe.with_extension("new");
    {
        let mut f = std::fs::File::create(&tmp_path).context("create temp binary")?;
        std::io::Write::write_all(&mut f, &binary_bytes).context("write new binary")?;
        // Make executable on Unix.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(0o755))
                .context("set executable permission")?;
        }
    }

    self_replace::self_replace(&tmp_path).context("replace binary")?;
    let _ = std::fs::remove_file(&tmp_path);

    eprintln!("Updated to {}. Please restart.", version);
    Ok(())
}

/// Extract a named binary from a `.tar.gz` archive.
fn extract_from_targz(bytes: &[u8], bin_name: &str) -> Result<Vec<u8>> {
    use flate2::read::GzDecoder;
    use tar::Archive;

    let cursor = std::io::Cursor::new(bytes);
    let gz = GzDecoder::new(cursor);
    let mut archive = Archive::new(gz);

    for entry in archive.entries().context("read tar entries")? {
        let mut entry = entry.context("read tar entry")?;
        let path = entry.path().context("entry path")?;
        let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if filename == bin_name {
            let mut data = Vec::new();
            entry
                .read_to_end(&mut data)
                .context("read binary from tar")?;
            return Ok(data);
        }
    }
    bail!("binary '{}' not found in archive", bin_name)
}

/// Extract a named binary from a `.zip` archive.
fn extract_from_zip(bytes: &[u8], bin_name: &str) -> Result<Vec<u8>> {
    let cursor = std::io::Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(cursor).context("open zip")?;
    let exe_name = format!("{}.exe", bin_name);
    for i in 0..archive.len() {
        let mut file = archive.by_index(i).context("zip entry")?;
        let filename = std::path::Path::new(file.name())
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");
        // Windows binaries have .exe suffix
        if filename == bin_name || filename == exe_name {
            let mut data = Vec::new();
            file.read_to_end(&mut data).context("read from zip")?;
            return Ok(data);
        }
    }
    bail!("binary '{}' not found in zip", bin_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_newer() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
    }

    #[test]
    fn test_current_target_returns_some() {
        // On any supported CI/dev machine this should return Some.
        assert!(current_target().is_some());
    }
}
