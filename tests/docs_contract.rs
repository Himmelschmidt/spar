//! Acceptance contract for the operator-model + TUI-IA planning docs (run `d995e566`).
//!
//! The deliverable is prose, so the acceptance bar is a document contract: the eight
//! target paths, the house format of each, the `DECISIONS.md` id sequences, the feature
//! frontmatter dependency chain, and the scope fence that keeps a docs-only run out of
//! `src/`. Everything asserted here is checkable by someone who did not write the docs.
//!
//! Auto-discovered by Cargo as `tests/docs_contract.rs`; unlike `tests/scenarios/*.rs`
//! it needs no `[[test]]` block, which keeps the run's "no `Cargo.toml` changes"
//! non-goal intact.
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

// ---------------------------------------------------------------- paths + io

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

const OPERATOR_DOC: &str = "docs/architecture-operator-model.md";
const TUI_DOC: &str = "docs/architecture-tui-ia.md";
const DECISIONS: &str = "DECISIONS.md";
const ROADMAP: &str = "roadmap/ROADMAP.md";

const FEATURES: [(&str, u32, &str); 4] = [
    ("roadmap/features/003-durable-run-ownership.md", 3, "[]"),
    (
        "roadmap/features/004-tui-information-architecture.md",
        4,
        "[3]",
    ),
    ("roadmap/features/005-gate-evidence.md", 5, "[4]"),
    ("roadmap/features/006-motion-and-identity.md", 6, "[5]"),
];

/// The eight paths the run is allowed to produce.
fn deliverables() -> Vec<&'static str> {
    let mut v = vec![OPERATOR_DOC, TUI_DOC, DECISIONS, ROADMAP];
    v.extend(FEATURES.iter().map(|(p, _, _)| *p));
    v
}

/// New files only. `DECISIONS.md` and `ROADMAP.md` carry pre-existing em dashes, so
/// style checks on those two run per added line instead of over the whole file.
fn new_files() -> Vec<&'static str> {
    let mut v = vec![OPERATOR_DOC, TUI_DOC];
    v.extend(FEATURES.iter().map(|(p, _, _)| *p));
    v
}

fn read(rel: &str) -> String {
    let p = repo_root().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

fn read_baseline(name: &str) -> String {
    read(&format!("tests/fixtures/docs-baseline/{name}"))
}

fn exists(rel: &str) -> bool {
    repo_root().join(rel).exists()
}

// ------------------------------------------------------------- style scanners

const EM_DASH: char = '\u{2014}';

/// Glyphs already in the repo's docs, plus the interface glyphs the plan quotes.
/// Anything else in the emoji planes is a violation.
fn is_emoji(c: char) -> bool {
    const ALLOWED: &[char] = &[
        '\u{2691}', // flag, the attention marker
        '\u{25B8}', // rail drill-down triangle
        '\u{00B7}', '\u{2026}', '\u{2192}', '\u{2265}', '\u{2260}', '\u{2212}', '\u{2191}',
        '\u{2193}', '\u{2194}', '\u{21D2}', '\u{00D7}', '\u{2713}', '\u{2717}',
    ];
    if ALLOWED.contains(&c) {
        return false;
    }
    if ('\u{2800}'..='\u{28FF}').contains(&c) {
        return false; // braille density blocks
    }
    let u = c as u32;
    (0x1F000..=0x1FAFF).contains(&u)
        || (0x2600..=0x27BF).contains(&u)
        || (0x2B00..=0x2BFF).contains(&u)
        || u == 0xFE0F
}

/// Lines present in `text` that are not present in `baseline`, by multiset difference.
/// A reordering of existing lines therefore reads as "added", which is intended: this
/// run may only append.
fn added_lines(text: &str, baseline: &str) -> Vec<(usize, String)> {
    let mut pool: BTreeMap<&str, usize> = BTreeMap::new();
    for l in baseline.lines() {
        *pool.entry(l).or_insert(0) += 1;
    }
    let mut out = Vec::new();
    for (i, l) in text.lines().enumerate() {
        match pool.get_mut(l) {
            Some(n) if *n > 0 => *n -= 1,
            _ => out.push((i + 1, l.to_string())),
        }
    }
    out
}

// ------------------------------------------------------------ markdown helpers

/// Cells of a pipe table row, outer pipes stripped, each trimmed.
fn row_cells(line: &str) -> Option<Vec<String>> {
    let t = line.trim();
    if !t.starts_with('|') || !t.ends_with('|') || t.len() < 3 {
        return None;
    }
    let inner = &t[1..t.len() - 1];
    Some(inner.split('|').map(|c| c.trim().to_string()).collect())
}

/// `P7`, `O38`, `MS14`: one to three capitals then digits. Excludes the `| ID |` header
/// and the `|----|` separator, which are otherwise shaped like rows.
fn is_decision_id(id: &str) -> bool {
    let letters = id.chars().take_while(|c| c.is_ascii_uppercase()).count();
    let digits = &id[letters..];
    letters > 0 && letters <= 3 && !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
}

/// `## Heading` -> the lines belonging to that section (heading excluded).
fn section<'a>(text: &'a str, heading: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut inside = false;
    for l in text.lines() {
        if l.starts_with("## ") {
            inside = l.trim() == heading;
            continue;
        }
        if inside {
            out.push(l);
        }
    }
    out
}

