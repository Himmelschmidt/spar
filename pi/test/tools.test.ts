import assert from "node:assert/strict";
import { after, afterEach, test } from "node:test";
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import { runSpar } from "../extensions/spar/exec.ts";
import { detailLines, type ToolDetails } from "../extensions/spar/renderers.ts";
import { display, exitMeaning, implementArgs, planArgs, reviewArgs, sparTools, UNATTENDED_ENV, UNATTENDED_FLAG } from "../extensions/spar/tools.ts";
import { plain } from "../extensions/spar/format.ts";
import { cleanup, installFake, installNoSpar, row, status } from "./helpers.ts";

after(cleanup);
afterEach(() => {
	delete process.env[UNATTENDED_ENV];
});

type Tools = ReturnType<typeof sparTools>;

function load(flags: Record<string, boolean> = {}): Tools {
	const pi = { getFlag: (name: string) => flags[name] } as unknown as ExtensionAPI;
	return sparTools(pi);
}

function ctx(cwd: string, ui?: { confirm?: (title: string, body: string) => Promise<boolean> }): ExtensionContext & { asked: string[] } {
	const asked: string[] = [];
	return {
		cwd,
		hasUI: ui !== undefined,
		mode: ui ? "tui" : "print",
		asked,
		ui: {
			confirm: async (title: string, body: string) => {
				asked.push(`${title}\n${body}`);
				return ui?.confirm ? ui.confirm(title, body) : false;
			},
		},
	} as unknown as ExtensionContext & { asked: string[] };
}

async function call(tools: Tools, name: string, params: unknown, c: ExtensionContext, signal?: AbortSignal) {
	const tool = tools.find((t) => t.name === name);
	assert.ok(tool, `no tool ${name}`);
	const res = await tool.execute("call-1", params as never, signal, undefined, c as never);
	const text = res.content[0]?.type === "text" ? res.content[0].text : "";
	return { res, text, details: res.details as ToolDetails };
}

const line = (args: string[]) => args.join(" ");

test("every tool is a spar_ tool and human-only actions are not exposed", () => {
	const names = load().map((t) => t.name);
	assert.deepEqual(names, [
		"spar_status",
		"spar_brief",
		"spar_events",
		"spar_logs",
		"spar_providers",
		"spar_stats",
		"spar_plan",
		"spar_implement",
		"spar_review",
		"spar_wait",
		"spar_bus_send",
		"spar_bus_log",
		"spar_skill",
	]);
	for (const banned of ["approve", "reject", "ship", "confirm", "reconcile", "rule", "cleanup", "reassign", "amend", "override"]) {
		assert.ok(!names.some((n) => n.includes(banned)), banned);
	}
});

test("observe tools pass exact argv without a shell", async () => {
	const f = installFake({
		"status --json": [row("aaaa1111"), row("bbbb2222", { needs_human: true })],
		"status aaaa1111 --json": status("aaaa1111"),
		"brief aaaa1111 --json": { run: "aaaa1111", next_command: "spar ship aaaa1111", open_findings: [], recent_events: [] },
		events: [{ lines: [] }],
		"logs aaaa1111 impl": { __text: "a\nb\n" },
		"provider list --json": [{ name: "claude", quota_status: "available" }],
		"stats --json": { groups: [] },
		"bus log --json": [],
		"bus send": { id: "m1" },
		"skills get core": { __text: "# spar\nintro\n## Exit codes\nzero\n## Swarm bus\nbus\n" },
	});
	const tools = load();
	const c = ctx(f.dir);
	await call(tools, "spar_status", {}, c);
	await call(tools, "spar_status", { run: "aaaa1111", all: true }, c);
	await call(tools, "spar_status", { all: true }, c);
	await call(tools, "spar_brief", { run: "aaaa1111" }, c);
	await call(tools, "spar_events", { run: "aaaa1111" }, c);
	await call(tools, "spar_logs", { run: "aaaa1111", slot: "impl", lines: 1 }, c);
	await call(tools, "spar_providers", { usage: true }, c);
	await call(tools, "spar_stats", { since: "7d", by: "model" }, c);
	await call(tools, "spar_bus_log", { run: "aaaa1111" }, c);
	await call(tools, "spar_bus_send", { message: "--persist; rm -rf /", run: "aaaa1111", to: "impl", requires_ack: true }, c);
	await call(tools, "spar_skill", { section: "exit" }, c);
	assert.deepEqual(f.argv(), [
		["status", "--json"],
		["status", "aaaa1111", "--json"],
		["status", "--json", "--all"],
		["brief", "aaaa1111", "--json"],
		["events", "--run=aaaa1111", "--json"],
		["logs", "aaaa1111", "impl"],
		["provider", "list", "--json", "--usage"],
		["stats", "--json", "--since=7d", "--by=model"],
		["bus", "log", "--json", "--run=aaaa1111"],
		["bus", "send", "--message=--persist; rm -rf /", "--to=impl", "--json", "--run=aaaa1111", "--requires-ack"],
		["skills", "get", "core"],
	]);
});

