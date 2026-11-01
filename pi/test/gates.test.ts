import assert from "node:assert/strict";
import { existsSync, rmSync } from "node:fs";
import { after, test } from "node:test";
import {
	type ActionResult,
	actionsFor,
	buildArgv,
	cliCommand,
	type GateContext,
	type GateRun,
	nextRound,
	performAction,
	report,
	setReporter,
	verbCommand,
	verbCompletions,
} from "../extensions/spar/gates.ts";
import { cleanup, installFake, row, status } from "./helpers.ts";

after(cleanup);

interface Script {
	confirm?: boolean[];
	input?: (string | undefined)[];
	select?: (string | undefined)[];
}

function fakeCtx(cwd: string, script: Script = {}, mode = "tui") {
	const dialogs: string[] = [];
	const notes: { text: string; level: string }[] = [];
	setReporter((text, level) => notes.push({ text, level }));
	const ctx: GateContext = {
		cwd,
		mode,
		hasUI: mode === "tui" || mode === "rpc",
		ui: {
			confirm: async (title, message) => {
				dialogs.push(`confirm: ${title}\n${message}`);
				return script.confirm?.shift() ?? true;
			},
			input: async (title) => {
				dialogs.push(`input: ${title}`);
				return script.input?.shift();
			},
			select: async (title, options) => {
				dialogs.push(`select: ${title} [${options.join(" | ")}]`);
				return script.select?.shift();
			},
			notify: (text, level) => {
				notes.push({ text, level: level ?? "info" });
			},
		},
	};
	return { ctx, dialogs, notes };
}

function run(phase: string, over: Partial<GateRun> = {}): GateRun {
	return status("r1", { phase, ...over });
}

const ran = (r: ActionResult) => {
	assert.equal(r.kind, "ran");
	return (r as Extract<ActionResult, { kind: "ran" }>).outcome;
};

async function finished(r: ActionResult) {
	assert.equal(r.kind, "started");
	return (r as Extract<ActionResult, { kind: "started" }>).done;
}

test("each phase offers the actions valid there", () => {
	const at = (phase: string, over = {}) => actionsFor(row("r1", { phase, ...over }));
	assert.deepEqual(at("awaiting_plan_approval"), ["approve", "reject", "rule"]);
	assert.deepEqual(at("awaiting_ship_confirm"), ["ship", "rule"]);
	assert.deepEqual(at("awaiting_winner_confirm"), ["confirm", "reconcile", "rule"]);
	assert.deepEqual(at("awaiting_reconcile"), ["override", "confirm", "reconcile", "rule"]);
	assert.deepEqual(at("awaiting_reconcile", { holds_seat: true }), ["stop", "rule"], "a running reconcile");
	assert.deepEqual(at("awaiting_ship_confirm", { holds_seat: true }), ["rule"], "a ship in progress");
	assert.deepEqual(at("shipping", { holds_seat: true }), ["rule"]);
	assert.deepEqual(at("awaiting_round_extension", { resumable: true }), ["override", "extend", "rule"]);
	assert.deepEqual(at("stuck", { resumable: true }), ["override", "resume", "rule", "reassign"]);
	assert.deepEqual(at("stuck", { resumable: false }), ["override", "rule"]);
	assert.deepEqual(at("stopped", { resumable: true }), ["resume", "rule", "reassign"]);
	assert.deepEqual(at("stopped", { resumable: false }), ["rule"]);
	assert.deepEqual(at("plan_approved", { resumable: true }), ["resume", "rule", "reassign"]);
	assert.deepEqual(at("review", { holds_seat: true }), ["stop", "rule"]);
	assert.deepEqual(at("dispatch", { abandoned: true, resumable: true }), ["resume", "stop", "rule"]);
	assert.deepEqual(at("done"), ["rule", "cleanup"]);
	assert.deepEqual(at("plan_rejected"), ["rule"]);
});