/// Every `| ID | ... |` row id in a section, in file order.
fn section_row_ids(text: &str, heading: &str) -> Vec<String> {
    section(text, heading)
        .into_iter()
        .filter_map(row_cells)
        .map(|c| c[0].clone())
        .filter(|id| is_decision_id(id))
        .collect()
}

fn decision_row(text: &str, id: &str) -> Option<Vec<String>> {
    text.lines()
        .filter_map(row_cells)
        .find(|c| c.first().map(|s| s.as_str()) == Some(id))
}

fn frontmatter(text: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    let mut lines = text.lines();
    assert_eq!(lines.next(), Some("---"), "frontmatter must open on line 1");
    for l in lines {
        if l.trim() == "---" {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            map.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    map
}

fn headings(text: &str) -> Vec<String> {
    text.lines()
        .filter(|l| l.starts_with('#'))
        .map(|l| l.trim().to_string())
        .collect()
}

fn has_heading(text: &str, prefix: &str) -> bool {
    headings(text).iter().any(|h| h.starts_with(prefix))
}

fn mentions_all(text: &str, needles: &[&str]) -> Vec<String> {
    let lower = text.to_lowercase();
    needles
        .iter()
        .filter(|n| !lower.contains(&n.to_lowercase()))
        .map(|n| n.to_string())
        .collect()
}

// ------------------------------------------------------------------- git scope

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo_root())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Every path this branch changed relative to its merge base, including uncommitted
/// and untracked work.
fn changed_paths() -> Vec<String> {
    let base = ["main", "origin/main", "master"]
        .iter()
        .find_map(|r| git(&["merge-base", "HEAD", r]))
        .expect("no merge base against main/origin/main/master; run this inside the repo");
    let tracked = git(&["diff", "--name-only", &base]).expect("git diff");
    let untracked = git(&["ls-files", "--others", "--exclude-standard"]).expect("git ls-files");
    let mut v: Vec<String> = tracked
        .lines()
        .chain(untracked.lines())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    v.sort();
    v.dedup();
    v
}

// ============================================================== AC-1 .. AC-16

/// AC-1: all eight deliverable paths exist.
#[test]
fn ac1_all_eight_deliverables_exist() {
    let missing: Vec<&str> = deliverables().into_iter().filter(|p| !exists(p)).collect();
    assert!(missing.is_empty(), "missing deliverables: {missing:?}");
}

/// AC-2: a docs-only run touches nothing that changes behavior.
#[test]
fn ac2_no_behavior_or_out_of_scope_files_touched() {
    const FORBIDDEN: &[&str] = &[
        "src/",
        "Cargo.toml",
        "Cargo.lock",
        "skills/",
        "templates/",
        "AGENTS.md",
        "CLAUDE.md",
        "docs/PRODUCT.md",
        "roadmap/BACKLOG.md",
    ];
    let hits: Vec<String> = changed_paths()
        .into_iter()
        .filter(|p| FORBIDDEN.iter().any(|f| p.starts_with(f)))
        .collect();
    assert!(hits.is_empty(), "docs-only run touched: {hits:?}");
}

/// AC-3: nothing outside the eight deliverables (and this contract's own files) changed.
#[test]
fn ac3_changes_confined_to_the_deliverable_set() {
    let allowed = |p: &str| {
        deliverables().contains(&p)
            || p == "tests/docs_contract.rs"
            || p.starts_with("tests/fixtures/docs-baseline/")
    };
    let stray: Vec<String> = changed_paths()
        .into_iter()
        .filter(|p| !allowed(p.as_str()))
        .collect();
    assert!(stray.is_empty(), "unexpected paths in the diff: {stray:?}");
}

/// AC-4: zero em dashes in the six new files and in every line added to the two
/// appended files.
///
/// DECISIONS.md gets one cell-aware carve-out: AC-8 mandates editing U3's Status cell
/// while leaving its pre-existing Decision text byte-identical, and that Decision text
/// (written long before this run, U+2014 and all) is otherwise-unmodified prose the run
/// did not author. A pure line-level diff cannot tell "line changed because of the
/// sanctioned Status edit" from "line is newly authored", so it would flag those
/// pre-existing dashes as new. Row-parse instead: when a row's id existed in the baseline
/// with the same Decision cell, only its Status cell is new content and gets scanned;
/// every other line (all genuinely new or altered content) still goes through the
/// original whole-line check.
#[test]
fn ac4_no_em_dashes_in_new_content() {
    let mut bad = Vec::new();
    for f in new_files() {
        for (i, l) in read(f).lines().enumerate() {
            if l.contains(EM_DASH) {
                bad.push(format!("{f}:{}: {l}", i + 1));
            }
        }
    }
    for (f, base) in [(DECISIONS, "DECISIONS.md"), (ROADMAP, "ROADMAP.md")] {
        let baseline = read_baseline(base);
        for (n, l) in added_lines(&read(f), &baseline) {
            if let Some(cells) = row_cells(&l) {
                if cells.len() == 3 && is_decision_id(&cells[0]) {
                    if let Some(base_cells) = decision_row(&baseline, &cells[0]) {
                        if base_cells.len() == 3 && base_cells[1] == cells[1] {
                            if cells[2].contains(EM_DASH) {
                                bad.push(format!("{f}:{n}: {l} (new Status text)"));
                            }
                            continue;
                        }
                    }
                }
            }
            if l.contains(EM_DASH) {
                bad.push(format!("{f}:{n}: {l}"));
            }
        }
    }
    assert!(
        bad.is_empty(),
        "em dashes in new content:\n{}",
        bad.join("\n")
    );
}

/// AC-5: zero emoji anywhere in the eight deliverables.
#[test]
fn ac5_no_emoji_in_deliverables() {
    let mut bad = Vec::new();
    for f in deliverables() {
        for (i, l) in read(f).lines().enumerate() {
            for c in l.chars().filter(|c| is_emoji(*c)) {
                bad.push(format!("{f}:{}: U+{:04X} in {l}", i + 1, c as u32));
            }
        }
    }
    assert!(bad.is_empty(), "emoji found:\n{}", bad.join("\n"));
}

/// AC-6: exactly the sixteen new decision ids land, each in the right section, each a
/// well-formed three-cell row, appended at the end of its table.
#[test]
fn ac6_decisions_rows_added_in_the_right_sections() {
    let text = read(DECISIONS);
    let expect: [(&str, &[&str]); 4] = [
        ("## Product", &["P7"]),
        ("## Orchestration", &["O38", "O39", "O40", "O41", "O42"]),
        (
            "## TUI",
            &["U6", "U7", "U8", "U9", "U10", "U11", "U12", "U13"],
        ),
        ("## Open", &["X9", "X10"]),
    ];
    for (heading, ids) in expect {
        let present = section_row_ids(&text, heading);
        let tail: Vec<&str> = present
            .iter()
            .rev()
            .take(ids.len())
            .rev()
            .map(|s| s.as_str())
            .collect();
        assert_eq!(
            tail, ids,
            "{heading} must end with {ids:?}; found tail {tail:?} in {present:?}"
        );
    }
    for id in expect.iter().flat_map(|(_, ids)| ids.iter()) {
        let cells = decision_row(&text, id).unwrap_or_else(|| panic!("no row for {id}"));
        assert_eq!(cells.len(), 3, "{id} must have 3 cells, got {cells:?}");
        assert!(!cells[1].is_empty(), "{id} decision cell is empty");
        assert!(!cells[2].is_empty(), "{id} status cell is empty");
    }
}

/// AC-7: no duplicate decision ids anywhere in `DECISIONS.md`.
#[test]
fn ac7_decision_ids_are_unique() {
    let text = read(DECISIONS);
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    for cells in text.lines().filter_map(row_cells) {
        if is_decision_id(&cells[0]) {
            *seen.entry(cells[0].clone()).or_insert(0) += 1;
        }
    }
    let dupes: Vec<&String> = seen
        .iter()
        .filter(|(_, n)| **n > 1)
        .map(|(k, _)| k)
        .collect();
    assert!(dupes.is_empty(), "duplicate decision ids: {dupes:?}");
}

/// AC-8: every pre-existing decision row is byte-identical, except U3's Status cell,
/// whose Decision text must still be unchanged and whose Status must name U8.
#[test]
fn ac8_existing_decision_rows_are_immutable_except_u3_status() {
    let base = read_baseline("DECISIONS.md");
    let text = read(DECISIONS);
    let now: BTreeMap<String, Vec<String>> = text
        .lines()
        .filter_map(row_cells)
        .filter(|c| c.len() == 3 && is_decision_id(&c[0]))
        .map(|c| (c[0].clone(), c))
        .collect();
    for cells in base
        .lines()
        .filter_map(row_cells)
        .filter(|c| c.len() == 3 && is_decision_id(&c[0]))
    {
        let id = &cells[0];
        let Some(after) = now.get(id) else {
            panic!("pre-existing row {id} disappeared");
        };
        assert_eq!(&after[1], &cells[1], "{id} decision text was edited");
        if id != "U3" {
            assert_eq!(&after[2], &cells[2], "{id} status was edited");
        }
    }
    let u3 = now.get("U3").expect("U3 row");
    assert_ne!(u3[2], "DECIDED", "U3 status must record the supersession");
    assert!(
        u3[2].contains("U8") || u3[2].to_lowercase().contains("supersed"),
        "U3 status must name its superseder: {:?}",
        u3[2]
    );
}

/// AC-9: the new milestone lands before `## Later`, its bullets point at 003 through
/// 006, and every one of them keeps the file's trailing double-space line break.
#[test]
fn ac9_roadmap_milestone_block_added_before_later() {
    let text = read(ROADMAP);
    let lines: Vec<&str> = text.lines().collect();
    let later = lines
        .iter()
        .position(|l| l.trim() == "## Later")
        .expect("## Later heading");
    let milestone = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.starts_with("## "))
        .filter(|(i, _)| *i < later)
        .map(|(i, _)| i)
        .next_back()
        .expect("a milestone heading before ## Later");
    let block = &lines[milestone..later];
    assert!(
        block[0].contains("Milestone 6"),
        "the new block must be Milestone 6, got {:?}",
        block[0]
    );
    let bullets: Vec<&&str> = block
        .iter()
        .filter(|l| l.trim_start().starts_with("- "))
        .collect();
    assert_eq!(bullets.len(), 4, "one bullet per feature, got {bullets:?}");
    for b in &bullets {
        assert!(
            b.ends_with("  "),
            "ROADMAP bullets end with a double-space line break: {b:?}"
        );
    }
    for (path, _, _) in FEATURES {
        let file = path.rsplit('/').next().unwrap();
        assert!(
            block.iter().any(|l| l.contains(file)),
            "milestone block must reference {file}"
        );
    }
}