test("run and slot ids that could be flags are refused before spar runs", async () => {
	const f = installFake({});
	const tools = load();
	for (const [name, params] of [
		["spar_status", { run: "--all" }],
		["spar_brief", { run: "-x" }],
		["spar_logs", { run: "aaaa1111", slot: "--follow" }],
		["spar_wait", { run: "a b" }],
		["spar_implement", { run: "--new" }],
	] as const) {
		const { res, text } = await call(tools, name, params, ctx(f.dir));
		assert.equal(res.isError, true, name);
		assert.match(text, /is not a valid/);
	}
	assert.deepEqual(f.argv(), []);
});

test("launch argv uses --flag=value so a task starting with a dash stays a value", () => {
	assert.deepEqual(
		planArgs({ task: "-t trick", providers: ["cli:claude", " cli:codex "], roles: ["planner=cli:claude@opus"], fleet: "small", without: ["critic", "spec"], effort: ["planner=max"], backups: ["planner=cli:codex"], base: "origin/main", dry_run: true }),
		["plan", "--task=-t trick", "--providers=cli:claude,cli:codex", "--role=planner=cli:claude@opus", "--backup=planner=cli:codex", "--effort=planner=max", "--fleet=small", "--without=critic,spec", "--base=origin/main", "--dry-run", "--detach", "--json"],
	);
	assert.deepEqual(implementArgs({ run: "abcd1234" }), ["implement", "--run=abcd1234", "--detach", "--json"]);
	assert.deepEqual(implementArgs({ task: "fix it", providers: ["cli:claude"] }), ["implement", "--task=fix it", "--providers=cli:claude", "--detach", "--json"]);
	assert.deepEqual(reviewArgs({ task: "review PR 3", roles: ["reviewer=cli:claude", "reviewer=cli:codex"] }), [
		"run",
		"--workflow=review",
		"--task=review PR 3",
		"--role=reviewer=cli:claude",
		"--role=reviewer=cli:codex",
		"--detach",
		"--json",
	]);
});

test("the command shown to the operator is shell-quoted", () => {
	assert.equal(display(["plan", "--task=it's done; rm -rf /", "--providers=cli:claude", "--json"]), "spar plan --task='it'\\''s done; rm -rf /' --providers=cli:claude --json");
});

const PLAN = { task: "add a line", providers: ["cli:claude"] };
const PLAN_ARGS = planArgs(PLAN);
const launched = {
	id: "cccc3333",
	phase: "planning",
	dry_run: false,
	fleet: [{ seat: "planner-cli-claude", role: "planner", provider: "cli:claude", model: null, source: "cli-providers", projected: false }],
};

function launchFake(args = PLAN_ARGS) {
	return installFake({
		[line([...args, "--validate-only"])]: { ok: true, run_id: null, kind: "validation" },
		[line(args)]: launched,
		"brief cccc3333 --json": { run: "cccc3333", next_command: "spar wait cccc3333", open_findings: [], recent_events: [] },
	});
}

test("launch without a UI fails closed: validated, never launched", async () => {
	const f = launchFake();
	const { res, text, details } = await call(load(), "spar_plan", PLAN, ctx(f.dir));
	assert.equal(res.isError, true);
	assert.match(text, /no operator UI to confirm a launch/);
	assert.match(text, new RegExp(`--${UNATTENDED_FLAG}`));
	assert.equal(details.kind, "launch");
	assert.deepEqual(f.calls(), [line([...PLAN_ARGS, "--validate-only"])]);
});

test("launch the operator declines is not run, and the dialog shows the fleet and the command", async () => {
	const params = { ...PLAN, roles: ["reviewer=cli:codex"], fleet: "small" as const };
	const f = launchFake(planArgs(params));
	const c = ctx(f.dir, { confirm: async () => false });
	const { res, text } = await call(load(), "spar_plan", params, c);
	assert.equal(res.isError, true);
	assert.match(text, /operator declined/);
	assert.equal(c.asked.length, 1);
	assert.match(c.asked[0]!, /^Launch spar plan\?/);
	assert.match(c.asked[0]!, /spends money/);
	assert.match(c.asked[0]!, /providers: cli:claude/);
	assert.match(c.asked[0]!, /roles: reviewer=cli:codex/);
	assert.match(c.asked[0]!, /fleet preset: small/);
	assert.match(c.asked[0]!, /spar plan --task='add a line' --providers=cli:claude/);
	assert.ok(!f.calls().some((l) => !l.endsWith("--validate-only")), "only the validation ran");
});