test("every action builds argv with ids and text as separate arguments", () => {
	assert.deepEqual(buildArgv("approve", "r1"), ["approve", "r1", "--json"]);
	assert.deepEqual(buildArgv("reject", "r1"), ["reject", "r1", "--json"]);
	assert.deepEqual(buildArgv("reject", "r1", { reason: "-no tests" }), ["reject", "r1", "--reason=-no tests", "--json"]);
	assert.deepEqual(buildArgv("ship", "r1"), ["ship", "r1", "--confirm", "--json"]);
	assert.deepEqual(buildArgv("override", "r1", { reason: "known flake" }), ["ship", "r1", "--override-gate", "--reason=known flake", "--json"]);
	assert.deepEqual(buildArgv("confirm", "r1"), ["confirm", "r1", "--json"]);
	assert.deepEqual(buildArgv("confirm", "r1", { winner: "impl-1" }), ["confirm", "r1", "--winner", "impl-1", "--json"]);
	assert.deepEqual(buildArgv("reconcile", "r1"), ["reconcile", "r1", "--json"]);
	assert.deepEqual(buildArgv("extend", "r1", { rounds: 3 }), ["implement", "--run", "r1", "--max-rounds", "+3", "--detach", "--json"]);
	assert.deepEqual(buildArgv("resume", "r1"), ["resume", "r1", "--detach", "--json"]);
	assert.deepEqual(buildArgv("stop", "r1"), ["stop", "r1", "--json"]);
	assert.deepEqual(buildArgv("rule", "r1", { text: "--waive is fine; $(rm -rf)", waive: ["AC-2"] }), [
		"rule",
		"add",
		"--run",
		"r1",
		"--waive",
		"AC-2",
		"--json",
		"--",
		"--waive is fine; $(rm -rf)",
	]);
	assert.deepEqual(buildArgv("reassign", "r1", { seat: "impl", provider: "cli:claude@sonnet" }), [
		"implement",
		"--run",
		"r1",
		"--reassign",
		"impl=cli:claude@sonnet",
		"--detach",
		"--json",
	]);
	assert.deepEqual(buildArgv("cleanup", "r1"), ["cleanup", "r1", "--if-merged", "--json"]);
});

test("approve states the exact command, runs it and shows the result", async () => {
	const f = installFake({ "approve r1 --json": { id: "r1", phase: "plan_approved" } });
	const { ctx, dialogs, notes } = fakeCtx(f.dir);
	const out = ran(await performAction(ctx, "approve", run("awaiting_plan_approval")));
	assert.equal(out.ok, true);
	assert.match(dialogs[0]!, /^confirm: Approve the plan for run r1\?\nRuns: spar approve r1 --json\n/);
	assert.deepEqual(f.calls(), ["approve r1 --json"]);
	assert.deepEqual(notes, [{ text: "spar approve r1: done, phase plan_approved", level: "info" }]);
});

test("declining the confirmation runs nothing", async () => {
	const f = installFake({});
	const { ctx, notes } = fakeCtx(f.dir, { confirm: [false] });
	assert.equal((await performAction(ctx, "stop", run("review"))).kind, "cancelled");
	assert.deepEqual(f.calls(), []);
	assert.equal(notes[0]!.text, "spar: cancelled, nothing ran");
});

test("reject takes an optional reason and its exit 1 with plan_rejected is success", async () => {
	const f = installFake({ "reject r1": { __exit: 1, __stdout: { id: "r1", phase: "plan_rejected" }, __stderr: null } });
	const { ctx, dialogs, notes } = fakeCtx(f.dir, { input: ["scope is wrong"] });
	const out = ran(await performAction(ctx, "reject", run("awaiting_plan_approval")));
	assert.equal(out.ok, true);
	assert.match(dialogs[1]!, /Runs: spar reject r1 '--reason=scope is wrong' --json/);
	assert.deepEqual(f.calls(), ["reject r1 --reason=scope is wrong --json"]);
	assert.equal(notes[0]!.text, "spar reject r1: done, phase plan_rejected");

	const g = installFake({ "reject r1 --json": { __exit: 1, __stdout: { id: "r1", phase: "plan_rejected" }, __stderr: null } });
	const empty = fakeCtx(g.dir, { input: [""] });
	assert.equal(ran(await performAction(empty.ctx, "reject", run("awaiting_plan_approval"))).ok, true);
	assert.deepEqual(g.calls(), ["reject r1 --json"]);
});

test("a refusal shows spar's stderr and exit meaning in full", async () => {
	const f = installFake({
		"approve r1 --json": { __exit: 1, __stderr: "error: run r1 is not awaiting plan approval (phase=Review)\ncaused by: x" },
	});
	const { ctx, notes } = fakeCtx(f.dir);
	const out = ran(await performAction(ctx, "approve", run("awaiting_plan_approval")));
	assert.equal(out.ok, false);
	assert.deepEqual(notes, [
		{
			text: "spar approve r1 failed (exit 1: failed)\nerror: run r1 is not awaiting plan approval (phase=Review)\ncaused by: x",
			level: "error",
		},
	]);
});

