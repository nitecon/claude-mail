use sha2::{Digest, Sha256};

/// Sanitize an arbitrary project identity string into a valid Discord channel name.
/// Mirrors the logic in the gateway so MCP clients can predict the channel name.
pub fn sanitize_ident(raw: &str) -> String {
    let trimmed = raw.trim_end_matches(".git");

    let basename = trimmed
        .rsplit(|c| c == '/' || c == '\\')
        .find(|s| !s.is_empty())
        .unwrap_or(trimmed);

    let lower = basename.to_lowercase();

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

    let truncated = if stripped.len() > 100 {
        stripped[..100].trim_matches('-').to_string()
    } else {
        stripped
    };

    if truncated.is_empty() {
        let mut hasher = Sha256::new();
        hasher.update(raw.as_bytes());
        let hash = hasher.finalize();
        format!("project-{}", hex::encode(&hash[..4]))
    } else {
        truncated
    }
}