test("a refused launch never asks the operator", async () => {
	const f = installFake({
		[line([...PLAN_ARGS, "--validate-only"])]: { __exit: 1, __stdout: { ok: false, error: "unknown provider cli:nope", run_id: null, kind: "validation" } },
	});
	const c = ctx(f.dir, { confirm: async () => true });
	const { res, text } = await call(load(), "spar_plan", PLAN, c);
	assert.equal(res.isError, true);
	assert.match(text, /refused: unknown provider cli:nope/);
	assert.equal(c.asked.length, 0);
});

test("a confirmed launch runs detached and reports run id, fleet and next command", async () => {
	const f = launchFake();
	const c = ctx(f.dir, { confirm: async () => true });
	const { res, text, details } = await call(load(), "spar_plan", PLAN, c);
	assert.equal(res.isError, undefined);
	assert.equal(details.kind, "launch");
	assert.match(text, /run cccc3333 {2}phase planning/);
	assert.match(text, /exit 0: ok \(phase planning\)/);
	assert.match(text, /planner {2}now {2}cli:claude {2}planner-cli-claude {2}cli-providers/);
	assert.match(text, /next: spar wait cccc3333/);
	assert.deepEqual(f.argv()[1], PLAN_ARGS);
});

test("the unattended flag or env skips the dialog, even with no UI", async () => {
	let f = launchFake();
	let { res } = await call(load({ [UNATTENDED_FLAG]: true }), "spar_plan", PLAN, ctx(f.dir));
	assert.equal(res.isError, undefined);
	assert.equal(f.calls().length, 3);
	f = launchFake();
	process.env[UNATTENDED_ENV] = "1";
	({ res } = await call(load(), "spar_plan", PLAN, ctx(f.dir)));
	assert.equal(res.isError, undefined);
});

test("implement --run shows the run's own seats in the dialog", async () => {
	const args = implementArgs({ run: "dddd4444" });
	const f = installFake({
		[line([...args, "--validate-only"])]: { ok: true },
		"status dddd4444 --json": {
			...status("dddd4444", { phase: "plan_approved" }),
			fleet: [{ seat: "impl", role: "implementer", provider: "cli:muse", model: "spark", projected: true }],
		},
	});
	const c = ctx(f.dir, { confirm: async () => false });
	await call(load(), "spar_implement", { run: "dddd4444" }, c);
	assert.match(c.asked[0]!, /continues run dddd4444 \(loop, phase plan_approved, round 1\)/);
	assert.match(c.asked[0]!, /implementer impl cli:muse@spark \(projected\)/);
});

test("exit codes map to what the model should do", async () => {
	assert.match(exitMeaning(2, "awaiting_plan_approval"), /gate \(awaiting_plan_approval\).*do not try to clear it yourself/);
	assert.match(exitMeaning(3, "stuck"), /stuck/);
	assert.match(exitMeaning(4, "quota"), /no usable providers/);
	assert.match(exitMeaning(1, "failed"), /failed/);
	assert.equal(exitMeaning(0, "done"), "finished");

	const f = installFake({
		"wait eeee5555 --timeout=10m --json": { __exit: 2, __stdout: { id: "eeee5555", phase: "awaiting_ship_confirm" } },
		"wait ffff6666 --timeout=10m --json": { __exit: 4, __stdout: { id: "ffff6666", phase: "quota", error: "all paused" } },
		"brief eeee5555 --json": { run: "eeee5555", next_command: "spar ship eeee5555 --confirm", open_findings: [], recent_events: [] },
		"brief ffff6666 --json": { run: "ffff6666", next_command: "spar resume ffff6666", open_findings: [], recent_events: [] },
	});
	let { res, text } = await call(load(), "spar_wait", { run: "eeee5555" }, ctx(f.dir));
	assert.equal(res.isError, undefined, "a gate is a result, not a tool failure");
	assert.match(text, /exit 2: waiting on the human at a gate \(awaiting_ship_confirm\)/);
	assert.match(text, /next: spar ship eeee5555 --confirm/);
	({ text } = await call(load(), "spar_wait", { run: "ffff6666" }, ctx(f.dir)));
	assert.match(text, /exit 4: no usable providers/);
	assert.match(text, /error: all paused/);
});

test("wait passes a bounded timeout and reports a timeout as still working", async () => {
	const f = installFake({
		"wait abcd1234": { __exit: 3, __stdout: { id: "abcd1234", phase: "review", error: "wait timed out" } },
	});
	const { res, text, details } = await call(load(), "spar_wait", { run: "abcd1234", timeout_minutes: 500 }, ctx(f.dir));
	assert.deepEqual(f.argv()[0], ["wait", "abcd1234", "--timeout=120m", "--json"]);
	assert.equal(res.isError, undefined);
	assert.equal(details.kind === "wait" && details.waitedTimeout, true);
	assert.match(text, /still working after 120m/);
});

