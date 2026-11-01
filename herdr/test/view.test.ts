import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { tmpdir } from "node:os";
import { afterEach, test } from "node:test";
import type { Brief } from "../src/spar.ts";
import { renderView } from "../src/view.ts";
import { wrap } from "../src/wrap.ts";
import { cleanup, installHerdr, installSpar, MAIN, status } from "./helpers.ts";

afterEach(cleanup);

const ROOT = tmpdir();

const LONG = "cli:opencode@openrouter/some-vendor/a-very-long-model-name-that-goes-on-and-on-and-on-v2";

function wide() {
	return status("960d210b", {
		phase: "awaiting_round_extension",
		needs_human: true,
		resumable: true,
		round: 6,
		task: "The live change stream serves a table only when its owning module declares the table's read rule in Rust, and nothing else streams",
		error: "round ceiling reached: round 6 of a maximum 6. Buy more with `spar implement --run 960d210b --max-rounds 8`",
		slots: [
			{ id: "impl", role: "implementer", provider: LONG, status: "done", round: 6 },
			{ id: "review-0-cli-codex", role: "reviewer", provider: "cli:codex", model: "gpt-6.1-terra", status: "failed", round: 6, quota_hit: true },
		],
	});
}

const brief: Brief = {
	run: "960d210b",
	next_command: "spar implement --run 960d210b --max-rounds <N>",
	open_findings: [
		{ slot_id: "review-0", severity: "critical", text: "auth stamps nothing" },
		{ slot_id: "review-0", severity: "major", text: "declarations still module-wide" },
	],
	recent_events: [
		{ ts: new Date().toISOString(), type: "slot", slot: "review-0-cli-codex", status: "failed", message: "x".repeat(300) },
		{ ts: new Date().toISOString(), type: "phase", prev_phase: "review", phase: "awaiting_round_extension", round: 6 },
	],
};

for (const width of [72, 140]) {
	test(`the run view fits ${width} columns and shows every seat's status before its provider`, () => {
		const lines = renderView({ status: wide(), brief, now: new Date(), width });
		for (const l of lines) assert.ok(l.text.length <= width, `${l.text.length} > ${width}: ${l.text}`);
		const text = lines.map((l) => l.text).join("\n");
		assert.match(text, /^spar 960d210b NEEDS YOU/);
		assert.match(text, /awaiting_round_extension \| round 6\/6/);
		assert.match(text, /gate: awaiting_round_extension; can override, extend, rule/);
		assert.match(text, /^ {2}done impl r6\b/m);
		assert.match(text, /^ {2}failed \[QUOTA\] review-0-cli-codex r6 cli:codex@gpt-6.1-terra/m);
		assert.match(text, /findings: 2 open \(1 critical, 1 major\)/);
		assert.match(text, /next: spar implement --run 960d210b --max-rounds <N>/);
		assert.ok(text.replace(/\s+/g, "").includes(LONG), "the long provider wraps instead of being cut");
	});
}

test("a short pane drops events first and keeps the header and next command", () => {
	const lines = renderView({ status: wide(), brief, now: new Date(), width: 72, height: 8 });
	assert.ok(lines.length <= 8);
	assert.match(lines[0]!.text, /^spar 960d210b/);
	assert.match(lines.at(-1)!.text, /^next: /);
	assert.ok(!lines.some((l) => l.text === "recent"));
});

test("wrap never exceeds the width and keeps indentation", () => {
	assert.deepEqual(wrap("  alpha beta gamma", 12, 2), ["  alpha beta", "    gamma"]);
	for (const l of wrap(`x ${"y".repeat(50)} z`, 10)) assert.ok(l.length <= 10);
	assert.deepEqual(wrap("", 10), [""]);
});

function runView(env: Record<string, string>): Promise<{ code: number | null; stdout: string }> {
	return new Promise((resolve, reject) => {
		const child = spawn(process.execPath, [MAIN, "run"], { env: { ...process.env, ...env }, stdio: ["ignore", "pipe", "pipe"] });
		let stdout = "";
		child.stdout.on("data", (d) => {
			stdout += d;
		});
		const timer = setTimeout(() => {
			child.kill("SIGKILL");
			reject(new Error(`run view did not exit; output:\n${stdout}`));
		}, 20_000);
		child.on("close", (code) => {
			clearTimeout(timer);
			resolve({ code, stdout });
		});
	});
}

const GONE = { __exit: 1, __stderr: "error: run r1 not found in current project or global registry" };

test("the run view reports blocked at a gate, then exits when the run disappears", async () => {
	const gate = status("r1", { phase: "awaiting_plan_approval", needs_human: true, slots: [] });
	installSpar({
		"status r1 --json": { __seq: [gate, GONE] },
		"brief r1 --json": { ...brief, run: "r1" },
		events: [{ lines: [{ run_id: "r1", cursor: "1,r1:9", ts: new Date().toISOString(), type: "info", message: "x" }] }],
	});
	const herdr = installHerdr({
		workspaces: [{ workspace_id: "w1", label: "alpha" }],
		tabs: [{ tab_id: "w1:t1", workspace_id: "w1", label: "spar" }],
		panes: [{ pane_id: "w1:p1", tab_id: "w1:t1", workspace_id: "w1" }],
	});
	const { code, stdout } = await runView({ SPAR_HERDR_RUN: "r1", SPAR_HERDR_ROOT: ROOT, HERDR_PANE_ID: "w1:p1" });
	assert.equal(code, 0);
	assert.match(stdout, /spar r1 NEEDS YOU/);
	const reports = herdr.argv().filter((a) => a[1] === "report-agent");
	assert.equal(reports.length, 1, "state is reported once, not on every refresh");
	assert.deepEqual(reports[0]!.slice(0, 3), ["pane", "report-agent", "w1:p1"]);
	assert.equal(reports[0]![reports[0]!.indexOf("--state") + 1], "blocked");
	const pane = herdr.world().panes[0]!;
	assert.deepEqual(pane.tokens, { summary: "awaiting_plan_approval", spar_run: "r1", spar_root: ROOT });
});

test("the run view exits at once when the run is archived", async () => {
	installSpar({ "status r1 --json": status("r1", { phase: "done", archived_at: new Date().toISOString() }) });
	installHerdr();
	const { code } = await runView({ SPAR_HERDR_RUN: "r1", SPAR_HERDR_ROOT: ROOT });
	assert.equal(code, 0);
});

test("the run view backs off on spar errors instead of spinning", async () => {
	const spar = installSpar({ "status r1 --json": { __exit: 1, __stderr: "error: state.json is locked" } });
	installHerdr();
	const child = spawn(process.execPath, [MAIN, "run"], { env: { ...process.env, SPAR_HERDR_RUN: "r1", SPAR_HERDR_ROOT: ROOT }, stdio: "ignore" });
	await new Promise((r) => setTimeout(r, 4_000));
	child.kill("SIGTERM");
	const calls = spar.argv().filter((a) => a[0] === "status").length;
	assert.ok(calls >= 2 && calls <= 3, `${calls} status calls in 4s`);
});