test("a --json validation refusal shows its error", async () => {
	const f = installFake({
		"implement --run r1 --max-rounds +2 --detach --json": {
			__exit: 1,
			__stdout: { ok: false, error: "round ceiling 8 is not above round 9", run_id: "r1", kind: "validation" },
			__stderr: null,
		},
	});
	const { ctx, notes } = fakeCtx(f.dir, { input: ["2"] });
	const out = ran(await performAction(ctx, "extend", run("awaiting_round_extension", { round: 7 })));
	assert.equal(out.ok, false);
	assert.equal(notes[0]!.text, "spar extend r1 failed (exit 1: failed)\nround ceiling 8 is not above round 9");
});

test("extend defaults to one round, accepts +N and refuses nonsense without running", async () => {
	const f = installFake({ "implement --run r1": { id: "r1", phase: "dispatch" } });
	const one = fakeCtx(f.dir, { input: [""] });
	await performAction(one.ctx, "extend", run("awaiting_round_extension", { round: 6 }));
	assert.match(one.dialogs[1]!, /^confirm: Buy 1 more round for run r1\?\nRuns: spar implement --run r1 --max-rounds \+1 --detach --json\n\nRaises the ceiling to round 7/);
	const plus = fakeCtx(f.dir, { input: ["+3"] });
	await performAction(plus.ctx, "extend", run("awaiting_round_extension"));
	const bad = fakeCtx(f.dir, { input: ["0"] });
	assert.equal((await performAction(bad.ctx, "extend", run("awaiting_round_extension"))).kind, "cancelled");
	assert.equal(bad.notes[0]!.level, "error");
	assert.equal(one.notes[0]!.text, "spar extend r1: done, continuing detached; follow with spar wait r1");
	assert.deepEqual(f.calls(), [
		"implement --run r1 --max-rounds +1 --detach --json",
		"implement --run r1 --max-rounds +3 --detach --json",
	]);
});

test("ship at the ship gate runs in the background and reports a failure with spar's error", async () => {
	const f = installFake({
		"ship r1 --confirm --json": {
			__exit: 1,
			// The confirmation, then the ship result, each pretty-printed.
			__stdout: `${JSON.stringify({ id: "r1", phase: "awaiting_ship_confirm", error: null }, null, 2)}\n${JSON.stringify(
				{ id: "r1", phase: "awaiting_ship_confirm", error: "ship partial failure; see artifacts/ship.md", gates: { ship_confirmed: true } },
				null,
				2,
			)}\n`,
			__stderr: "warning: gh not authenticated",
		},
	});
	const { ctx, notes } = fakeCtx(f.dir);
	const out = await finished(await performAction(ctx, "ship", run("awaiting_ship_confirm")));
	assert.equal(out.ok, false);
	assert.equal(notes[0]!.text, "spar: started in the background: spar ship r1 --confirm --json");
	assert.match(notes[1]!.text, /^spar ship r1 failed \(exit 1: failed\)\nship partial failure; see artifacts\/ship.md\nwarning: gh not authenticated\noutput kept in (\S+)$/);
	const kept = /output kept in (\S+)$/.exec(notes[1]!.text)![1]!;
	assert.ok(existsSync(kept));
	rmSync(kept, { recursive: true, force: true });
	assert.deepEqual(f.calls(), ["ship r1 --confirm --json"]);
});

test("override needs a reason and says why a plain ship is refused", async () => {
	const f = installFake({ "ship r1 --override-gate": { id: "r1", phase: "done", pr_url: "https://example/pr/1" } });
	const { ctx, dialogs, notes } = fakeCtx(f.dir, { input: ["flaky test, tracked in #9"] });
	const out = await finished(await performAction(ctx, "override", run("awaiting_round_extension")));
	assert.equal(out.ok, true);
	assert.match(dialogs[1]!, /Runs: spar ship r1 --override-gate '--reason=flaky test, tracked in #9' --json\n\nAt awaiting_round_extension a plain ship is refused/);
	assert.deepEqual(f.calls(), ["ship r1 --override-gate --reason=flaky test, tracked in #9 --json"]);
	assert.equal(notes.at(-1)!.text, "spar override r1: done, phase done, https://example/pr/1");

	const g = installFake({});
	const blank = fakeCtx(g.dir, { input: [""] });
	assert.equal((await performAction(blank.ctx, "override", run("stuck"))).kind, "cancelled");
	assert.equal(blank.notes[0]!.text, "spar: an override needs a reason; nothing ran");
	assert.deepEqual(g.calls(), []);
});

