use sha2::{Digest, Sha256};

/// Sanitize an arbitrary project identity string into a valid Discord channel name.
///
/// Discord channel rules: lowercase letters, digits, hyphens, underscores;
/// no leading/trailing hyphens; max 100 characters.
pub fn sanitize_ident(raw: &str) -> String {
    // 1. Strip trailing ".git"
    let trimmed = raw.trim_end_matches(".git");

    // 2. Take the last path segment (handle both '/' and '\')
    let basename = trimmed
        .rsplit(|c| c == '/' || c == '\\')
        .find(|s| !s.is_empty())
        .unwrap_or(trimmed);

    // 3. Lowercase
    let lower = basename.to_lowercase();

    // 4. Replace any char that's not [a-z0-9_-] with '-'
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

    // 5. Collapse consecutive hyphens
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

    // 6. Strip leading/trailing hyphens
    let stripped = collapsed.trim_matches('-').to_string();

    // 7. Truncate to 100 chars
    let truncated = if stripped.len() > 100 {
        stripped[..100].trim_matches('-').to_string()
    } else {
        stripped
    };

    // 8. Fallback for empty result
    if truncated.is_empty() {
        let mut hasher = Sha256::new();
        hasher.update(raw.as_bytes());
        let hash = hasher.finalize();
        format!("project-{}", hex::encode(&hash[..4]))
    } else {
        truncated
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_git_and_takes_basename() {
        assert_eq!(sanitize_ident("github.com/nitecon/bruce.git"), "bruce");
        assert_eq!(sanitize_ident("/home/user/projects/my-app"), "my-app");
        assert_eq!(sanitize_ident("C:\\Users\\nitec\\Documents\\Projects\\bruce"), "bruce");
    }

    #[test]
    fn replaces_invalid_chars() {
        assert_eq!(sanitize_ident("My Project!"), "my-project-");
        // trailing hyphen stripped
        assert_eq!(sanitize_ident("hello world"), "hello-world");
    }

    #[test]
    fn collapses_hyphens() {
        assert_eq!(sanitize_ident("foo---bar"), "foo-bar");
    }

    #[test]
    fn fallback_for_all_special() {
        let result = sanitize_ident("!!!!");
        assert!(result.starts_with("project-"));
    }
}
