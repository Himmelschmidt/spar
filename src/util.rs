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
}
