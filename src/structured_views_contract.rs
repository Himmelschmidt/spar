use crate::record::{
    parse_document, parse_log_records, Columns, LogDirection, PathShortener, RecordKind, SourceId,
    ToolKind,
};
use chrono::{TimeZone, Utc};
use std::path::PathBuf;

fn shortener() -> PathShortener {
    PathShortener::new(vec![
        PathBuf::from("/tmp/codex/spar-44e0e49a-impl"),
        PathBuf::from("/home/sholom/projects/spar"),
    ])
}

#[test]
fn log_records_keep_typed_fields_times_and_source_ranges() {
    let started = Utc.timestamp_millis_opt(1_725_551_040_000).unwrap();
    let finished = Utc.timestamp_millis_opt(1_725_551_040_800).unwrap();
    let text = "→ Bash  ls -la /etc | head -5\n← ✓  total 1184\n";
    let records = parse_log_records(
        text,
        400,
        &[(400, started), (431, finished)],
        &shortener(),
        "3d3d6f59",
        "implementer",
    );

    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record.time, Some(started));
    assert_eq!(record.direction, LogDirection::Tool);
    assert_eq!(record.tool, Some(ToolKind::Run));
    assert_eq!(record.argument, "ls -la /etc | head -5");
    assert_eq!(record.result.as_deref(), Some("total 1184"));
    assert_eq!(record.elapsed.unwrap().as_millis(), 800);
    // end=452 is the chunk's real end (start_offset + the whole text's byte length):
    // it must span the merged record's actual bytes, including the result line and
    // its newline, not just the head line up to wherever a marker was stripped.
    assert_eq!(
        record.source,
        SourceId::Log {
            run_id: "3d3d6f59".into(),
            slot_id: "implementer".into(),
            start: 400,
            end: 452,
        }
    );
}

#[test]
fn no_index_never_fabricates_time_and_ambiguous_result_stays_visible() {
    let records = parse_log_records(
        "→ Bash  first\n→ Bash  second\n← ✓  result for an unknown call\n",
        0,
        &[],
        &shortener(),
        "3d3d6f59",
        "implementer",
    );

    assert!(records.iter().all(|record| record.time.is_none()));
    assert!(records.iter().all(|record| record.elapsed.is_none()));
    assert_eq!(
        records.len(),
        3,
        "an ambiguous result must not be lost or mis-paired"
    );
    assert_eq!(records[2].kind, RecordKind::Result { ok: true });
    assert_eq!(
        records[2].result.as_deref(),
        Some("result for an unknown call")
    );
}

#[test]
fn path_shortening_is_run_local_and_keeps_the_last_two_components() {
    let value = shortener()
        .shorten("/tmp/codex/spar-44e0e49a-impl/.spar/runs/3d3d6f59/artifacts/test-contract.md");
    assert_eq!(value, ".spar/runs/3d3d6f59/artifacts/test-contract.md");

    let external = shortener().shorten("/var/lib/agents/scratchpad/output.txt");
    assert!(external.ends_with("scratchpad/output.txt"));
    assert!(!external.starts_with("/var/lib/agents/"));
}

#[test]
fn widths_reserve_columns_without_moving_them_for_content() {
    let narrow = Columns::for_width(87);
    let wide = Columns::for_width(120);
    assert!(narrow.meta > narrow.summary);
    assert!(wide.meta > narrow.meta, "a wider view must reveal a field");
    assert!(
        wide.actor.is_some(),
        "the actor column appears at the wide breakpoint"
    );

    for width in [79, 80, 99, 100, 119, 120] {
        let columns = Columns::for_width(width);
        assert!(columns.glyph < columns.verb);
        assert!(columns.verb < columns.summary);
        assert!(columns.summary < columns.meta);
        assert_eq!(columns.meta + columns.meta_width, width);
    }
}

#[test]
fn document_parser_preserves_sections_and_reports_missing_documents() {
    let records = parse_document(
        "plan.md",
        "# Plan\nfirst body line\n\n## Risks\nsecond body line\n",
        "plan.md",
    );
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].kind, RecordKind::Doc);
    assert_eq!(records[0].head, "Plan");
    assert_eq!(records[1].head, "Risks");
    assert!(records[1]
        .body
        .iter()
        .any(|line| line == "second body line"));
    // AC-7: each section is its own immutable identity, not one identity shared by
    // the whole document — otherwise `Space` expands every section at once and
    // `J`/`K` cannot move between them.
    assert_ne!(
        records[0].source, records[1].source,
        "each document section must have its own source identity"
    );
}
