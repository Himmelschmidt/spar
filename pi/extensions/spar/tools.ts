import type { AgentToolResult, ExtensionAPI, ExtensionContext, Theme, ToolDefinition } from "@earendil-works/pi-coding-agent";
import { isToolCallEventType } from "@earendil-works/pi-coding-agent";
import { StringEnum } from "@earendil-works/pi-ai";
import { Text } from "@earendil-works/pi-tui";
import { type Static, type TSchema, Type } from "typebox";
import { clean, firstLine, hasControls, jsonLines, parseJson, runSpar, type SparRun } from "./exec.ts";
import { sortRuns } from "./format.ts";
import { judgeBash } from "./guard.ts";
import { type BriefDetail, type FleetSeat, modelText, type Outcome, renderDetails, type RunDetail, type StatsDetail, type ToolDetails } from "./renderers.ts";
import { type BusMessage, isRunId, type Provider, type RunRow, type SparEvent } from "./spar.ts";

export const UNATTENDED_FLAG = "spar-unattended-launch";
export const UNATTENDED_ENV = "SPAR_PI_UNATTENDED_LAUNCH";

const SLOT = /^[A-Za-z0-9][A-Za-z0-9_.:-]{0,127}$/;
const LIST_LIMIT = 50;
const OBSERVE_TIMEOUT_MS = 60_000;
const LAUNCH_TIMEOUT_MS = 5 * 60_000;

type Result = AgentToolResult<ToolDetails | undefined>;

function result(details: ToolDetails, isError = false): Result {
	return { content: [{ type: "text", text: modelText(details) }], details, ...(isError ? { isError: true } : {}) };
}

function error(message: string, exit_code?: number | null): Result {
	return result({ kind: "error", message, exit_code }, true);
}

function failure(run: SparRun, what: string): string {
	if (run.missing) return "spar: the spar binary is not on PATH (or set SPAR_PI_BIN)";
	if (run.timedOut) return `spar ${what}: timed out`;
	const why = firstLine(run.stderr) || firstLine(run.stdout) || "no output";
	return `spar ${what} exited ${run.code}: ${why}`;
}

export function exitMeaning(code: number | null, phase: string | null): string {
	switch (code) {
		case 0:
			return phase === "done" ? "finished" : `ok (phase ${phase ?? "unknown"})`;
		case 1:
			return "failed, or stopped by the operator";
		case 2:
			return `waiting on the human at a gate (${phase ?? "gate"}). Gates are the operator's to clear: tell them and give them the next command; do not try to clear it yourself`;
		case 3:
			return "stuck or escalated: the run cannot progress on its own. Read spar_brief and report to the operator";
		case 4:
			return "no usable providers (quota or pause); the run is parked. Check spar_providers";
		default:
			return `unexpected exit code ${code}`;
	}
}

function badId(name: string, value: string): Result {
	return error(`"${value}" is not a valid ${name}`);
}

async function observeJson<T>(args: string[], ctx: ExtensionContext, signal: AbortSignal | undefined): Promise<T | Result> {
	const run = await runSpar(args, ctx.cwd, { signal, timeoutMs: OBSERVE_TIMEOUT_MS, maxBytes: 256 * 1024 * 1024 });
	if (run.aborted) throw new Error("cancelled");
	if (run.code !== 0) return error(failure(run, args[0]!), run.code);
	const value = parseJson<T>(run.stdout);
	if (value === undefined) return error(`spar ${args[0]}: unparseable JSON`);
	return value;
}

function isResult(value: unknown): value is Result {
	return typeof value === "object" && value !== null && "content" in value && "details" in value;
}

function nonEmpty(list: string[] | undefined): string[] {
	return (list ?? []).map((s) => s.trim()).filter(Boolean);
}

const FleetParams = {
	providers: Type.Optional(
		Type.Array(Type.String(), {
			description: "Provider pool, positional: index 0 is the planner/implementer, the rest reviewers. e.g. [\"cli:claude\", \"cli:codex@gpt-5.6-terra\"]",
		}),
	),
	roles: Type.Optional(Type.Array(Type.String(), { description: "Per-role pins ROLE=PROVIDER[@MODEL], e.g. \"implementer=cli:muse\"; repeat reviewer= to set the panel exactly" })),
	backups: Type.Optional(Type.Array(Type.String(), { description: "Per-role backups ROLE=PROVIDER[@MODEL], used only on quota or provider outage" })),
	effort: Type.Optional(Type.Array(Type.String(), { description: "Per-role reasoning effort ROLE=LEVEL, e.g. \"planner=max\"" })),
	fleet: Type.Optional(StringEnum(["small", "standard"] as const, { description: "Fleet preset for this run" })),
	without: Type.Optional(Type.Array(StringEnum(["critic", "spec", "suite"] as const), { description: "Seats to drop for this run" })),
	base: Type.Optional(Type.String({ description: "Ref to cut worktrees from (branch, tag or sha)" })),
	dry_run: Type.Optional(Type.Boolean({ description: "Stub agents: writes .spar/ state, spawns and bills nothing" })),
};

