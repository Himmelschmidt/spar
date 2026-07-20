use anyhow::{bail, Result};
use std::time::Duration;

/// Parse durations like `30s`, `5m`, `2h`, `1d`, or plain seconds.
pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    if s.is_empty() {
        bail!("empty duration");
    }
    if let Ok(secs) = s.parse::<u64>() {
        return Ok(Duration::from_secs(secs));
    }
    let (num, unit) = s.split_at(s.len() - 1);
    let n: u64 = num
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid duration number in {s}"))?;
    let secs = match unit {
        "s" | "S" => n,
        "m" | "M" => n.saturating_mul(60),
        "h" | "H" => n.saturating_mul(3600),
        "d" | "D" => n.saturating_mul(86400),
        _ => bail!("unknown duration unit in {s} (use s/m/h/d)"),
    };
    Ok(Duration::from_secs(secs))
}

pub fn short_run_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
}

/// Make a slot id safe to use as a filename, a path component, and a git refname.
/// Lowercases, maps every char outside `[a-z0-9_-]` to `-`, collapses dash runs,
/// and trims leading/trailing dashes.
pub fn sanitize_slot(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let c = c.to_ascii_lowercase();
        let c = if c.is_ascii_alphanumeric() || c == '_' {
            c
        } else {
            '-'
        };
        if c == '-' && out.ends_with('-') {
            continue;
        }
        out.push(c);
    }
    out.trim_matches('-').to_string()
}

pub fn env_truthy(name: &str) -> bool {
    match std::env::var(name) {
        Ok(v) => matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_units() {
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert_eq!(parse_duration("90").unwrap(), Duration::from_secs(90));
    }

    #[test]
    fn strips_provider_ref_punctuation() {
        assert_eq!(sanitize_slot("cli:claude"), "cli-claude");
    }

    #[test]
    fn strips_model_slug_punctuation() {
        assert_eq!(
            sanitize_slot("cli:codex@anthropic/claude-opus-4.5"),
            "cli-codex-anthropic-claude-opus-4-5"
        );
    }

    #[test]
    fn collapses_runs() {
        assert_eq!(sanitize_slot("cli::claude"), "cli-claude");
        assert_eq!(sanitize_slot("a--b"), "a-b");
    }

    #[test]
    fn trims_edges() {
        assert_eq!(sanitize_slot("@claude@"), "claude");
    }

    #[test]
    fn lowercases() {
        assert_eq!(sanitize_slot("API:Claude"), "api-claude");
    }

    #[test]
    fn idempotent() {
        for case in [
            "cli:claude",
            "cli:codex@anthropic/claude-opus-4.5",
            "cli::claude",
            "@claude@",
            "review-0-a",
            "tencent/hy3:free",
        ] {
            let once = sanitize_slot(case);
            assert_eq!(sanitize_slot(&once), once, "not idempotent for {case}");
        }
    }

    #[test]
    fn preserves_safe_chars() {
        assert_eq!(sanitize_slot("review-0-a"), "review-0-a");
        assert_eq!(sanitize_slot("impl"), "impl");
        assert_eq!(sanitize_slot("test_author"), "test_author");
    }

    #[test]
    fn no_refname_hostile_chars() {
        let out = sanitize_slot("arena-0-cli:claude");
        assert_eq!(out, "arena-0-cli-claude");
        assert!(!out.contains([':', '/', '@', '.', ' ']));
    }
}
