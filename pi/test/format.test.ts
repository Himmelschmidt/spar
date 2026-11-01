import assert from "node:assert/strict";
import { test } from "node:test";
import { detailLines, footerText, listRows, listText, plain, sortRuns } from "../extensions/spar/format.ts";
import type { Brief } from "../extensions/spar/spar.ts";
import { minutesSinceLocalMidnight, statsCost } from "../extensions/spar/spar.ts";
import { row, status } from "./helpers.ts";

test("runs needing the operator sort first, then running, then most recent", () => {
	const now = Date.now();
	const runs = [
		row("old", { updated_at: new Date(now - 5000).toISOString() }),
		row("live", { holds_seat: true }),
		row("new", { updated_at: new Date(now).toISOString() }),
		row("gate", { needs_human: true }),
	];
	assert.deepEqual(
		sortRuns(runs).map((r) => r.id),
		["gate", "live", "new", "old"],
	);
});

test("footer counts running and waiting runs, paused providers and cost", () => {
	const text = footerText(
		{
			runs: [row("a", { holds_seat: true }), row("b", { needs_human: true }), row("c", { needs_human: true })],
			paused: [{ name: "codex", quota_status: "paused" }],
			costToday: 3.5,
		},
		plain,
	);
	assert.equal(text, "spar | 1 running | 2 need you | paused codex | $3.50 today");
});

test("footer omits empty parts", () => {
	assert.equal(footerText({ runs: [], paused: [], costToday: undefined }, plain), "spar | 0 running");
});

test("list rows carry id, phase, workflow, round, age, flag and cost", () => {
	const runs = [row("aaaa1111", { round: 3 }), row("bbbb2222", { needs_human: true, phase: "awaiting_ship_confirm" })];
	const costs = new Map([["aaaa1111", status("aaaa1111").cost!.total]]);
	const rows = listRows(runs, costs, new Date());
	assert.equal(rows[0]!.id, "bbbb2222");
	assert.match(rows[0]!.label, /^bbbb2222\s+awaiting_ship_confirm\s+r1\s+1h\s+NEEDS YOU\s+-\s+loop\s*$/);
	assert.match(rows[1]!.label, /^aaaa1111\s+review\s+r3\s+1h\s+\$12\.50\+\s+loop\s*$/);
	assert.equal(rows[1]!.description, "task for aaaa1111");
	assert.equal(listText([], new Map(), new Date()), "spar: no runs in this project");
});

test("detail groups findings by severity and ends with the next command", () => {
	const brief: Brief = {
		run: "r1",
		next_command: "spar ship r1",
		fleet: ["implementer=cli:muse"],
		open_findings: [
			{ slot_id: "review-0", severity: "minor", text: "nit", file: "src/a.rs", line: 3 },
			{ slot_id: "review-1", severity: "critical", text: "data loss\nsecond line", possibly_addressed: true },
			{ slot_id: "review-0", severity: "major", text: "bug", follow_up: true },
		],
		recent_events: [
			{ ts: new Date().toISOString(), type: "phase", prev_phase: "review", phase: "awaiting_ship_confirm", round: 2 },
			{ ts: new Date().toISOString(), type: "slot", slot: "impl", status: "done" },
		],
	};
	const lines = detailLines(
		{
			status: status("r1", { needs_human: true }),
			brief,
			bus: [{ ts: new Date().toISOString(), from: "orchestrator", to: "broadcast", kind: "chat", body: "suite green\nx" }],
			now: new Date(),
		},
		plain,
	);
	const text = lines.join("\n");
	assert.match(lines[0]!, /^run r1 {2}loop {2}phase review {2}round 1\/6 {2}NEEDS YOU$/);
	assert.match(text, /cost: \$12\.50\+ \(1 of 4 dispatches report no cost\)/);
	assert.match(text, /implementer +impl +cli:muse@muse-spark +running +r2 \[alive\]/);
	const critical = text.indexOf("critical (1)");
	const major = text.indexOf("major (1)");
	const minor = text.indexOf("minor (1)");
	assert.ok(critical > 0 && critical < major && major < minor, text);
	assert.match(text, /- data loss \(review-1, possibly addressed\)\n {6}second line/);
	assert.match(text, /- src\/a\.rs:3 nit \(review-0\)/);
	assert.match(text, /- bug \(review-0, follow-up\)/);
	assert.match(text, /phase review -> awaiting_ship_confirm \(round 2\)/);
	assert.match(text, /slot impl done/);
	assert.match(text, /orchestrator -> broadcast: suite green$/m);
	assert.equal(lines.at(-1), "next: spar ship r1");
});

test("detail without a brief still renders and falls back to spar brief", () => {
	const lines = detailLines({ status: status("r2", { slots: [] }), brief: undefined, bus: undefined, now: new Date() }, plain);
	assert.equal(lines.at(-1), "next: spar brief r2");
	assert.ok(lines.includes("  none dispatched yet"));
});

test("stats cost sums provider groups and run-level-only cost", () => {
	assert.equal(
		statsCost({ groups: [{ key: "a", usage: { cost_usd: 1.25 } }, { key: "b", usage: { cost_usd: null } }], run_level_only: { cost_usd: 2 } }),
		3.25,
	);
});

test("today is measured from local midnight", () => {
	const d = new Date(2026, 9, 15, 2, 30, 0);
	assert.equal(minutesSinceLocalMidnight(d), 150);
	assert.equal(minutesSinceLocalMidnight(new Date(2026, 9, 15, 0, 0, 0)), 1);
});