const PlanParams = Type.Object({ task: Type.String({ description: "What to build, as a full brief" }), ...FleetParams });
const ImplementParams = Type.Object({
	task: Type.Optional(Type.String({ description: "A fresh task (new run), or with run: this round's amendment" })),
	run: Type.Optional(Type.String({ description: "Continue this run (e.g. an approved plan) instead of starting a new one" })),
	...FleetParams,
});
const ReviewParams = Type.Object({ task: Type.String({ description: "What to review, e.g. \"Review PR #12 for auth bugs\"" }), ...FleetParams });

type FleetInput = Static<typeof PlanParams> | Static<typeof ImplementParams>;

function fleetArgs(p: FleetInput): string[] {
	const args: string[] = [];
	const providers = nonEmpty(p.providers);
	if (providers.length) args.push(`--providers=${providers.join(",")}`);
	for (const r of nonEmpty(p.roles)) args.push(`--role=${r}`);
	for (const b of nonEmpty(p.backups)) args.push(`--backup=${b}`);
	for (const e of nonEmpty(p.effort)) args.push(`--effort=${e}`);
	if (p.fleet) args.push(`--fleet=${p.fleet}`);
	if (p.without?.length) args.push(`--without=${p.without.join(",")}`);
	if (p.base?.trim()) args.push(`--base=${p.base.trim()}`);
	if (p.dry_run) args.push("--dry-run");
	return args;
}

export function planArgs(p: Static<typeof PlanParams>): string[] {
	return ["plan", `--task=${p.task}`, ...fleetArgs(p), "--detach", "--json"];
}

export function implementArgs(p: Static<typeof ImplementParams>): string[] {
	const args = ["implement"];
	if (p.run) args.push(`--run=${p.run}`);
	if (p.task !== undefined) args.push(`--task=${p.task}`);
	return [...args, ...fleetArgs(p), "--detach", "--json"];
}

export function reviewArgs(p: Static<typeof ReviewParams>): string[] {
	return ["run", "--workflow=review", `--task=${p.task}`, ...fleetArgs(p), "--detach", "--json"];
}

function visible(text: string): string {
	return text.replace(/[\x00-\x08\x0b-\x1f\x7f-\x9f]/g, (c) => `\\x${c.charCodeAt(0).toString(16).padStart(2, "0")}`);
}

function quote(value: string): string {
	return /^[\w@:.,=/+-]+$/.test(value) ? value : `'${visible(value).replace(/'/g, "'\\''")}'`;
}

export function display(args: string[]): string {
	return `spar ${args
		.map((a) => {
			const flag = /^(--[\w-]+=)([\s\S]*)$/.exec(a);
			return flag ? `${flag[1]}${quote(flag[2]!)}` : quote(a);
		})
		.join(" ")}`;
}

export interface Gates {
	autonomy?: string;
	auto_plan?: boolean;
	auto_winner?: boolean;
	auto_ship?: boolean;
}

interface Validation {
	ok?: boolean;
	error?: string;
	fleet?: FleetSeat[] | null;
	gates?: Gates;
}

type LaunchKind = "plan" | "implement" | "review";

// Which gates the run would pass on its own, among the ones this kind of launch reaches.
export function gatesOff(kind: LaunchKind, gates: Gates | undefined): string[] {
	if (!gates) return [];
	const off: string[] = [];
	if (kind === "plan" && gates.auto_plan) off.push("plan approval");
	if (kind !== "review" && gates.auto_ship) off.push("ship (pushes a branch and opens a draft PR)");
	return off;
}

function gateLine(kind: LaunchKind, gates: Gates | undefined): string {
	const off = gatesOff(kind, gates);
	if (off.length) {
		return `WARNING: this project's config passes these gates WITHOUT you: ${off.join("; ")} (autonomy ${gates?.autonomy ?? "?"}). Once launched, the run continues past them on its own.`;
	}
	if (kind === "review") return "Gates: a review ends when the reviewers finish.";
	return `Gates: ${kind === "plan" ? "plan approval and ship wait" : "ship waits"} for you (autonomy ${gates?.autonomy ?? "unknown"}).`;
}

