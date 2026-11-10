import { execFile, spawn } from "node:child_process";
import { closeSync, mkdtempSync, openSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { ExtensionContext } from "@earendil-works/pi-coding-agent";
import type { AutocompleteItem, KeyId } from "@earendil-works/pi-tui";
import { isRunId, type RunRow, type RunStatus, sparBin, sparJson } from "./spar.ts";

export type Verb =
	| "approve"
	| "reject"
	| "ship"
	| "override"
	| "confirm"
	| "reconcile"
	| "extend"
	| "resume"
	| "stop"
	| "rule"
	| "reassign"
	| "cleanup";

export const VERBS: Record<Verb, { key: KeyId; label: string; summary: string }> = {
	approve: { key: "a", label: "approve", summary: "approve the plan" },
	reject: { key: "x", label: "reject", summary: "reject the plan, with an optional reason" },
	ship: { key: "s", label: "ship", summary: "confirm the ship gate: push and open a draft PR" },
	override: { key: "o", label: "override+ship", summary: "ship over a gate the review never cleared, with a reason" },
	confirm: { key: "c", label: "confirm winner", summary: "confirm the arena winner (ranked default or a pick)" },
	reconcile: { key: "m", label: "reconcile", summary: "merge the arena's good parts, then review" },
	extend: { key: "e", label: "extend", summary: "buy N more rounds and keep going" },
	resume: { key: "r", label: "resume", summary: "pick the run back up, detached" },
	stop: { key: "k", label: "stop", summary: "halt the run, keeping branch and worktrees" },
	rule: { key: "u", label: "rule", summary: "record an operator ruling on the run" },
	reassign: { key: "n", label: "reassign", summary: "move a seat to another provider, then continue" },
	cleanup: { key: "d", label: "cleanup", summary: "remove worktrees if the work is merged" },
};

const ORDER = Object.keys(VERBS) as Verb[];

export function isVerb(value: string | undefined): value is Verb {
	return value !== undefined && Object.hasOwn(VERBS, value);
}

const GATES = new Set([
	"awaiting_plan_approval",
	"awaiting_winner_confirm",
	"awaiting_reconcile",
	"awaiting_ship_confirm",
	"awaiting_round_extension",
]);
const AT_REST = new Set([...GATES, "done", "plan_rejected", "plan_approved", "stopped", "failed", "stuck", "quota", "escalated"]);
// Where a plain `spar ship --confirm` is refused and `ship::overridable` allows the
// override. Not the ship gate (a plain ship works) nor winner confirm (confirm works).
const OVERRIDE_PHASES = new Set(["awaiting_reconcile", "awaiting_round_extension", "stuck", "failed", "quota"]);
// Where `implement --run --reassign` is accepted: a parked, resumable run. Not the plan
// gate, which spar refuses.
const REASSIGN_PHASES = new Set(["stopped", "failed", "stuck", "quota", "plan_approved"]);

// Runs with a background job of ours (ship, override, reconcile, cleanup) still going.
const inFlight = new Set<string>();

export function busy(id: string): boolean {
	return inFlight.has(id);
}

// What the operator can do from here. A hint for the UI only: spar is the authority and
// refuses anything this gets wrong, and that refusal is shown. Gate verbs need a run
// nobody is driving; a run holding a seat only offers stop and rule.
export function actionsFor(run: RunRow): Verb[] {
	const verbs = new Set<Verb>();
	const free = !run.holds_seat && !inFlight.has(run.id);
	if (free) {
		switch (run.phase) {
			case "awaiting_plan_approval":
				verbs.add("approve").add("reject");
				break;
			case "awaiting_ship_confirm":
				verbs.add("ship");
				break;
			case "awaiting_winner_confirm":
			case "awaiting_reconcile":
				verbs.add("confirm").add("reconcile");
				break;
			case "awaiting_round_extension":
				verbs.add("extend");
				break;
		}
		if (OVERRIDE_PHASES.has(run.phase)) verbs.add("override");
		if (run.resumable && !GATES.has(run.phase)) verbs.add("resume");
		if (run.resumable && REASSIGN_PHASES.has(run.phase)) verbs.add("reassign");
		if (run.phase === "done") verbs.add("cleanup");
	}
	// Shipping is a push in progress: stopping it half-way is not an operator action spar
	// offers. A running reconcile holds its run at awaiting_reconcile, which stop reaps.
	const inFlightPhase = !AT_REST.has(run.phase) && run.phase !== "shipping";
	if (inFlightPhase || (run.phase === "awaiting_reconcile" && run.holds_seat)) verbs.add("stop");
	verbs.add("rule");
	return ORDER.filter((v) => verbs.has(v));
}

export interface ActionInput {
	reason?: string;
	winner?: string;
	rounds?: number;
	text?: string;
	waive?: string[];
	seat?: string;
	provider?: string;
}

// At the extension gate a run below its ceiling (or with none) is a no-progress
// stall, not an exhausted budget: re-entry needs no flag, and the flag would
// resolve under the ceiling it never reached.
export function isStallPark(run: { round: number; max_rounds?: number }): boolean {
	return (run.max_rounds ?? 0) === 0 || run.round < (run.max_rounds ?? 0);
}

export function buildArgv(verb: Verb, id: string, input: ActionInput = {}, run?: { round: number; max_rounds?: number }): string[] {
	switch (verb) {
		case "approve":
			return ["approve", id, "--json"];
		case "reject":
			return input.reason ? ["reject", id, `--reason=${input.reason}`, "--json"] : ["reject", id, "--json"];
		case "ship":
			return ["ship", id, "--confirm", "--json"];
		case "override":
			return ["ship", id, "--override-gate", `--reason=${input.reason ?? ""}`, "--json"];
		case "confirm":
			return input.winner ? ["confirm", id, "--winner", input.winner, "--json"] : ["confirm", id, "--json"];
		case "reconcile":
			return ["reconcile", id, "--json"];
		case "extend":
			// One argv owner for both parks: a stall needs a bare re-entry, and
			// the flag would resolve under a ceiling the run never reached.
			if (run !== undefined && isStallPark(run)) return ["implement", "--run", id, "--detach", "--json"];
			return ["implement", "--run", id, "--max-rounds", `+${input.rounds ?? 1}`, "--detach", "--json"];
		case "resume":
			return ["resume", id, "--detach", "--json"];
		case "stop":
			return ["stop", id, "--json"];
		case "rule":
			return [
				"rule",
				"add",
				"--run",
				id,
				...(input.waive ?? []).flatMap((w) => ["--waive", w]),
				"--json",
				"--",
				input.text ?? "",
			];
		case "reassign":
			return ["implement", "--run", id, "--reassign", `${input.seat}=${input.provider}`, "--detach", "--json"];
		case "cleanup":
			return ["cleanup", id, "--if-merged", "--json"];
	}
}

// Ship pushes and calls gh, reconcile dispatches agents, cleanup may ask gh about merges:
// none of them should hold pi while they work, and none should die with it.
export const BACKGROUND = new Set<Verb>(["ship", "override", "reconcile", "cleanup"]);

// These print the run as it was when the detached orchestrator took over, not where it goes.
const DETACHED = new Set<Verb>(["extend", "resume", "reassign"]);

const TIMEOUT_MS: Partial<Record<Verb, number>> = { stop: 120_000, extend: 120_000, resume: 120_000, reassign: 120_000 };

export function displayCommand(args: string[]): string {
	const quote = (a: string) => (/^[A-Za-z0-9_@%+=:,./-]+$/.test(a) ? a : `'${a.replace(/'/g, `'\\''`)}'`);
	return ["spar", ...args].map(quote).join(" ");
}

export interface Raw {
	code: number | null;
	stdout: string;
	stderr: string;
	missing?: boolean;
	signal?: string | null;
}

export function runSpar(args: string[], cwd: string, timeoutMs = 60_000): Promise<Raw> {
	return new Promise((resolve) => {
		execFile(
			sparBin(),
			args,
			{ cwd, timeout: timeoutMs, maxBuffer: 64 * 1024 * 1024, windowsHide: true },
			(error, stdout, stderr) => {
				const err = error as (NodeJS.ErrnoException & { code?: number | string; killed?: boolean; signal?: string }) | null;
				if (err && err.code === "ENOENT") {
					resolve({ code: null, stdout: "", stderr: "", missing: true });
					return;
				}
				const code = err ? (typeof err.code === "number" ? err.code : null) : 0;
				const timedOut = err?.killed ? `\ntimed out after ${Math.round(timeoutMs / 1000)}s` : "";
				resolve({ code, stdout, stderr: `${stderr}${timedOut}`, signal: err?.signal ?? null });
			},
		);
	});
}

export interface Background {
	dir: string;
	done: Promise<Raw>;
}

// Detached into its own session with output in files, so pi exiting neither kills it nor
// leaves it writing to a closed pipe.
export function runBackground(args: string[], cwd: string): Background {
	const dir = mkdtempSync(join(tmpdir(), "spar-pi-"));
	const outPath = join(dir, "stdout");
	const errPath = join(dir, "stderr");
	const out = openSync(outPath, "w");
	const err = openSync(errPath, "w");
	const done = new Promise<Raw>((resolve) => {
		const child = spawn(sparBin(), args, { cwd, detached: true, stdio: ["ignore", out, err], windowsHide: true });
		closeSync(out);
		closeSync(err);
		child.unref();
		let settled = false;
		child.on("error", (error: NodeJS.ErrnoException) => {
			if (settled) return;
			settled = true;
			resolve({ code: null, stdout: "", stderr: error.message, missing: error.code === "ENOENT" });
		});
		child.on("exit", (code, signal) => {
			if (settled) return;
			settled = true;
			resolve({ code, signal, stdout: readFileSync(outPath, "utf8"), stderr: readFileSync(errPath, "utf8") });
		});
	});
	return { dir, done };
}

const EXIT_MEANING: Record<number, string> = { 0: "ok", 1: "failed", 2: "waiting on a human gate", 3: "stuck", 4: "provider quota" };

// `ship --confirm --json` prints the confirmation and then the ship result, so the result
// is the last top-level object: the last line opening one at column 0.
function parseJson(stdout: string): Record<string, unknown> | undefined {
	const lines = stdout.split("\n");
	for (let i = lines.length - 1; i >= 0; i--) {
		if (!lines[i]!.startsWith("{")) continue;
		try {
			const value = JSON.parse(lines.slice(i).join("\n"));
			return typeof value === "object" && value !== null && !Array.isArray(value) ? value : undefined;
		} catch {
			return undefined;
		}
	}
	return undefined;
}

export interface Outcome {
	ok: boolean;
	text: string;
	phase?: string;
}

export function outcome(verb: Verb, id: string, raw: Raw): Outcome {
	if (raw.missing) return { ok: false, text: "spar: the spar binary is not on PATH" };
	const json = parseJson(raw.stdout);
	const phase = typeof json?.phase === "string" ? json.phase : undefined;
	const refused = json?.ok === false;
	// `reject` exits 1 by design once the plan is rejected.
	const rejected = verb === "reject" && raw.code === 1 && phase === "plan_rejected";
	const ok = !refused && (raw.code === 0 || raw.code === 2 || rejected);
	const head = `spar ${verb} ${id}`;
	if (ok) {
		const facts: string[] = [];
		if (DETACHED.has(verb)) facts.push(`continuing detached; follow with spar wait ${id}`);
		else if (phase) facts.push(`phase ${phase}`);
		if (verb === "rule" && typeof json?.id === "string") facts.push(`ruling ${json.id} recorded`);
		if (typeof json?.pr_url === "string") facts.push(json.pr_url);
		if (typeof json?.removed === "number") facts.push(`${json.removed} worktree(s) removed`);
		if (raw.code === 2) facts.push("waiting on a human gate");
		return { ok, phase, text: `${head}: done${facts.length ? `, ${facts.join(", ")}` : ""}` };
	}
	const code = raw.code === null ? `signal ${raw.signal ?? "unknown"}` : `exit ${raw.code}: ${EXIT_MEANING[raw.code] ?? "unknown"}`;
	const detail = [
		typeof json?.error === "string" ? json.error : "",
		raw.stderr.trim(),
		json ? "" : raw.stdout.trim(),
	]
		.filter(Boolean)
		.join("\n");
	let text = `${head} failed (${code})${detail ? `\n${detail}` : ""}`;
	if ((verb === "ship" || verb === "override") && phase && OVERRIDE_PHASES.has(phase)) {
		text += `\nrun is at ${phase}; ship over the gate with /spar override ${id}`;
	}
	return { ok, phase, text };
}

export interface GateRun extends RunStatus {
	winner_slot?: string | null;
	dispatched_round?: number;
}

interface Planned {
	verb: Verb;
	args: string[];
	title: string;
	effect: string;
}

type Ui = Pick<ExtensionContext["ui"], "confirm" | "input" | "select" | "notify">;
export interface GateContext {
	cwd: string;
	hasUI: boolean;
	mode: string;
	ui: Ui;
}

// Mirrors implement's re-entry: a round opened but never dispatched is reused, anything
// else begins the next one.
export function nextRound(run: GateRun): string {
	const reuse = run.round > 0 && (run.dispatched_round ?? 0) < run.round;
	return reuse ? `re-runs round ${run.round}, which never dispatched` : `starts round ${run.round + 1}`;
}

async function ask(ui: Ui, title: string, placeholder: string, preset: string): Promise<string | undefined> {
	if (preset) return preset;
	const value = await ui.input(title, placeholder);
	return value === undefined ? undefined : value.trim();
}

async function plan(ctx: GateContext, verb: Verb, run: GateRun, preset: string): Promise<Planned | undefined> {
	const { ui } = ctx;
	const id = run.id;
	const done = (args: string[], title: string, effect: string): Planned => ({ verb, args, title, effect });
	switch (verb) {
		case "approve":
			return done(buildArgv(verb, id), `Approve the plan for run ${id}?`, "Moves the run to plan_approved. Implementation starts when you resume it.");
		case "reject": {
			const reason = await ask(ui, `Reject run ${id}: reason (optional, enter to skip)`, "why the plan is rejected", preset);
			if (reason === undefined) return undefined;
			return done(
				buildArgv(verb, id, { reason }),
				`Reject the plan for run ${id}?`,
				`Moves the run to plan_rejected and removes its worktrees that hold no work; one with uncommitted changes or unmerged commits is kept. The plan and critique stay in the run's artifacts, and spar approve ${id} can still approve it later.`,
			);
		}
		case "ship":
			return done(buildArgv(verb, id), `Ship run ${id}?`, "Pushes the run's branch and opens a draft PR. It never merges. Runs in the background.");
		case "override": {
			const reason = await ask(ui, `Override the gate on run ${id}: reason (required)`, "why this ships over its gate", preset);
			if (reason === undefined) return undefined;
			if (!reason) {
				ui.notify("spar: an override needs a reason; nothing ran", "error");
				return undefined;
			}
			return done(
				buildArgv(verb, id, { reason }),
				`Ship run ${id} over its gate?`,
				`At ${run.phase} a plain ship is refused: the review never cleared. This pushes the implementer HEAD and opens a draft PR listing every open finding; the override is recorded. Runs in the background.`,
			);
		}
		case "confirm": {
			const implementers = run.slots.filter((s) => s.role === "implementer").map((s) => s.id);
			const ranked = `ranked default${run.winner_slot ? ` (${run.winner_slot})` : ""}`;
			let winner: string | undefined = preset || undefined;
			if (!winner && implementers.length > 0) {
				const choice = await ui.select(`Confirm a winner for run ${id}`, [ranked, ...implementers]);
				if (choice === undefined) return undefined;
				winner = choice === ranked ? undefined : choice;
			}
			return done(
				buildArgv(verb, id, { winner }),
				`Confirm ${winner ?? ranked} as the winner of run ${id}?`,
				"Moves the run to the ship gate with that winner.",
			);
		}
		case "reconcile":
			return done(
				buildArgv(verb, id),
				`Reconcile run ${id}?`,
				"Dispatches a reconciler to merge the good parts of every attempt, then a review panel, then parks at the ship gate. Runs in the background.",
			);
		case "extend": {
			if (isStallPark(run)) {
				return done(
					buildArgv(verb, id, {}, run),
					`Re-enter run ${id}?`,
					`Re-enters without raising the ceiling: round ${run.round} stalled below it, so the next dispatch carries the no-progress warning.`,
				);
			}
			const text = await ask(ui, `Extend run ${id}: rounds to add (default 1)`, "1", preset);
			if (text === undefined) return undefined;
			const rounds = text === "" ? 1 : Number(text.replace(/^\+/, ""));
			if (!Number.isInteger(rounds) || rounds < 1) {
				ui.notify(`spar: "${text}" is not a whole number of rounds >= 1; nothing ran`, "error");
				return undefined;
			}
			return done(
				buildArgv(verb, id, { rounds }, run),
				`Buy ${rounds} more round${rounds === 1 ? "" : "s"} for run ${id}?`,
				`Raises the ceiling to round ${run.round + rounds} and continues the run detached.`,
			);
		}
		case "resume":
			return done(buildArgv(verb, id), `Resume run ${id}?`, `Picks the run back up from ${run.phase}, detached.`);
		case "stop":
			return done(
				buildArgv(verb, id),
				`Stop run ${id}?`,
				"Signals the orchestrator and its slots and parks the run at stopped. Branch and worktrees are kept; resume later.",
			);
		case "rule": {
			const text = await ask(ui, `Ruling for run ${id}`, "a settled point every later reviewer and implementer sees", preset);
			if (text === undefined) return undefined;
			if (!text) {
				ui.notify("spar: a ruling needs text; nothing ran", "error");
				return undefined;
			}
			const waived = preset ? "" : await ui.input("Waive acceptance criteria (optional, e.g. AC-2,AC-5)", "enter to waive none");
			if (waived === undefined) return undefined;
			const waive = waived.split(/[\s,]+/).filter(Boolean);
			const bad = waive.find((w) => !/^AC-\d+$/.test(w));
			if (bad) {
				ui.notify(`spar: "${bad}" is not a criterion id like AC-2; nothing ran`, "error");
				return undefined;
			}
			return done(
				buildArgv(verb, id, { text, waive }),
				`Record this ruling on run ${id}?`,
				`"${text}"${waive.length ? `\nWaives ${waive.join(", ")}.` : ""}\nTakes effect at the next reviewer or implementer dispatch.`,
			);
		}
		case "reassign": {
			let seat: string | undefined;
			let provider: string | undefined;
			const presetMatch = /^([^=\s]+)=(\S+)$/.exec(preset);
			if (presetMatch) {
				seat = presetMatch[1];
				provider = presetMatch[2];
			} else {
				if (run.slots.length === 0) {
					ui.notify(`spar: run ${id} has no seats yet; nothing to reassign`, "error");
					return undefined;
				}
				const labels = run.slots.map((s) => `${s.id}  ${s.role}  ${s.model && !s.provider.includes("@") ? `${s.provider}@${s.model}` : s.provider}`);
				const choice = await ui.select(`Reassign which seat of run ${id}?`, labels);
				if (choice === undefined) return undefined;
				seat = run.slots[labels.indexOf(choice)]!.id;
				provider = await ask(ui, `Move seat ${seat} to which provider?`, "cli:claude@sonnet", "");
				if (provider === undefined) return undefined;
			}
			if (!provider || /\s/.test(provider)) {
				ui.notify("spar: name a provider like cli:claude@sonnet; nothing ran", "error");
				return undefined;
			}
			return done(
				buildArgv(verb, id, { seat, provider }),
				`Move seat ${seat} of run ${id} to ${provider}?`,
				`The seat keeps its worktree, branch and history; the new provider starts cold. Then the run continues, detached, and ${nextRound(run)}.`,
			);
		}
		case "cleanup":
			return done(
				buildArgv(verb, id),
				`Clean up run ${id}?`,
				"Removes the run's worktrees only if its work is in its base (merged, squash-merged or rebased). Refused otherwise. Runs in the background.",
			);
	}
}

