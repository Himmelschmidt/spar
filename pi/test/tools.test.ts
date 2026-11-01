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

function ctx(cwd: string, ui?: { answers: string[] }): ExtensionContext & { asked: string[]; options: string[][] } {
	const asked: string[] = [];
	const options: string[][] = [];
	return {
		cwd,
		hasUI: ui !== undefined,
		mode: ui ? "tui" : "print",
		asked,
		options,
		ui: {
			select: async (title: string, opts: string[]) => {
				asked.push(title);
				options.push(opts);
				return ui?.answers.shift();
			},
		},
	} as unknown as ExtensionContext & { asked: string[]; options: string[][] };
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
		"events --run=aaaa1111 --json": { __text: "" },
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
		["bus", "send", "--message=--persist; rm -rf /", "--from=pi", "--to=impl", "--json", "--run=aaaa1111", "--requires-ack"],
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
const keyed = (args: string[]) => [...args.slice(0, -2), "--key=pi-call-1", "--key-scope=local", ...args.slice(-2)];
const PLAN_ARGS = keyed(planArgs(PLAN));
const launched = {
	id: "cccc3333",
	phase: "planning",
	dry_run: false,
	fleet: [{ seat: "planner-cli-claude", role: "planner", provider: "cli:claude", model: null, source: "cli-providers", projected: false }],
};
const MANUAL = { autonomy: "manual", auto_plan: false, auto_winner: false, auto_ship: false };
const validated = (gates = MANUAL) => ({
	ok: true,
	run_id: null,
	kind: "validation",
	gates,
	fleet: [
		{ seat: "planner-cli-claude", role: "planner", provider: "cli:claude", model: null, source: "cli-providers", projected: false },
		{ seat: "review-0-cli-codex", role: "reviewer", provider: "cli:codex", model: "terra", source: "cli-role", projected: true },
	],
});

function launchFake(args = PLAN_ARGS, gates = MANUAL) {
	return installFake({
		[line([...args, "--validate-only"])]: validated(gates),
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

test("the launch dialog: Cancel first, resolved seats, gates, the whole command; cancelling launches nothing", async () => {
	const task = `add a line\n${Array.from({ length: 10 }, (_, i) => `step ${i}`).join("\n")}`;
	const params = { ...PLAN, task, roles: ["reviewer=cli:codex@terra"], fleet: "small" as const };
	const f = launchFake(keyed(planArgs(params)));
	const c = ctx(f.dir, { answers: ["View full task", "Back", "Cancel"] });
	const { res, text } = await call(load(), "spar_plan", params, c);
	assert.equal(res.isError, true);
	assert.match(text, /operator declined/);
	assert.equal(c.asked.length, 3);
	const dialog = c.asked[0]!;
	assert.deepEqual(c.options[0], ["Cancel", "Launch", "View full task"]);
	assert.match(dialog, /^Launch spar plan\?\n\nGates: plan approval and ship wait for you \(autonomy manual\)/);
	assert.match(dialog, /spends money/);
	assert.match(dialog, /step 4\n {6}\(\+5 more lines: choose View full task\)/);
	assert.match(dialog, /planner cli:claude {2}\[planner-cli-claude, cli-providers\]/);
	assert.match(dialog, /reviewer cli:codex@terra \(later phase\)/);
	assert.ok(dialog.includes(`step 9' --providers=cli:claude --role=reviewer=cli:codex@terra --fleet=small --key=pi-call-1`), dialog);
	assert.match(c.asked[1]!, /^Full task\n\nadd a line\n[\s\S]*step 9$/);
	assert.deepEqual(f.argv().map((a) => a.at(-1)), ["--validate-only"], "only the validation ran");
});

test("gates the config passes on its own are a loud warning in the dialog and the result", async () => {
	const gates = { autonomy: "semi", auto_plan: true, auto_winner: false, auto_ship: true };
	const f = launchFake(PLAN_ARGS, gates);
	const c = ctx(f.dir, { answers: ["Launch"] });
	const { text } = await call(load(), "spar_plan", PLAN, c);
	assert.match(c.asked[0]!, /WARNING: this project's config passes these gates WITHOUT you: plan approval; ship \(pushes a branch and opens a draft PR\) \(autonomy semi\)/);
	assert.match(text, /gates the config passes without the operator: plan approval; ship/);
});

test("control characters in a launch are refused before spar or the operator sees them", async () => {
	const f = installFake({});
	const c = ctx(f.dir, { answers: ["Launch"] });
	const { res, text } = await call(load(), "spar_plan", { ...PLAN, task: "ok\x1b[2J\x1b[Hhidden" }, c);
	assert.equal(res.isError, true);
	assert.match(text, /control characters/);
	assert.doesNotMatch(text, /\x1b/);
	assert.equal(c.asked.length, 0);
	assert.deepEqual(f.calls(), []);
});

test("a refused launch never asks the operator", async () => {
	const f = installFake({
		[line([...PLAN_ARGS, "--validate-only"])]: { __exit: 1, __stdout: { ok: false, error: "unknown provider cli:nope", run_id: null, kind: "validation" } },
	});
	const c = ctx(f.dir, { answers: ["Launch"] });
	const { res, text } = await call(load(), "spar_plan", PLAN, c);
	assert.equal(res.isError, true);
	assert.match(text, /refused: unknown provider cli:nope/);
	assert.equal(c.asked.length, 0);
});

test("a confirmed launch runs detached with an idempotency key and reports run id, fleet and next command", async () => {
	const f = launchFake();
	const c = ctx(f.dir, { answers: ["Launch"] });
	const { res, text, details } = await call(load(), "spar_plan", PLAN, c);
	assert.equal(res.isError, undefined);
	assert.equal(details.kind, "launch");
	assert.match(text, /run cccc3333 {2}phase planning/);
	assert.match(text, /exit 0: ok \(phase planning\)/);
	assert.match(text, /planner {2}now {2}cli:claude {2}planner-cli-claude {2}cli-providers/);
	assert.match(text, /next: spar wait cccc3333/);
	assert.deepEqual(f.argv()[1], PLAN_ARGS);
	assert.ok(PLAN_ARGS.includes("--key=pi-call-1"));
});

test("a launch cancelled after spar started says it may have launched", async () => {
	const f = installFake({ [line([...PLAN_ARGS, "--validate-only"])]: validated(), [line(PLAN_ARGS)]: { __hold: true } });
	const controller = new AbortController();
	const c = ctx(f.dir, { answers: ["Launch"] });
	const started = setInterval(() => {
		if (f.calls().length >= 2) {
			clearInterval(started);
			controller.abort();
		}
	}, 20);
	await assert.rejects(call(load(), "spar_plan", PLAN, c, controller.signal), /may have launched. Check spar_status/);
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

test("implement --run shows the run's own seats, and refuses a run at a gate, stopped or abandoned", async () => {
	const args = implementArgs({ run: "dddd4444" });
	assert.ok(!args.some((a) => a.startsWith("--key")), "an existing run keeps its own key");
	const f = installFake({
		[line([...args, "--validate-only"])]: {
			ok: true,
			gates: MANUAL,
			fleet: [{ seat: "impl", role: "implementer", provider: "cli:muse", model: "spark", source: "cli-role", projected: false }],
		},
		"status dddd4444 --json": status("dddd4444", { phase: "plan_approved" }),
		"status eeee5555 --json": status("eeee5555", { phase: "awaiting_ship_confirm", needs_human: true }),
		"status ffff6666 --json": status("ffff6666", { phase: "stopped" }),
		"status abab7777 --json": status("abab7777", { phase: "implement", abandoned: true }),
	});
	const c = ctx(f.dir, { answers: ["Cancel"] });
	await call(load(), "spar_implement", { run: "dddd4444" }, c);
	assert.match(c.asked[0]!, /continues run dddd4444 \(loop, phase plan_approved, round 1\)/);
	assert.match(c.asked[0]!, /Gates: ship waits for you/);
	assert.match(c.asked[0]!, /implementer cli:muse@spark {2}\[impl, cli-role\]/);
	for (const [id, why] of [
		["eeee5555", /waiting on the operator at awaiting_ship_confirm\. The operator can act with: \/spar ship eeee5555 \(each asks them to confirm\)/],
		["ffff6666", /was stopped; resuming it is the operator's call\. The operator can act with: \/spar rule ffff6666/],
		["abab7777", /is abandoned/],
	] as const) {
		const { res, text } = await call(load({ [UNATTENDED_FLAG]: true }), "spar_implement", { run: id }, ctx(f.dir));
		assert.equal(res.isError, true, id);
		assert.match(text, why);
	}
	assert.ok(!f.calls().some((l) => /^implement --run=(eeee|ffff|abab)/.test(l)), "no validation or launch for a refused run");
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
		"status eeee5555 --json": status("eeee5555", { phase: "awaiting_ship_confirm", needs_human: true }),
		"status ffff6666 --json": status("ffff6666", { phase: "quota", resumable: true }),
		"brief ffff6666 --json": { run: "ffff6666", next_command: "spar resume ffff6666", open_findings: [], recent_events: [] },
	});
	let { res, text } = await call(load(), "spar_wait", { run: "eeee5555" }, ctx(f.dir));
	assert.equal(res.isError, undefined, "a gate is a result, not a tool failure");
	assert.match(text, /exit 2: waiting on the human at a gate \(awaiting_ship_confirm\)/);
	assert.match(text, /operator: \/spar ship eeee5555\n/);
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
	const events = Array.from({ length: 50 }, (_, i) => JSON.stringify({ run_id: "aaaa1111", cursor: "c", ts: new Date(0).toISOString(), type: "info", message: `e${i}` })).join("\n");
	const f = installFake({ "logs aaaa1111": { __text: `${lines}\n` }, "events --run=aaaa1111 --json": { __text: `${events}\n` } });
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

test("a child that ignores SIGTERM is killed after the grace period", async () => {
	const f = installFake({ "wait abcd1234": { __hold: "ignore-term" } });
	const started = Date.now();
	const run = await runSpar(["wait", "abcd1234"], f.dir, { timeoutMs: 300, killGraceMs: 300 });
	assert.equal(run.timedOut, true);
	assert.ok(Date.now() - started < 3_000);
});

test("wait: only spar's own timeout means still working; exit 3 otherwise is stuck", async () => {
	const f = installFake({
		"wait abcd1234": { __exit: 3, __stdout: { id: "abcd1234", phase: "stuck", error: "escalated" } },
		"brief abcd1234 --json": { run: "abcd1234", next_command: "spar brief abcd1234", open_findings: [], recent_events: [] },
	});
	const { details, text } = await call(load(), "spar_wait", { run: "abcd1234" }, ctx(f.dir));
	assert.equal(details.kind === "wait" && details.waitedTimeout, false);
	assert.match(text, /exit 3: stuck/);
});

test("escape sequences and carriage returns from spar never reach the model or the screen", async () => {
	const f = installFake({
		"logs aaaa1111": { __text: "\x1b[31mred\x1b[0m\r\nprogress 10%\rprogress 99%\n\x1b]0;title\x07done\n" },
		"brief aaaa1111 --json": { run: "aaaa1111", next_command: "spar ship\x1b[2K aaaa1111", open_findings: [{ slot_id: "r", severity: "major", text: "bad\x1b[8mhidden" }], recent_events: [] },
		"skills get core": { __text: "# x\n\x1b[1mbold\x1b[0m\n## A\nb\n" },
		"status zzzz --json": { __exit: 1, __stderr: "\x1b[31merror:\x1b[0m no run" },
	});
	const outputs = [
		(await call(load(), "spar_logs", { run: "aaaa1111" }, ctx(f.dir))).text,
		(await call(load(), "spar_brief", { run: "aaaa1111" }, ctx(f.dir))).text,
		(await call(load(), "spar_skill", {}, ctx(f.dir))).text,
		(await call(load(), "spar_status", { run: "zzzz" }, ctx(f.dir))).text,
	];
	for (const out of outputs) assert.doesNotMatch(out, /[\x1b\r\x07]/, JSON.stringify(out));
	assert.match(outputs[0]!, /\nred\nprogress 10%progress 99%\ndone$/);
	assert.match(outputs[1]!, /bad hidden|badhidden/);
	assert.match(outputs[3]!, /exited 1: error: no run/);
});