const TASK_PREVIEW = 6;

function confirmBody(kind: LaunchKind, p: FleetInput, run: RunDetail | undefined, v: Validation, args: string[]): string {
	const lines: string[] = [gateLine(kind, v.gates), ""];
	lines.push(p.dry_run ? "Dry run: no agents spawn and nothing is billed." : "This dispatches paid agents and spends money.");
	if ("run" in p && p.run) lines.push(`continues run ${p.run}${run ? ` (${run.workflow}, phase ${run.phase}, round ${run.round})` : ""}`);
	if (p.task?.trim()) {
		const task = visible(p.task.trim()).split("\n");
		lines.push(`task: ${task.slice(0, TASK_PREVIEW).join("\n      ")}${task.length > TASK_PREVIEW ? `\n      (+${task.length - TASK_PREVIEW} more lines: choose View full task)` : ""}`);
	}
	const fleet = v.fleet ?? [];
	if (fleet.length) {
		lines.push("seats:");
		for (const s of fleet) {
			lines.push(`  ${s.role} ${s.model && !s.provider.includes("@") ? `${s.provider}@${s.model}` : s.provider}${s.projected ? " (later phase)" : ""}  [${s.seat}, ${s.source ?? "?"}]`);
		}
		lines.push("  (before availability and quota checks; a paused provider may be swapped for its backup)");
	} else {
		lines.push("seats: spar cannot name them before launch (--select ranks at launch)");
	}
	lines.push("", "command:", display(args));
	return lines.join("\n");
}

async function confirmLaunch(ctx: ExtensionContext, title: string, body: string, task: string | undefined, signal: AbortSignal | undefined): Promise<boolean> {
	const options = ["Cancel", "Launch", ...(task?.trim() ? ["View full task"] : [])];
	for (;;) {
		const choice = await ctx.ui.select(`${title}\n\n${body}`, options, { signal });
		if (choice === "Launch") return true;
		if (choice !== "View full task") return false;
		await ctx.ui.select(`Full task\n\n${visible(task!)}`, ["Back"], { signal });
	}
}

interface RunJson {
	id?: string;
	run_id?: string;
	phase?: string;
	error?: string | null;
	dry_run?: boolean;
	exit_code?: number;
	fleet?: FleetSeat[] | null;
	ok?: boolean;
}

async function nextCommand(id: string, ctx: ExtensionContext, signal: AbortSignal | undefined): Promise<string | undefined> {
	const brief = await runSpar(["brief", id, "--json"], ctx.cwd, { signal, timeoutMs: OBSERVE_TIMEOUT_MS });
	if (brief.code !== 0) return undefined;
	return parseJson<BriefDetail>(brief.stdout)?.next_command;
}

async function outcomeOf(run: SparRun, json: RunJson, ctx: ExtensionContext, signal: AbortSignal | undefined): Promise<Outcome> {
	const id = json.id ?? json.run_id ?? null;
	const phase = json.phase ?? null;
	return {
		run_id: id,
		phase,
		exit_code: run.code,
		meaning: exitMeaning(run.code, phase),
		error: json.error,
		dry_run: json.dry_run,
		fleet: json.fleet ?? undefined,
		next_command: id ? await nextCommand(id, ctx, signal) : undefined,
	};
}

export function unattendedAllowed(pi: ExtensionAPI): boolean {
	return pi.getFlag(UNATTENDED_FLAG) === true || process.env[UNATTENDED_ENV] === "1";
}

export function launchKey(toolCallId: string): string {
	return `pi-${toolCallId.replace(/[^A-Za-z0-9_-]/g, "-").slice(0, 80)}`;
}

const OPERATOR_PATH = "The operator clears gates from /spar (the gate dialogs) or with the command spar_brief names.";

// A run waiting on the operator, stopped or abandoned is theirs to move: `implement --run`
// on it would re-dispatch past their decision.
function blockedRun(run: RunDetail): string | undefined {
	if (run.phase.startsWith("awaiting_")) return `run ${run.id} is waiting on the operator at ${run.phase}. ${OPERATOR_PATH}`;
	if (run.phase === "stopped") return `run ${run.id} was stopped; resuming it is the operator's call. ${OPERATOR_PATH}`;
	if (run.abandoned) return `run ${run.id} is abandoned (no orchestrator); resuming it is the operator's call. ${OPERATOR_PATH}`;
	if (run.holds_seat) return `run ${run.id} is already running; use spar_wait or spar_status`;
	return undefined;
}