/// AC-10: milestones 0 through 5 and the `## Later` block are byte-identical.
#[test]
fn ac10_existing_roadmap_content_is_immutable() {
    let base = read_baseline("ROADMAP.md");
    let text = read(ROADMAP);
    let base_lines: Vec<&str> = base.lines().collect();
    let now_lines: Vec<&str> = text.lines().collect();

    let prefix_len = base_lines
        .iter()
        .position(|l| l.trim() == "## Milestone 5" || l.starts_with("## Milestone 5"))
        .map(|i| {
            base_lines[i..]
                .iter()
                .position(|l| l.trim() == "## Later")
                .map(|j| i + j)
                .expect("## Later after milestone 5")
        })
        .expect("## Milestone 5 heading");
    assert_eq!(
        &now_lines[..prefix_len],
        &base_lines[..prefix_len],
        "milestones 0 through 5 must be untouched"
    );

    let base_later = base_lines
        .iter()
        .position(|l| l.trim() == "## Later")
        .unwrap();
    let now_later = now_lines
        .iter()
        .position(|l| l.trim() == "## Later")
        .unwrap();
    assert_eq!(
        &now_lines[now_later..],
        &base_lines[base_later..],
        "the ## Later block must be untouched"
    );
}

/// AC-11: feature frontmatter parses, ids are unpadded 3 to 6, status `backlog`,
/// milestone `6`, and the dependency chain encodes the build order.
#[test]
fn ac11_feature_frontmatter_encodes_the_dependency_chain() {
    for (path, id, deps) in FEATURES {
        let text = read(path);
        let fm = frontmatter(&text);
        assert_eq!(
            fm.get("id").map(String::as_str),
            Some(id.to_string().as_str()),
            "{path} id"
        );
        assert_eq!(
            fm.get("status").map(String::as_str),
            Some("backlog"),
            "{path} status"
        );
        assert_eq!(
            fm.get("milestone").map(String::as_str),
            Some("6"),
            "{path} milestone"
        );
        assert_eq!(
            fm.get("dependencies").map(String::as_str),
            Some(deps),
            "{path} dependencies must encode the ordering"
        );
        for k in ["title", "effort", "priority"] {
            let v = fm.get(k).map(String::as_str).unwrap_or("");
            assert!(!v.is_empty(), "{path} frontmatter `{k}` is empty");
        }
    }
}