export type Report = (text: string, level: "info" | "warning" | "error") => void;

let reporter: Report | undefined;

// Background jobs and feed alerts outlive the command or session that started them, and a
// ctx from a replaced session throws on use. They report here instead; session_start points
// this at the live session's UI and session_shutdown clears it, so a result that lands with
// no session is dropped.
export function setReporter(next: Report | undefined): void {
	reporter = next;
}

export function report(text: string, level: "info" | "warning" | "error"): void {
	try {
		reporter?.(text, level);
	} catch {
		// A UI torn down between the check and the call: nobody to tell.
	}
}

export type ActionResult =
	| { kind: "cancelled" }
	| { kind: "ran"; outcome: Outcome }
	| { kind: "started"; done: Promise<Outcome> };

export async function performAction(ctx: GateContext, verb: Verb, run: GateRun, preset = ""): Promise<ActionResult> {
	const planned = await plan(ctx, verb, run, preset.trim());
	if (!planned) return { kind: "cancelled" };
	const command = displayCommand(planned.args);
	const confirmed = await ctx.ui.confirm(planned.title, `Runs: ${command}\n\n${planned.effect}`);
	if (!confirmed) {
		ctx.ui.notify("spar: cancelled, nothing ran", "info");
		return { kind: "cancelled" };
	}
	if (BACKGROUND.has(planned.verb)) {
		const job = runBackground(planned.args, ctx.cwd);
		inFlight.add(run.id);
		ctx.ui.notify(`spar: started in the background: ${command}`, "info");
		const done = job.done.then((raw) => {
			inFlight.delete(run.id);
			const result = outcome(planned.verb, run.id, raw);
			if (!result.ok) result.text += `\noutput kept in ${job.dir}`;
			report(result.text, result.ok ? "info" : "error");
			if (result.ok) rmSync(job.dir, { recursive: true, force: true });
			return result;
		});
		return { kind: "started", done };
	}
	const result = outcome(planned.verb, run.id, await runSpar(planned.args, ctx.cwd, TIMEOUT_MS[planned.verb]));
	ctx.ui.notify(result.text, result.ok ? "info" : "error");
	return { kind: "ran", outcome: result };
}