const MAY_HAVE_LAUNCHED = "it may have launched. Check spar_status before trying again";

async function launch(
	pi: ExtensionAPI,
	kind: LaunchKind,
	label: string,
	launchArgs: string[],
	p: FleetInput,
	ctx: ExtensionContext,
	signal: AbortSignal | undefined,
	toolCallId: string,
): Promise<Result> {
	const existing = "run" in p && p.run ? p.run : undefined;
	const args = existing ? launchArgs : [...launchArgs.slice(0, -2), `--key=${launchKey(toolCallId)}`, "--key-scope=local", ...launchArgs.slice(-2)];
	const command = display(args);
	if (args.some(hasControls)) {
		return result({ kind: "launch", command, refused: "the task or a flag contains control characters (escape sequences, carriage returns); remove them" }, true);
	}
	let run: RunDetail | undefined;
	if (existing) {
		const status = await observeJson<RunDetail>(["status", existing, "--json"], ctx, signal);
		if (isResult(status)) return status;
		const blocked = blockedRun(status);
		if (blocked) return result({ kind: "launch", command, refused: blocked }, true);
		run = status;
	}
	const check = await runSpar([...args, "--validate-only"], ctx.cwd, { signal, timeoutMs: OBSERVE_TIMEOUT_MS });
	if (check.aborted) throw new Error("cancelled");
	const validation = parseJson<Validation>(check.stdout);
	if (check.code !== 0 || !validation?.ok) {
		return result({ kind: "launch", command, refused: validation?.error ?? failure(check, args[0]!) }, true);
	}
	if (!unattendedAllowed(pi)) {
		if (!ctx.hasUI) {
			return result(
				{
					kind: "launch",
					command,
					declined: `no operator UI to confirm a launch, and launches spend money. Ask the operator to run it, or start pi with --${UNATTENDED_FLAG} (or ${UNATTENDED_ENV}=1)`,
				},
				true,
			);
		}
		const ok = await confirmLaunch(ctx, `Launch ${label}?`, confirmBody(kind, p, run, validation, args), p.task, signal);
		if (signal?.aborted) throw new Error("cancelled");
		if (!ok) return result({ kind: "launch", command, declined: "the operator declined the launch. Ask them what to change before trying again" }, true);
	}
	const launched = await runSpar(args, ctx.cwd, { signal, timeoutMs: LAUNCH_TIMEOUT_MS });
	if (launched.aborted) throw new Error(`cancelled after the launch started: ${MAY_HAVE_LAUNCHED}`);
	if (launched.timedOut) return error(`spar ${args[0]} did not return within ${LAUNCH_TIMEOUT_MS / 60_000}m: ${MAY_HAVE_LAUNCHED}`);
	const json = parseJson<RunJson>(launched.stdout);
	if (!json || json.ok === false) {
		return result({ kind: "launch", command, refused: (json as { error?: string } | undefined)?.error ?? failure(launched, args[0]!) }, true);
	}
	const outcome = await outcomeOf(launched, json, ctx, signal);
	outcome.gates_off = gatesOff(kind, validation.gates);
	return result({ kind: "launch", command, outcome });
}

function tailLines(text: string, n: number): { lines: string[]; total: number } {
	const all = text.replace(/\n$/, "").split("\n");
	const lines = all.slice(-n).map((l) => (l.length > 400 ? `${l.slice(0, 397)}...` : l));
	return { lines, total: text ? all.length : 0 };
}