/// AC-12: each feature entry follows the `001-model-select.md` section shape and cites
/// the decisions it implements.
#[test]
fn ac12_feature_entries_follow_the_house_shape() {
    for (path, id, _) in FEATURES {
        let text = read(path);
        let h1 = headings(&text)
            .into_iter()
            .find(|h| h.starts_with("# "))
            .unwrap_or_else(|| panic!("{path} has no H1"));
        assert!(
            h1.contains(&format!("{id:03}")),
            "{path} H1 must carry the zero-padded id: {h1:?}"
        );
        for h in [
            "## Summary",
            "## Problem",
            "## Goals",
            "## Non-goals",
            "## Phases",
        ] {
            assert!(has_heading(&text, h), "{path} missing {h}");
        }
        let summary_end = text.find("## Problem").unwrap_or(text.len());
        let summary = &text[text.find("## Summary").unwrap_or(0)..summary_end];
        assert!(
            summary.contains("DECISIONS.md"),
            "{path} Summary must cite the DECISIONS.md rows it implements"
        );
        assert!(
            text.contains("### ") || text.contains("Phase "),
            "{path} Phases section must break the work into phases"
        );
    }
}

/// AC-13: 003 states that the target flow works with no TUI work, and 004 through 006
/// each cite the decision rows that drive them.
#[test]
fn ac13_feature_entries_carry_their_sequencing_rationale() {
    let f003 = read(FEATURES[0].0).to_lowercase();
    assert!(
        f003.contains("no tui work") || (f003.contains("without") && f003.contains("tui")),
        "003 must say the flow lands with no TUI work"
    );
    for (path, ids) in [
        (FEATURES[0].0, vec!["P7", "O38", "O39", "O40", "O41", "O42"]),
        (FEATURES[1].0, vec!["U6", "U7", "U8", "U13"]),
        (FEATURES[2].0, vec!["U9"]),
        (FEATURES[3].0, vec!["U10", "U11", "U12"]),
    ] {
        let text = read(path);
        let missing: Vec<&str> = ids.into_iter().filter(|i| !text.contains(i)).collect();
        assert!(missing.is_empty(), "{path} must cite {missing:?}");
    }
}