test("wait is cancelled by the abort signal and kills the child", async () => {
	const f = installFake({ "wait abcd1234": { __hold: true } });
	const controller = new AbortController();
	const started = Date.now();
	setTimeout(() => controller.abort(), 300);
	await assert.rejects(call(load(), "spar_wait", { run: "abcd1234" }, ctx(f.dir), controller.signal), /cancelled/);
	assert.ok(Date.now() - started < 5_000);
});

test("runSpar's own deadline kills a child that never returns", async () => {
	const f = installFake({ "wait abcd1234": { __hold: true } });
	const run = await runSpar(["wait", "abcd1234"], f.dir, { timeoutMs: 200 });
	assert.equal(run.timedOut, true);
});

test("a missing binary and a failing command are tool errors", async () => {
	installNoSpar();
	let { res, text } = await call(load(), "spar_brief", { run: "aaaa1111" }, ctx(process.cwd()));
	assert.equal(res.isError, true);
	assert.match(text, /not on PATH/);
	const f = installFake({ "brief zzzz --json": { __exit: 1, __stderr: "error: run zzzz not found" } });
	({ res, text } = await call(load(), "spar_brief", { run: "zzzz" }, ctx(f.dir)));
	assert.equal(res.isError, true);
	assert.equal(text, "spar brief exited 1: error: run zzzz not found");
});

test("logs and events are bounded to the tail", async () => {
	const lines = Array.from({ length: 500 }, (_, i) => `line ${i}`).join("\n");
	const events = Array.from({ length: 50 }, (_, i) => ({ run_id: "aaaa1111", cursor: "c", ts: new Date(0).toISOString(), type: "info", message: `e${i}` }));
	const f = installFake({ "logs aaaa1111": { __text: `${lines}\n` }, events: [{ lines: events }] });
	const logs = await call(load(), "spar_logs", { run: "aaaa1111" }, ctx(f.dir));
	assert.equal(logs.details.kind === "logs" && logs.details.lines.length, 80);
	assert.match(logs.text, /last 80 of 500 lines\n/);
	assert.match(logs.text, /line 499$/);
	const ev = await call(load(), "spar_events", { run: "aaaa1111", limit: 3 }, ctx(f.dir));
	assert.match(ev.text, /^last 3 of 50 events\n.*info: e47/);
});

test("skill returns the section list, then one section", async () => {
	const doc = "# spar core skill\nintro line\n## Workflows\nwf body\n### Rounds\nrounds body\n## Exit codes (stable)\n| 0 | ok |\n## Rules\nr\n";
	const f = installFake({ "skills get core": { __text: doc } });
	let { text } = await call(load(), "spar_skill", {}, ctx(f.dir));
	assert.match(text, /intro line/);
	assert.match(text, /- Workflows\n {2}- Rounds\n- Exit codes \(stable\)\n- Rules/);
	assert.doesNotMatch(text, /wf body/);
	({ text } = await call(load(), "spar_skill", { section: "exit codes" }, ctx(f.dir)));
	assert.equal(text, "## Exit codes (stable)\n| 0 | ok |");
	({ text } = await call(load(), "spar_skill", { section: "workflows" }, ctx(f.dir)));
	assert.match(text, /wf body\n### Rounds\nrounds body$/);
});

test("renderers: runs table, findings by severity, events as lines", () => {
	const runs = detailLines({ kind: "runs", runs: [row("aaaa1111"), row("bbbb2222", { needs_human: true })] }, plain);
	assert.match(runs[0]!, /^bbbb2222 +review +r1 .*NEEDS YOU/);
	const brief = detailLines(
		{
			kind: "brief",
			brief: {
				run: "bbbb2222",
				phase: "awaiting_ship_confirm",
				next_command: "spar ship bbbb2222 --confirm",
				open_findings: [
					{ slot_id: "r0", severity: "minor", text: "nit" },
					{ slot_id: "r1", severity: "critical", text: "data loss", file: "src/a.rs", line: 3 },
				],
				recent_events: [{ ts: new Date().toISOString(), type: "phase", prev_phase: "review", phase: "awaiting_ship_confirm" }],
			},
		},
		plain,
	).join("\n");
	assert.match(brief, /critical \(1\)\n {4}- src\/a\.rs:3 data loss \(r1\)\n {2}minor \(1\)\n {4}- nit \(r0\)/);
	assert.match(brief, /phase review -> awaiting_ship_confirm/);
	assert.match(brief, /next: spar ship bbbb2222 --confirm$/);
});