function section(doc: string, wanted: string | undefined): string {
	const lines = doc.split("\n");
	const headings = lines.map((l, i) => ({ i, m: /^(#{2,3}) (.+)$/.exec(l) })).filter((h) => h.m);
	if (!wanted?.trim()) {
		const intro = lines.slice(0, headings[0]?.i ?? lines.length).join("\n").trim();
		const toc = headings.map((h) => `${h.m![1] === "###" ? "  " : ""}- ${h.m![2]}`).join("\n");
		return `${intro}\n\nSections (call spar_skill with section set to one of these):\n${toc}`;
	}
	const needle = wanted.trim().toLowerCase();
	const hit = headings.find((h) => h.m![2]!.toLowerCase() === needle) ?? headings.find((h) => h.m![2]!.toLowerCase().includes(needle));
	if (!hit) return `No section matches "${wanted}". Call spar_skill without a section for the list.`;
	const level = hit.m![1]!.length;
	const end = headings.find((h) => h.i > hit.i && h.m![1]!.length <= level)?.i ?? lines.length;
	const body = lines.slice(hit.i, end).join("\n").trim();
	return body.length > 24_000 ? `${body.slice(0, 24_000)}\n... (section truncated)` : body;
}

function callText(theme: Theme, text: string): Text {
	return new Text(theme.fg("toolTitle", theme.bold("spar ")) + theme.fg("accent", text), 0, 0);
}

type Def<P extends TSchema> = ToolDefinition<P, ToolDetails | undefined>;

function tool<P extends TSchema>(
	def: Omit<Def<P>, "renderCall" | "renderResult" | "label"> & { call: (args: Static<P>) => string },
): Def<P> {
	const { call, ...rest } = def;
	return {
		...rest,
		label: `spar ${def.name.replace(/^spar_/, "").replace(/_/g, " ")}`,
		renderCall: (args, theme) => callText(theme, call(args)),
		renderResult: (res, { expanded, isPartial }, theme) => {
			if (isPartial) return new Text(theme.fg("warning", "running..."), 0, 0);
			const text = res.content[0]?.type === "text" ? res.content[0].text : "";
			return renderDetails(res.details, expanded, theme, text);
		},
	};
}

export function sparTools(pi: ExtensionAPI): Def<TSchema>[] {
	const defs = [
		tool({
			name: "spar_status",
			description:
				"List spar runs in this project (or every registered project with all=true), or show one run's phase, seats and cost. Read-only.",
			promptSnippet: "List spar runs or show one run's phase, seats and cost",
			parameters: Type.Object({
				run: Type.Optional(Type.String({ description: "Run id; omit to list runs" })),
				all: Type.Optional(Type.Boolean({ description: "List runs across every registered project" })),
			}),
			annotations: { readOnlyHint: true },
			call: (a) => (a.run ? `status ${a.run}` : a.all ? "status --all" : "status"),
			async execute(_id, p, signal, _u, ctx) {
				if (p.run) {
					if (!isRunId(p.run)) return badId("run id", p.run);
					const run = await observeJson<RunDetail>(["status", p.run, "--json"], ctx, signal);
					return isResult(run) ? run : result({ kind: "run", run });
				}
				const runs = await observeJson<RunRow[]>(["status", "--json", ...(p.all ? ["--all"] : [])], ctx, signal);
				if (isResult(runs)) return runs;
				const sorted = sortRuns(runs, new Date());
				const shown = sorted.slice(0, LIST_LIMIT);
				const res = result({ kind: "runs", runs: shown });
				if (sorted.length > shown.length) {
					res.content[0] = { type: "text", text: `${modelText({ kind: "runs", runs: shown })}\n(${sorted.length - shown.length} older runs not shown)` };
				}
				return res;
			},
		}),
		tool({
			name: "spar_brief",
			description:
				"Re-hydrate on one spar run: phase, fleet, open review findings by severity, recent events and the exact next command. Read-only. Use before acting on a run.",
			promptSnippet: "One spar run's findings, recent events and exact next command",
			parameters: Type.Object({ run: Type.String({ description: "Run id" }) }),
			annotations: { readOnlyHint: true },
			call: (a) => `brief ${a.run}`,
			async execute(_id, p, signal, _u, ctx) {
				if (!isRunId(p.run)) return badId("run id", p.run);
				const brief = await observeJson<BriefDetail>(["brief", p.run, "--json"], ctx, signal);
				return isResult(brief) ? brief : result({ kind: "brief", brief });
			},
		}),
		tool({
			name: "spar_events",
			description: "The lifecycle event backlog of one spar run (phase changes, gates, slot status), newest last. Read-only.",
			promptSnippet: "A spar run's lifecycle event backlog",
			parameters: Type.Object({
				run: Type.String({ description: "Run id" }),
				limit: Type.Optional(Type.Integer({ minimum: 1, maximum: 200, description: "How many of the latest events (default 30)" })),
			}),
			annotations: { readOnlyHint: true },
			call: (a) => `events --run ${a.run}`,
			async execute(_id, p, signal, _u, ctx) {
				if (!isRunId(p.run)) return badId("run id", p.run);
				const run = await runSpar(["events", `--run=${p.run}`, "--json"], ctx.cwd, { signal, timeoutMs: OBSERVE_TIMEOUT_MS, maxBytes: 64 * 1024 * 1024 });
				if (run.aborted) throw new Error("cancelled");
				if (run.code !== 0) return error(failure(run, "events"), run.code);
				const events = jsonLines<SparEvent>(run.stdout);
				const limit = Math.min(200, Math.max(1, p.limit ?? 30));
				return result({ kind: "events", run: p.run, events: events.slice(-limit), total: events.length });
			},
		}),
		tool({
			name: "spar_logs",
			description: "The tail of a spar run's log, or one slot's log (e.g. slot \"impl\"). Bounded; read-only.",
			promptSnippet: "Tail of a spar run's or slot's log",
			parameters: Type.Object({
				run: Type.String({ description: "Run id" }),
				slot: Type.Optional(Type.String({ description: "Slot id, e.g. impl or review-0-cli-claude" })),
				lines: Type.Optional(Type.Integer({ minimum: 1, maximum: 400, description: "How many trailing lines (default 80)" })),
			}),
			annotations: { readOnlyHint: true },
			call: (a) => `logs ${a.run}${a.slot ? ` ${a.slot}` : ""}`,
			async execute(_id, p, signal, _u, ctx) {
				if (!isRunId(p.run)) return badId("run id", p.run);
				if (p.slot && !SLOT.test(p.slot)) return badId("slot id", p.slot);
				const run = await runSpar(["logs", p.run, ...(p.slot ? [p.slot] : [])], ctx.cwd, { signal, timeoutMs: OBSERVE_TIMEOUT_MS, maxBytes: 4 * 1024 * 1024 });
				if (run.aborted) throw new Error("cancelled");
				if (run.code !== 0) return error(failure(run, "logs"), run.code);
				const { lines, total } = tailLines(clean(run.stdout), Math.min(400, Math.max(1, p.lines ?? 80)));
				return result({ kind: "logs", run: p.run, slot: p.slot, lines, total: run.truncated ? Math.max(total, lines.length) : total });
			},
		}),
		tool({
			name: "spar_providers",
			description: "spar's provider inventory with live quota and pause state. Read-only; probes each installed CLI, so it takes a few seconds.",
			promptSnippet: "spar providers and their quota or pause state",
			parameters: Type.Object({ usage: Type.Optional(Type.Boolean({ description: "Also read each account's current utilisation where available" })) }),
			annotations: { readOnlyHint: true },
			call: (a) => `provider list${a.usage ? " --usage" : ""}`,
			async execute(_id, p, signal, _u, ctx) {
				const providers = await observeJson<Provider[]>(["provider", "list", "--json", ...(p.usage ? ["--usage"] : [])], ctx, signal);
				return isResult(providers) ? providers : result({ kind: "providers", providers });
			},
		}),
		tool({
			name: "spar_stats",
			description: "Aggregate spar run history: runs, outcomes and cost, grouped by provider, model, role or project. Read-only.",
			promptSnippet: "spar run history: outcomes and cost by provider, model, role or project",
			parameters: Type.Object({
				since: Type.Optional(Type.String({ pattern: "^[0-9]+[dhm]$", description: "Window like 7d, 12h, 30m" })),
				by: Type.Optional(StringEnum(["provider", "model", "role", "project"] as const)),
			}),
			annotations: { readOnlyHint: true },
			call: (a) => `stats${a.since ? ` --since ${a.since}` : ""}${a.by ? ` --by ${a.by}` : ""}`,
			async execute(_id, p, signal, _u, ctx) {
				const args = ["stats", "--json"];
				if (p.since) args.push(`--since=${p.since}`);
				if (p.by) args.push(`--by=${p.by}`);
				const stats = await observeJson<StatsDetail>(args, ctx, signal);
				return isResult(stats) ? stats : result({ kind: "stats", stats });
			},
		}),
		tool({
			name: "spar_plan",
			description:
				"Launch a spar plan run (planner, critic, test contract), detached. It stops at the plan gate for the operator unless the project's config (autonomy, [gates]) approves plans itself; the result and the confirm dialog say which. Spends money: the operator confirms every launch.",
			promptSnippet: "Launch a spar plan run (operator confirms; spends money)",
			promptGuidelines: [
				"spar_plan, spar_implement and spar_review spend money. Name the fleet (providers or roles) you intend, and expect the operator to confirm or decline. A declined launch is final for that request: ask, do not retry.",
			],
			parameters: PlanParams,
			executionMode: "sequential",
			call: (a) => `plan ${JSON.stringify(firstLine(a.task).slice(0, 60))}`,
			async execute(id, p, signal, _u, ctx) {
				if (!p.task.trim()) return error("task is empty");
				return launch(pi, "plan", "spar plan", planArgs(p), p, ctx, signal, id);
			},
		}),
		tool({
			name: "spar_implement",
			description:
				"Launch spar's build and review loop, detached: either a fresh task (no planner) or continue a run, e.g. an approved plan, with run. It stops at the ship gate for the operator unless the project's config ships on its own; the result says which. A run waiting at a gate, stopped or abandoned is refused: that is the operator's call. Spends money: the operator confirms every launch.",
			promptSnippet: "Launch spar's implement loop for a task or an approved plan (operator confirms; spends money)",
			parameters: ImplementParams,
			executionMode: "sequential",
			call: (a) => (a.run ? `implement --run ${a.run}` : `implement ${JSON.stringify(firstLine(a.task ?? "").slice(0, 60))}`),
			async execute(id, p, signal, _u, ctx) {
				if (p.run && !isRunId(p.run)) return badId("run id", p.run);
				if (!p.run && !p.task?.trim()) return error("pass task for a new run, or run to continue one");
				return launch(pi, "implement", p.run ? `spar implement --run ${p.run}` : "spar implement", implementArgs(p), p, ctx, signal, id);
			},
		}),
		tool({
			name: "spar_review",
			description:
				"Launch an independent multi-provider review (spar run --workflow review), detached: N reviewers in parallel, no implementer. Spends money: the operator confirms every launch.",
			promptSnippet: "Launch a spar multi-provider review of existing code (operator confirms; spends money)",
			parameters: ReviewParams,
			executionMode: "sequential",
			call: (a) => `review ${JSON.stringify(firstLine(a.task).slice(0, 60))}`,
			async execute(id, p, signal, _u, ctx) {
				if (!p.task.trim()) return error("task is empty");
				return launch(pi, "review", "spar review", reviewArgs(p), p, ctx, signal, id);
			},
		}),
		tool({
			name: "spar_wait",
			description:
				"Block until a spar run reaches a gate or a terminal phase, for at most timeout_minutes (default 10, max 120). A timeout means the run is still working, not that it failed. Cancellable.",
			promptSnippet: "Wait (bounded) for a spar run to reach a gate or finish",
			parameters: Type.Object({
				run: Type.String({ description: "Run id" }),
				timeout_minutes: Type.Optional(Type.Integer({ minimum: 1, maximum: 120, description: "Default 10" })),
			}),
			annotations: { readOnlyHint: true },
			call: (a) => `wait ${a.run} (${a.timeout_minutes ?? 10}m)`,
			async execute(_id, p, signal, onUpdate, ctx) {
				if (!isRunId(p.run)) return badId("run id", p.run);
				const minutes = Math.min(120, Math.max(1, p.timeout_minutes ?? 10));
				onUpdate?.({ content: [{ type: "text", text: `waiting up to ${minutes}m for run ${p.run}` }], details: undefined });
				const run = await runSpar(["wait", p.run, `--timeout=${minutes}m`, "--json"], ctx.cwd, {
					signal,
					timeoutMs: minutes * 60_000 + 30_000,
				});
				if (run.aborted) throw new Error("spar wait cancelled");
				if (run.timedOut) return error(`spar wait did not return ${minutes}m after its own timeout should have fired: spar may be hung. Check spar_status`);
				const json = parseJson<RunJson>(run.stdout);
				if (run.code === 3 && json?.error === "wait timed out") {
					const outcome: Outcome = {
						run_id: p.run,
						phase: json?.phase ?? null,
						exit_code: run.code,
						meaning: `still working after ${minutes}m (no gate yet). Nothing is wrong: wait again or check back later`,
					};
					return result({ kind: "wait", outcome, waitedTimeout: true });
				}
				if (!json) return error(failure(run, "wait"), run.code);
				return result({ kind: "wait", outcome: await outcomeOf(run, json, ctx, signal), waitedTimeout: false });
			},
		}),
		tool({
			name: "spar_bus_send",
			description:
				"Send a message on spar's swarm bus, e.g. guidance to a run's implementer (to=\"impl\" with run). Delivered at the agent's next turn boundary.",
			promptSnippet: "Message a spar run's agents over the bus",
			parameters: Type.Object({
				message: Type.String(),
				run: Type.Optional(Type.String({ description: "Run id to scope the message to; required for to=impl" })),
				to: Type.Optional(Type.String({ description: "Agent id, impl, broadcast (default) or @human" })),
				requires_ack: Type.Optional(Type.Boolean({ description: "Redeliver until acked, then escalate to the operator" })),
			}),
			call: (a) => `bus send ${a.to ?? "broadcast"}${a.run ? ` --run ${a.run}` : ""}`,
			async execute(_id, p, signal, _u, ctx) {
				if (!p.message.trim()) return error("message is empty");
				if (p.run && !isRunId(p.run)) return badId("run id", p.run);
				const to = p.to?.trim() || "broadcast";
				const args = ["bus", "send", `--message=${p.message}`, "--from=pi", `--to=${to}`, "--json"];
				if (p.run) args.push(`--run=${p.run}`);
				if (p.requires_ack) args.push("--requires-ack");
				const sent = await observeJson<{ id?: string }>(args, ctx, signal);
				return isResult(sent) ? sent : result({ kind: "bus_sent", id: sent.id, to, run: p.run });
			},
		}),
		tool({
			name: "spar_bus_log",
			description: "Recent spar bus messages, for one run or the whole project. Read-only.",
			promptSnippet: "Recent spar bus messages",
			parameters: Type.Object({
				run: Type.Optional(Type.String({ description: "Run id; omit for every run" })),
				limit: Type.Optional(Type.Integer({ minimum: 1, maximum: 200, description: "How many of the latest (default 20)" })),
			}),
			annotations: { readOnlyHint: true },
			call: (a) => `bus log${a.run ? ` --run ${a.run}` : ""}`,
			async execute(_id, p, signal, _u, ctx) {
				if (p.run && !isRunId(p.run)) return badId("run id", p.run);
				const messages = await observeJson<BusMessage[]>(["bus", "log", "--json", ...(p.run ? [`--run=${p.run}`] : [])], ctx, signal);
				if (isResult(messages)) return messages;
				const limit = Math.min(200, Math.max(1, p.limit ?? 20));
				return result({ kind: "bus", messages: messages.slice(-limit), total: messages.length });
			},
		}),
		tool({
			name: "spar_skill",
			description:
				"spar's own agent manual (spar skills get core), always matching the installed binary. Without section: the overview and the section list. With section: that section.",
			promptSnippet: "Read spar's agent manual, by section",
			parameters: Type.Object({ section: Type.Optional(Type.String({ description: "Section heading, e.g. \"Exit codes\" or \"Swarm bus\"" })) }),
			annotations: { readOnlyHint: true },
			call: (a) => `skills get core${a.section ? ` (${a.section})` : ""}`,
			async execute(_id, p, signal, _u, ctx) {
				const run = await runSpar(["skills", "get", "core"], ctx.cwd, { signal, timeoutMs: OBSERVE_TIMEOUT_MS });
				if (run.aborted) throw new Error("cancelled");
				if (run.code !== 0) return error(failure(run, "skills"), run.code);
				return result({ kind: "skill", section: p.section, text: section(clean(run.stdout), p.section) });
			},
		}),
	];
	return defs as unknown as Def<TSchema>[];
}

export function registerTools(pi: ExtensionAPI): void {
	pi.registerFlag(UNATTENDED_FLAG, {
		type: "boolean",
		default: false,
		description: "Let the model launch spar runs (plan, implement, review) without the operator confirming each one",
	});
	for (const def of sparTools(pi)) pi.registerTool(def);
	pi.on("tool_call", async (event, ctx) => {
		if (!isToolCallEventType("bash", event)) return undefined;
		return guardBash(pi, event.input.command, ctx);
	});
}

// The spar_* tools are not the only way to reach spar: pi's bash runs it too. Best effort,
// see guard.ts: a command that hides spar behind a variable, alias or script gets through.
export async function guardBash(pi: ExtensionAPI, command: string, ctx: ExtensionContext): Promise<{ block: true; reason: string } | undefined> {
	const verdict = judgeBash(command);
	if (verdict.kind === "none") return undefined;
	if (verdict.kind === "launch" && unattendedAllowed(pi)) return undefined;
	const what =
		verdict.kind === "operator"
			? "an operator action (a gate, ship, stop, resume, cleanup, ruling or similar)"
			: "a launch that spends money (prefer spar_plan, spar_implement or spar_review, which validate and show the fleet)";
	if (!ctx.hasUI) {
		return { block: true, reason: `blocked: this bash command runs spar ${what}, and there is no operator UI to confirm it. ${OPERATOR_PATH}` };
	}
	const shown = verdict.commands.map((c) => display(c)).join("\n");
	const choice = await ctx.ui.select(
		`The model wants to run spar through bash: ${what}.\n\nspar commands found:\n${shown}\n\nfull bash command:\n${visible(command)}`,
		["Cancel", "Run it"],
		{ signal: ctx.signal },
	);
	if (choice === "Run it") return undefined;
	return { block: true, reason: `the operator declined this spar command. ${verdict.kind === "operator" ? OPERATOR_PATH : "Ask them before launching."}` };
}