export function cliCommand(verb: Verb, id: string): string {
	const commands: Record<Verb, string> = {
		approve: `spar approve ${id}`,
		reject: `spar reject ${id} [--reason=<why>]`,
		ship: `spar ship ${id} --confirm`,
		override: `spar ship ${id} --override-gate --reason=<why>`,
		confirm: `spar confirm ${id} [--winner <slot>]`,
		reconcile: `spar reconcile ${id}`,
		extend: `spar implement --run ${id} [--max-rounds +<N>] --detach`,
		resume: `spar resume ${id} --detach`,
		stop: `spar stop ${id}`,
		rule: `spar rule add --run ${id} [--waive AC-<n>] -- "<text>"`,
		reassign: `spar implement --run ${id} --reassign <seat>=<provider> --detach`,
		cleanup: `spar cleanup ${id} --if-merged`,
	};
	return commands[verb];
}

function say(ctx: GateContext, text: string): void {
	if (ctx.mode === "print") process.stdout.write(`${text}\n`);
	else if (ctx.hasUI) ctx.ui.notify(text, "error");
	else process.stderr.write(`${text}\n`);
}

export function usage(verb: Verb): string {
	const extra: Partial<Record<Verb, string>> = {
		reject: " [reason]",
		override: " [reason]",
		confirm: " [winner-slot]",
		extend: " [rounds]",
		rule: " [text]",
		reassign: " [seat=provider]",
	};
	return `/spar ${verb} <run-id>${extra[verb] ?? ""}`;
}