/// AC-14: every `path:line` citation in the two architecture docs resolves to a real
/// file with at least that many lines.
#[test]
fn ac14_code_citations_resolve() {
    let mut bad = Vec::new();
    for doc in [OPERATOR_DOC, TUI_DOC] {
        let text = read(doc);
        let mut n_checked = 0usize;
        for span in text.split('`').skip(1).step_by(2) {
            let Some((path, tail)) = span.rsplit_once(':') else {
                continue;
            };
            if path.contains('/') && path.contains("://") {
                continue;
            }
            let looks_like_path = path.contains('.')
                && path
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "._/-".contains(c));
            if !looks_like_path {
                continue;
            }
            let nums: Vec<&str> = tail.split('-').collect();
            if nums.is_empty()
                || !nums
                    .iter()
                    .all(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
            {
                continue;
            }
            n_checked += 1;
            let file = repo_root().join(path);
            let Ok(body) = std::fs::read_to_string(&file) else {
                bad.push(format!("{doc}: `{span}` -> no such file {path}"));
                continue;
            };
            let len = body.lines().count();
            for n in nums {
                let n: usize = n.parse().unwrap();
                if n == 0 || n > len {
                    bad.push(format!("{doc}: `{span}` -> {path} has {len} lines"));
                }
            }
        }
        assert!(
            n_checked >= 5,
            "{doc} cites only {n_checked} code locations; the brief requires path:line citations"
        );
    }
    assert!(
        bad.is_empty(),
        "unresolvable citations:\n{}",
        bad.join("\n")
    );
}

