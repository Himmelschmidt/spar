use super::{Capabilities, ProviderAdapter, SpawnOpts, TrustPolicy};
use std::path::Path;
use std::process::Command;

pub struct AgyAdapter;

impl ProviderAdapter for AgyAdapter {
    fn name(&self) -> &'static str {
        "agy"
    }

    fn binary_names(&self) -> &[&'static str] {
        &["agy"]
    }

    fn version_args(&self) -> &[&'static str] {
        &["--help"]
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            headless: true,
            interactive: true,
            resume: true,
            skip_permissions: true,
            native_sandbox: true,
        }
    }

    fn permission_args(&self, policy: TrustPolicy) -> Vec<String> {
        match policy {
            TrustPolicy::FullAuto => vec!["--dangerously-skip-permissions".into()],
            TrustPolicy::Prompt => vec![],
        }
    }

    fn build_headless(&self, bin: &Path, opts: &SpawnOpts) -> Command {
        let mut cmd = Command::new(bin);
        cmd.arg("--print");
        // Default print timeout; override via extra_args if needed.
        cmd.arg("--print-timeout");
        cmd.arg("1800");
        let prompt = if !opts.prompt.is_empty() {
            opts.prompt.clone()
        } else if let Some(pf) = &opts.prompt_file {
            std::fs::read_to_string(pf)
                .unwrap_or_else(|_| format!("Read and follow instructions in {}", pf.display()))
        } else {
            String::new()
        };
        cmd.arg(prompt);
        for a in self.permission_args(opts.trust) {
            cmd.arg(a);
        }
        for a in &opts.extra_args {
            cmd.arg(a);
        }
        cmd.current_dir(&opts.cwd);
        cmd
    }

    fn build_interactive(&self, bin: &Path, opts: &SpawnOpts) -> Command {
        let mut cmd = Command::new(bin);
        for a in self.permission_args(opts.trust) {
            cmd.arg(a);
        }
        if !opts.prompt.is_empty() {
            cmd.arg(&opts.prompt);
        }
        for a in &opts.extra_args {
            cmd.arg(a);
        }
        cmd.current_dir(&opts.cwd);
        cmd
    }
}
