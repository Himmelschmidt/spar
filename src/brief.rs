//! Intake for `spar plan --spec <file>` / `--spec -` (stdin). A brief is a durable
//! record of what was asked for: it is copied into `.spar/briefs/<slug>.md` even when
//! `--spec` pointed at a file already in the repo, because the brief must outlive the
//! file (or terminal) it came from, and it is never overwritten once written.

use crate::paths::SparPaths;
use anyhow::{Context, Result};
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub struct Brief {
    pub slug: String,
    pub path: PathBuf,
    pub body: String,
}

/// Read `spec` from a file, or stdin when `spec == "-"`.
pub fn read_spec_text(spec: &Path) -> Result<String> {
    let body = if spec == Path::new("-") {
        let mut s = String::new();
        std::io::stdin()
            .read_to_string(&mut s)
            .context("read spec from stdin")?;
        s
    } else {
        std::fs::read_to_string(spec)
            .with_context(|| format!("read spec file {}", spec.display()))?
    };
    if body.trim().is_empty() {
        anyhow::bail!("spec is empty");
    }
    Ok(body)
}

/// Read `spec` (or stdin when `spec == "-"`), derive a slug, and write
/// `.spar/briefs/<slug>.md`. Never overwrites an existing brief: on a title collision
/// the slug gets `-2`, `-3`, … appended — two runs from two different briefs that
/// happen to share a title must not clobber each other.
pub fn intake(paths: &SparPaths, spec: &Path) -> Result<Brief> {
    let body = read_spec_text(spec)?;
    let base = slug_of(&body);
    let dir = paths.briefs_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;

    // Exclusive create, not check-then-write: two concurrent `plan --spec` calls with
    // the same title racing past a plain `exists()` check would both pick `<slug>.md`
    // and the second write would clobber the first. `create_new` makes the filesystem
    // the arbiter, and an `AlreadyExists` just means try the next suffix.
    let mut candidate = base.clone();
    let mut n = 1u32;
    let path = loop {
        let path = paths.brief_file(&candidate);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => {
                f.write_all(body.as_bytes())
                    .with_context(|| format!("write {}", path.display()))?;
                break path;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                n += 1;
                candidate = format!("{base}-{n}");
            }
            Err(e) => return Err(e).with_context(|| format!("create {}", path.display())),
        }
    };
    Ok(Brief {
        slug: candidate,
        path,
        body,
    })
}

/// The first `# ` heading, else the first non-empty line; lowercased, non-alphanumerics
/// collapsed to `-`, trimmed, truncated to 48 chars.
fn slug_of(body: &str) -> String {
    let title = body
        .lines()
        .find_map(|l| l.strip_prefix("# ").map(str::trim))
        .filter(|t| !t.is_empty())
        .or_else(|| body.lines().find(|l| !l.trim().is_empty()))
        .unwrap_or("brief");

    let mut slug = String::new();
    let mut last_was_dash = false;
    for c in title.chars() {
        if c.is_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
            last_was_dash = false;
        } else if !last_was_dash && !slug.is_empty() {
            slug.push('-');
            last_was_dash = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    slug.truncate(48);
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        slug = "brief".to_string();
    }
    slug
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn slug_prefers_a_heading() {
        assert_eq!(
            slug_of("# Durable Run Ownership\n\nbody"),
            "durable-run-ownership"
        );
    }

    #[test]
    fn slug_falls_back_to_first_nonempty_line() {
        assert_eq!(
            slug_of("\n\nFix the login bug!!\nmore\n"),
            "fix-the-login-bug"
        );
    }

    #[test]
    fn slug_is_truncated_and_never_ends_in_a_dash() {
        let long = "# ".to_string() + &"word ".repeat(30);
        let slug = slug_of(&long);
        assert!(slug.len() <= 48);
        assert!(!slug.ends_with('-'));
    }

    #[test]
    fn intake_writes_the_body_verbatim_and_returns_the_slug() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let spec = tmp.path().join("spec.md");
        std::fs::write(&spec, "# My Task\n\ndo the thing\n").unwrap();
        let brief = intake(&paths, &spec).unwrap();
        assert_eq!(brief.slug, "my-task");
        assert_eq!(brief.path, paths.brief_file("my-task"));
        assert_eq!(std::fs::read_to_string(&brief.path).unwrap(), brief.body);
        assert!(brief.body.contains("do the thing"));
    }

    #[test]
    fn intake_never_overwrites_a_same_titled_brief() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let spec = tmp.path().join("spec.md");
        std::fs::write(&spec, "# Same Title\n\nfirst\n").unwrap();
        let first = intake(&paths, &spec).unwrap();
        std::fs::write(&spec, "# Same Title\n\nsecond\n").unwrap();
        let second = intake(&paths, &spec).unwrap();
        assert_ne!(first.path, second.path);
        assert_eq!(second.slug, "same-title-2");
        assert_eq!(
            std::fs::read_to_string(&first.path).unwrap(),
            "# Same Title\n\nfirst\n"
        );
        assert_eq!(
            std::fs::read_to_string(&second.path).unwrap(),
            "# Same Title\n\nsecond\n"
        );
    }

    #[test]
    fn intake_rejects_an_empty_spec() {
        let tmp = tempdir().unwrap();
        let paths = SparPaths::new(tmp.path());
        let spec = tmp.path().join("spec.md");
        std::fs::write(&spec, "   \n\n").unwrap();
        assert!(intake(&paths, &spec).is_err());
    }
}