test("a run with a background job of ours in flight offers no gate verbs until it ends", async () => {
	const f = installFake({ "ship r1 --confirm --json": { id: "r1", phase: "done" } });
	const { ctx } = fakeCtx(f.dir);
	const started = await performAction(ctx, "ship", run("awaiting_ship_confirm"));
	assert.deepEqual(actionsFor(row("r1", { phase: "awaiting_ship_confirm" })), ["rule"]);
	await finished(started);
	assert.deepEqual(actionsFor(row("r1", { phase: "awaiting_ship_confirm" })), ["ship", "rule"]);
});

test("a background result with no live session is dropped, and a throwing UI is contained", async () => {
	const f = installFake({ "reconcile r1 --json": { id: "r1", phase: "awaiting_ship_confirm" } });
	const { ctx, notes } = fakeCtx(f.dir);
	const started = await performAction(ctx, "reconcile", run("awaiting_reconcile"));
	setReporter(undefined);
	assert.equal((await finished(started)).ok, true);
	assert.equal(notes.length, 1, "only the start notice");
	setReporter(() => {
		throw new Error("This extension ctx is stale");
	});
	assert.doesNotThrow(() => report("late", "info"));
	setReporter(undefined);
});

test("confirm offers the ranked default or a pick among implementers", async () => {
	const f = installFake({ confirm: { id: "r1", phase: "awaiting_ship_confirm" } });
	const slots = [
		{ id: "impl-0-cli-claude", role: "implementer", provider: "cli:claude", status: "done", round: 1 },
		{ id: "impl-1-cli-codex", role: "implementer", provider: "cli:codex", status: "done", round: 1 },
	];
	const pick = fakeCtx(f.dir, { select: ["impl-1-cli-codex"] });
	await performAction(pick.ctx, "confirm", run("awaiting_winner_confirm", { slots, winner_slot: "impl-0-cli-claude" }));
	assert.match(pick.dialogs[0]!, /\[ranked default \(impl-0-cli-claude\) \| impl-0-cli-claude \| impl-1-cli-codex\]/);
	const ranked = fakeCtx(f.dir, { select: ["ranked default (impl-0-cli-claude)"] });
	await performAction(ranked.ctx, "confirm", run("awaiting_winner_confirm", { slots, winner_slot: "impl-0-cli-claude" }));
	assert.deepEqual(f.calls(), ["confirm r1 --winner impl-1-cli-codex --json", "confirm r1 --json"]);
});

test("reconcile and cleanup run in the background", async () => {
	const f = installFake({ reconcile: { id: "r1", phase: "awaiting_ship_confirm" }, cleanup: { run_id: "r1", removed: 2 } });
	const { ctx, notes } = fakeCtx(f.dir);
	await finished(await performAction(ctx, "reconcile", run("awaiting_reconcile")));
	await finished(await performAction(ctx, "cleanup", run("done")));
	assert.deepEqual(f.calls(), ["reconcile r1 --json", "cleanup r1 --if-merged --json"]);
	assert.equal(notes.at(-1)!.text, "spar cleanup r1: done, 2 worktree(s) removed");
});

