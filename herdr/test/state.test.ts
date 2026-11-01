import assert from "node:assert/strict";
import { test } from "node:test";
import { inScope } from "../src/config.ts";
import { agentState, relevant } from "../src/spar.ts";
import { row } from "./helpers.ts";

const now = new Date();
const DAY = 86_400_000;
const ago = (ms: number) => new Date(now.getTime() - ms).toISOString();

test("a run at a human gate nobody drives is blocked", () => {
	assert.equal(agentState(row("a", { phase: "awaiting_plan_approval", needs_human: true }), now), "blocked");
	assert.equal(agentState(row("a", { phase: "failed", needs_human: true }), now), "blocked");
});

test("a stale gate is idle, not blocked", () => {
	assert.equal(agentState(row("a", { phase: "awaiting_ship_confirm", needs_human: true, updated_at: ago(8 * DAY) }), now), "idle");
});

test("a run holding a seat is working, even at a gate a reconcile runs under", () => {
	assert.equal(agentState(row("a", { phase: "dispatch", holds_seat: true }), now), "working");
	assert.equal(agentState(row("a", { phase: "awaiting_reconcile", needs_human: true, holds_seat: true }), now), "working");
});

test("a finished run is idle", () => {
	assert.equal(agentState(row("a", { phase: "done" }), now), "idle");
});

test("relevance: needs you, holds a seat, or moved within the recent window", () => {
	const recent = 24 * 3_600_000;
	assert.equal(relevant(row("a", { phase: "done", updated_at: ago(3_600_000) }), now, recent), true);
	assert.equal(relevant(row("a", { phase: "done", updated_at: ago(2 * DAY) }), now, recent), false);
	assert.equal(relevant(row("a", { phase: "failed", needs_human: true, updated_at: ago(3 * DAY) }), now, recent), true);
	assert.equal(relevant(row("a", { phase: "failed", needs_human: true, updated_at: ago(9 * DAY) }), now, recent), false);
	assert.equal(relevant(row("a", { phase: "dispatch", holds_seat: true, updated_at: ago(9 * DAY) }), now, recent), true);
});

test("the project allow-list matches a root or a name; empty means every project", () => {
	const base = { recent_hours: 24, board_on_startup: true, sync_interval_secs: 60 };
	const run = { project_root: "/p/alpha", project_name: "alpha" };
	assert.equal(inScope(base, run), true);
	assert.equal(inScope({ ...base, projects: ["/p/alpha"] }, run), true);
	assert.equal(inScope({ ...base, projects: ["alpha"] }, run), true);
	assert.equal(inScope({ ...base, projects: ["beta"] }, run), false);
});

test("config clamps a zero or negative sync interval so the board cannot spin", async () => {
	const { mkdtempSync, writeFileSync, rmSync } = await import("node:fs");
	const { tmpdir } = await import("node:os");
	const { join } = await import("node:path");
	const { loadConfig } = await import("../src/config.ts");
	const dir = mkdtempSync(join(tmpdir(), "spar-herdr-cfg-"));
	process.env.HERDR_PLUGIN_CONFIG_DIR = dir;
	writeFileSync(join(dir, "config.json"), JSON.stringify({ sync_interval_secs: 0, recent_hours: -3 }));
	const config = loadConfig();
	assert.equal(config.sync_interval_secs, 60);
	writeFileSync(join(dir, "config.json"), JSON.stringify({ sync_interval_secs: -5 }));
	assert.equal(loadConfig().sync_interval_secs, 10);
	assert.equal(config.recent_hours, 0);
	rmSync(dir, { recursive: true, force: true });
});