// `/spar <verb> <id> [rest]`. Every action goes through a confirmation dialog, so a mode
// without dialogs (print, JSON) refuses instead of acting unconfirmed.
export async function verbCommand(ctx: GateContext, verb: Verb, args: string): Promise<ActionResult | undefined> {
	if (!ctx.hasUI) {
		const id = args.trim().split(/\s+/)[0] || "<id>";
		say(
			ctx,
			`spar: /spar ${verb} needs a confirmation dialog, which this mode (${ctx.mode}) has no way to show; nothing ran. Use interactive or RPC mode, or run it yourself: ${cliCommand(verb, isRunId(id) ? id : "<id>")}`,
		);
		return undefined;
	}
	const first = args.trim().split(/\s+/)[0] ?? "";
	const rest = args.trim().slice(first.length).trim();
	let id = first;
	if (id && !isRunId(id)) {
		say(ctx, `spar: "${id}" is not a run id. Usage: ${usage(verb)}`);
		return undefined;
	}
	if (!id) {
		const runs = await sparJson<RunRow[]>(["status", "--json"], ctx.cwd);
		if (!runs.ok) {
			say(ctx, runs.missing ? "spar: the spar binary is not on PATH" : `spar: ${runs.message}`);
			return undefined;
		}
		const fits = runs.value.filter((r) => actionsFor(r).includes(verb));
		if (fits.length === 0) {
			say(ctx, `spar: no run here can ${verb} now. Usage: ${usage(verb)}`);
			return undefined;
		}
		const labels = fits.map((r) => `${r.id}  ${r.phase}  ${(r.task ?? "").split("\n")[0]?.slice(0, 60) ?? ""}`.trimEnd());
		const choice = await ctx.ui.select(`${VERBS[verb].label}: which run?`, labels);
		if (choice === undefined) return undefined;
		id = fits[labels.indexOf(choice)]!.id;
	}
	const status = await sparJson<GateRun>(["status", id, "--json"], ctx.cwd);
	if (!status.ok) {
		say(ctx, status.missing ? "spar: the spar binary is not on PATH" : `spar: ${status.message}`);
		return undefined;
	}
	const valid = actionsFor(status.value);
	if (!valid.includes(verb)) {
		say(
			ctx,
			`spar: run ${id} is at ${status.value.phase}${status.value.holds_seat ? " and running" : ""}${busy(id) ? ", with a background job from this pi still going" : ""}; ${verb} does not apply. From here: ${valid.join(", ")}`,
		);
		return undefined;
	}
	return performAction(ctx, verb, status.value, rest);
}

// `/spar <verb> <partial id>` completes run ids the verb applies to; a bare word completes
// verbs. Returns undefined when the prefix is not about a verb.
export function verbCompletions(prefix: string, runs: RunRow[]): AutocompleteItem[] | undefined {
	const match = /^(\S+)\s+(\S*)$/.exec(prefix);
	if (match) {
		const verb = match[1];
		if (!isVerb(verb)) return undefined;
		return runs
			.filter((r) => r.id.startsWith(match[2]!) && actionsFor(r).includes(verb))
			.map((r) => ({ value: `${verb} ${r.id}`, label: r.id, description: r.phase }));
	}
	if (/\s/.test(prefix)) return undefined;
	return ORDER.filter((v) => v.startsWith(prefix)).map((v) => ({ value: `${v} `, label: v, description: VERBS[v].summary }));
}
