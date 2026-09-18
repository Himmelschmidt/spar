use std::str::FromStr;

/// spar's normalized reasoning-effort ladder: the union of every native-cli
/// adapter's accepted vocabulary (muse's set is the superset). A slot carries
/// one rung; each adapter maps it onto its own control, and a rung its CLI
/// does not offer is refused at dispatch, never silently clamped (O95).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EffortLevel {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
    Ultra,
}

impl EffortLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            EffortLevel::None => "none",
            EffortLevel::Minimal => "minimal",
            EffortLevel::Low => "low",
            EffortLevel::Medium => "medium",
            EffortLevel::High => "high",
            EffortLevel::Xhigh => "xhigh",
            EffortLevel::Max => "max",
            EffortLevel::Ultra => "ultra",
        }
    }

    pub fn all() -> &'static [&'static str] {
        &[
            "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
        ]
    }
}

impl std::fmt::Display for EffortLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for EffortLevel {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "none" => Ok(EffortLevel::None),
            "minimal" => Ok(EffortLevel::Minimal),
            "low" => Ok(EffortLevel::Low),
            "medium" => Ok(EffortLevel::Medium),
            "high" => Ok(EffortLevel::High),
            "xhigh" => Ok(EffortLevel::Xhigh),
            "max" => Ok(EffortLevel::Max),
            "ultra" => Ok(EffortLevel::Ultra),
            other => anyhow::bail!(
                "unknown effort level {other:?} (takes: {})",
                EffortLevel::all().join(", ")
            ),
        }
    }
}

/// The rungs `cli_name` accepts, or `None` when the CLI takes a free string
/// (opencode's `--variant`) or spar knows no vocabulary for the name. Verified
/// against the versions on this box: `claude --help`, grok's vendored docs
/// (`none` through `max`), `muse exec --help`, `agy --help`.
///
/// **codex is the union, and it is the honest limit of what this table can
/// express.** codex has no client-side gate — it forwards `-c
/// model_reasoning_effort=<v>` to the server — and its levels are per *model*,
/// not per provider: `~/.codex/models_cache.json` (codex-cli 0.153.4) gives
/// `gpt-5.5` `low..xhigh`, `gpt-5.6-luna` and `codex-auto-review` `low..max`,
/// and `gpt-6-astra` / `gpt-5.6-sol` / `gpt-5.6-terra` `low..ultra`. A
/// per-provider set cannot say that, so spar takes the union and lets codex
/// reject a level its model does not offer, server-side, per dispatch. Probed
/// live against codex 0.153.4: `-c model_reasoning_effort=max` and `=ultra`
/// with `-m gpt-5.6-terra` both completed normally. Per-model validation is
/// deliberately not built here.
pub fn accepted_levels(cli_name: &str) -> Option<&'static [&'static str]> {
    match cli_name {
        "claude" => Some(&["low", "medium", "high", "xhigh", "max"]),
        "grok" => Some(&["none", "minimal", "low", "medium", "high", "xhigh", "max"]),
        "muse" => Some(&[
            "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
        ]),
        "codex" => Some(&[
            "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
        ]),
        "agy" => Some(&["low", "medium", "high"]),
        _ => None,
    }
}

/// Refuse a provider+effort pair the CLI does not offer. Called at dispatch,
/// before spawn, because rotation, widening and backup can change a slot's
/// provider after parse time. A refusal is spar's fault (never retried, never
/// an agent failure) and names the accepted levels. api-sdk providers and
/// free-string CLIs never refuse.
pub fn check_compatible(cli_name: &str, level: EffortLevel) -> anyhow::Result<()> {
    let Some(set) = accepted_levels(cli_name) else {
        return Ok(());
    };
    if set.contains(&level.as_str()) {
        return Ok(());
    }
    anyhow::bail!(
        "effort '{}' not accepted by cli:{} (takes: {})",
        level.as_str(),
        cli_name,
        set.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ladder_parses_all_rungs() {
        for rung in EffortLevel::all() {
            let parsed: EffortLevel = rung.parse().expect(rung);
            assert_eq!(parsed.as_str(), *rung);
        }
    }

    #[test]
    fn ladder_parse_is_case_insensitive_and_trims() {
        assert_eq!("XHigh".parse::<EffortLevel>().unwrap(), EffortLevel::Xhigh);
        assert_eq!("  max ".parse::<EffortLevel>().unwrap(), EffortLevel::Max);
    }

    #[test]
    fn ladder_parse_error_lists_rungs() {
        let err = "turbo".parse::<EffortLevel>().unwrap_err();
        let msg = format!("{err:#}");
        for rung in EffortLevel::all() {
            assert!(msg.contains(rung), "error names {rung}: {msg}");
        }
    }

    #[test]
    fn vocab_tables_match_verified_surfaces() {
        assert_eq!(
            accepted_levels("claude"),
            Some(&["low", "medium", "high", "xhigh", "max"][..])
        );
        assert_eq!(
            accepted_levels("grok"),
            Some(&["none", "minimal", "low", "medium", "high", "xhigh", "max"][..])
        );
        assert_eq!(accepted_levels("muse"), Some(EffortLevel::all()));
        assert_eq!(
            accepted_levels("codex"),
            Some(&["none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"][..])
        );
        assert_eq!(accepted_levels("agy"), Some(&["low", "medium", "high"][..]));
        assert_eq!(accepted_levels("opencode"), None);
    }

    #[test]
    fn check_accepts_in_vocab() {
        check_compatible("agy", EffortLevel::Low).unwrap();
        check_compatible("codex", EffortLevel::Xhigh).unwrap();
        // Probed live against codex 0.153.4 with `-m gpt-5.6-terra`: both
        // completed normally. codex validates per model, server-side.
        check_compatible("codex", EffortLevel::Max).unwrap();
        check_compatible("codex", EffortLevel::Ultra).unwrap();
        check_compatible("muse", EffortLevel::Ultra).unwrap();
        check_compatible("opencode", EffortLevel::Ultra).unwrap();
    }

    #[test]
    fn check_refuses_out_of_vocab_naming_levels() {
        // grok's only out-of-vocab rung: canonical levels run none..max.
        let err = check_compatible("grok", EffortLevel::Ultra).unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "effort 'ultra' not accepted by cli:grok (takes: none, minimal, low, medium, high, xhigh, max)"
        );
        let err = check_compatible("agy", EffortLevel::Ultra).unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "effort 'ultra' not accepted by cli:agy (takes: low, medium, high)"
        );
        let err = check_compatible("claude", EffortLevel::None).unwrap_err();
        assert!(format!("{err:#}").contains("cli:claude"), "{err:#}");
    }
}