test("rule takes text and waived criteria; reassign picks a seat and a provider", async () => {
	const f = installFake({ rule: { id: "R1", text: "x" }, "implement --run r1": { id: "r1", phase: "dispatch" } });
	const rule = fakeCtx(f.dir, { input: ["-1 is fine here", "AC-2, AC-5"] });
	assert.equal(ran(await performAction(rule.ctx, "rule", run("review"))).text, "spar rule r1: done, ruling R1 recorded");
	const badWaive = fakeCtx(f.dir, { input: ["text", "two"] });
	assert.equal((await performAction(badWaive.ctx, "rule", run("review"))).kind, "cancelled");

	const move = fakeCtx(f.dir, { select: ["impl  implementer  cli:muse@muse-spark"], input: ["cli:claude@sonnet"] });
	await performAction(move.ctx, "reassign", run("stopped", { round: 2, dispatched_round: 2 }));
	assert.match(move.dialogs[2]!, /continues, detached, and starts round 3\.$/);
	assert.match(move.dialogs[0]!, /^select: Reassign which seat of run r1\? \[impl  implementer  cli:muse@muse-spark \| review-0-cli-claude/);
	assert.deepEqual(f.calls(), [
		"rule add --run r1 --waive AC-2 --waive AC-5 --json -- -1 is fine here",
		"implement --run r1 --reassign impl=cli:claude@sonnet --detach --json",
	]);
});

test("/spar <verb> refuses without a dialog-capable UI instead of acting", async () => {
	const f = installFake({});
	const print = fakeCtx(f.dir, {}, "print");
	const original = process.stdout.write.bind(process.stdout);
	let out = "";
	process.stdout.write = ((chunk: string) => {
		out += chunk;
		return true;
	}) as typeof process.stdout.write;
	try {
		assert.equal(await verbCommand(print.ctx, "approve", "r1"), undefined);
	} finally {
		process.stdout.write = original;
	}
	assert.match(out, /^spar: \/spar approve needs a confirmation dialog, which this mode \(print\) has no way to show; nothing ran\. .*run it yourself: spar approve r1\n$/);
	assert.deepEqual(f.calls(), []);
});

test("refusals name the real spar command for every verb", () => {
	assert.equal(cliCommand("ship", "r1"), "spar ship r1 --confirm");
	assert.equal(cliCommand("override", "r1"), "spar ship r1 --override-gate --reason=<why>");
	assert.equal(cliCommand("rule", "r1"), 'spar rule add --run r1 [--waive AC-<n>] -- "<text>"');
	assert.equal(cliCommand("reassign", "r1"), "spar implement --run r1 --reassign <seat>=<provider> --detach");
	assert.equal(cliCommand("extend", "r1"), "spar implement --run r1 --max-rounds +<N> --detach");
	assert.equal(cliCommand("cleanup", "r1"), "spar cleanup r1 --if-merged");
});

test("reassign names the round it starts", () => {
	assert.equal(nextRound(run("stopped", { round: 2, dispatched_round: 2 })), "starts round 3");
	assert.equal(nextRound(run("stopped", { round: 2, dispatched_round: 1 })), "re-runs round 2, which never dispatched");
});

test("/spar <verb> <id> checks the phase, then dialogs; preset text skips the input", async () => {
	const f = installFake({
		"status r1 --json": status("r1", { phase: "review", holds_seat: true }),
		"status r2 --json": status("r2", { phase: "awaiting_plan_approval" }),
		"reject r2": { __exit: 1, __stdout: { id: "r2", phase: "plan_rejected" }, __stderr: null },
	});
	const wrong = fakeCtx(f.dir, {}, "rpc");
	assert.equal(await verbCommand(wrong.ctx, "approve", "r1"), undefined);
	assert.equal(wrong.notes[0]!.text, "spar: run r1 is at review and running; approve does not apply. From here: stop, rule");

	const right = fakeCtx(f.dir, {}, "rpc");
	const result = await verbCommand(right.ctx, "reject", " r2  not this way ");
	assert.equal(result?.kind, "ran");
	assert.equal(right.dialogs.length, 1, "only the confirmation");
	assert.deepEqual(f.calls(), ["status r1 --json", "status r2 --json", "reject r2 --reason=not this way --json"]);

	const bad = fakeCtx(f.dir, {}, "rpc");
	await verbCommand(bad.ctx, "approve", "--all");
	assert.equal(bad.notes[0]!.text, 'spar: "--all" is not a run id. Usage: /spar approve <run-id>');
});

test("/spar <verb> without an id asks which run, among those it applies to", async () => {
	const f = installFake({
		"status --json": [row("r1", { phase: "review", holds_seat: true }), row("r2", { phase: "awaiting_plan_approval", task: "do it" })],
		"status r2 --json": status("r2", { phase: "awaiting_plan_approval" }),
		"approve r2 --json": { id: "r2", phase: "plan_approved" },
	});
	const { ctx, dialogs } = fakeCtx(f.dir, { select: ["r2  awaiting_plan_approval  do it"] });
	await verbCommand(ctx, "approve", "");
	assert.equal(dialogs[0], "select: approve: which run? [r2  awaiting_plan_approval  do it]");
	assert.deepEqual(f.calls(), ["status --json", "status r2 --json", "approve r2 --json"]);
});

test("verb completion offers verbs, then the runs each verb applies to", () => {
	const runs = [row("aa11", { phase: "awaiting_plan_approval" }), row("ab22", { phase: "review", holds_seat: true })];
	assert.deepEqual(
		verbCompletions("re", runs)?.map((i) => i.value),
		["reject ", "reconcile ", "resume ", "reassign "],
	);
	assert.deepEqual(
		verbCompletions("approve ", runs)?.map((i) => i.value),
		["approve aa11"],
	);
	assert.deepEqual(
		verbCompletions("stop a", runs)?.map((i) => i.value),
		["stop ab22"],
	);
	assert.equal(verbCompletions("aa11 x", runs), undefined);
	assert.deepEqual(verbCompletions("approve zz", runs), []);
});
