import assert from "node:assert/strict";
import { test } from "node:test";
import { AttentionTracker } from "../extensions/spar/attention.ts";
import type { FeedEvent } from "../extensions/spar/feed.ts";
import { row } from "./helpers.ts";

const ev = (run_id: string, type: string, phase?: string): FeedEvent => ({
	run_id,
	cursor: "1",
	ts: "2026-11-01T10:00:00Z",
	type,
	...(phase ? { phase } : {}),
});

test("alerts once when a run enters a gate; the gate event after the phase event is no repeat", () => {
	const t = new AttentionTracker();
	assert.equal(t.onEvent(ev("r1", "phase", "review")), undefined);
	assert.deepEqual(t.onEvent(ev("r1", "phase", "awaiting_ship_confirm")), {
		runId: "r1",
		message: "spar: run r1 needs you: waiting at awaiting_ship_confirm. /spar r1",
		level: "warning",
	});
	assert.equal(t.onEvent(ev("r1", "gate", "awaiting_ship_confirm")), undefined);
	assert.equal(t.onEvent(ev("r1", "slot")), undefined);
	assert.equal(t.onEvent(ev("r1", "phase", "awaiting_ship_confirm")), undefined);
});

test("reconcile's own in-progress phase is not a gate to alert on", () => {
	const t = new AttentionTracker();
	assert.ok(t.onEvent(ev("r1", "phase", "awaiting_winner_confirm")));
	assert.equal(t.onEvent(ev("r1", "phase", "awaiting_reconcile")), undefined);
	assert.equal(t.onEvent(ev("r1", "gate", "awaiting_reconcile")), undefined);
	assert.ok(t.onEvent(ev("r1", "phase", "awaiting_ship_confirm")));
});

test("stuck, failed, quota and abandoned alert as errors; leaving and re-entering alerts again", () => {
	const t = new AttentionTracker();
	assert.equal(t.onEvent(ev("r1", "phase", "stuck"))?.level, "error");
	assert.equal(t.onEvent(ev("r2", "phase", "quota"))?.message, "spar: run r2 needs you: quota. /spar r2");
	assert.equal(t.onEvent(ev("r3", "phase", "failed"))?.level, "error");
	assert.equal(
		t.onEvent(ev("r4", "abandoned", "dispatch"))?.message,
		"spar: run r4 needs you: abandoned in dispatch, no live orchestrator. /spar r4",
	);
	assert.equal(t.onEvent(ev("r4", "abandoned", "dispatch")), undefined, "a restarted daemon repeating itself");
	assert.equal(t.onEvent(ev("r1", "phase", "dispatch")), undefined);
	assert.ok(t.onEvent(ev("r1", "phase", "stuck")), "a new entry after leaving");
	assert.equal(t.onEvent(ev("r1", "phase", "done")), undefined);
	assert.equal(t.onEvent(ev("r1", "phase", "stopped")), undefined);
});

test("quiet for runs already waiting at startup, but not for runs that changed after the feed started", () => {
	const t = new AttentionTracker();
	const start = new Date();
	t.seed(
		[
			row("old", { phase: "awaiting_plan_approval", needs_human: true }),
			row("driven", { phase: "awaiting_reconcile", needs_human: true, holds_seat: true }),
			row("gone", { phase: "dispatch", needs_human: true, abandoned: true }),
			row("fresh", { phase: "awaiting_plan_approval", needs_human: true, updated_at: new Date(start.getTime() + 5).toISOString() }),
		],
		start,
	);
	assert.equal(t.onEvent(ev("old", "gate", "awaiting_plan_approval")), undefined);
	assert.equal(t.onEvent(ev("gone", "abandoned", "dispatch")), undefined);
	assert.ok(t.onEvent(ev("fresh", "phase", "awaiting_plan_approval")));
	assert.deepEqual(t.onEvent(ev("old", "phase", "plan_approved")), {
		runId: "old",
		message: "spar: run old needs you: plan approved, waiting to be implemented (resume it). /spar old",
		level: "warning",
	});
});