/// AC-15: the operator-model doc covers every topic the brief requires of it.
#[test]
fn ac15_operator_model_doc_covers_the_required_topics() {
    let text = read(OPERATOR_DOC);
    assert!(
        text.starts_with("# ") && text.contains("**Status:**") && text.contains("**Decisions:**"),
        "operator doc must open with the house H1 + Status/Decisions metadata block"
    );
    let missing = mentions_all(
        &text,
        &[
            "disposable",
            "RunState::save",
            "daemon",
            "setsid",
            "abandon",
            "notify",
            "concurrency cap",
            "/proc",
            "rejected",
            "spar resume",
            ".spar/briefs/",
            "draft PR",
        ],
    );
    assert!(missing.is_empty(), "operator doc missing: {missing:?}");
    for id in ["P7", "O38", "O39", "O40", "O41", "O42"] {
        assert!(text.contains(id), "operator doc must reference {id}");
    }
    for step in 1..=8 {
        assert!(
            text.contains(&format!("{step}.")) || text.contains(&format!("{step})")),
            "operator doc must enumerate target-flow step {step}"
        );
    }
}

/// AC-16: the TUI IA doc settles the nouns, retires "session", and reconciles the
/// DECIDED rows it changes.
#[test]
fn ac16_tui_ia_doc_settles_nouns_and_reconciles_decided_rows() {
    let text = read(TUI_DOC);
    assert!(
        text.starts_with("# ") && text.contains("**Status:**") && text.contains("**Decisions:**"),
        "TUI doc must open with the house H1 + Status/Decisions metadata block"
    );
    let missing = mentions_all(
        &text,
        &[
            "Project",
            "Run",
            "Agent",
            "Shell",
            "Home",
            "spar-<run_id>",
            "/spawn",
            "Session / run home",
            "test-contract.md",
            "require_all_criteria",
            "Snapshot",
            "skeleton",
            "Tween",
            "TestBackend",
        ],
    );
    assert!(missing.is_empty(), "TUI doc missing: {missing:?}");
    for id in ["U1", "U3", "U4", "U5", "X2", "U6", "U7", "U8", "U9", "U13"] {
        assert!(text.contains(id), "TUI doc must reconcile or cite {id}");
    }
    let lower = text.to_lowercase();
    assert!(
        lower.contains("supersede"),
        "TUI doc must say plainly which rows it supersedes"
    );
    assert!(
        text.contains("AC-n") || lower.contains("per-criterion") || lower.contains("criteria"),
        "TUI doc must describe per-criterion gate evidence"
    );
}
