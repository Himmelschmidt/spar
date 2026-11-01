import type { RunStatus } from "./spar.ts";

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

export const VERBS: Record<Verb, string> = {
	approve: "approve the plan",
	reject: "reject the plan, with an optional reason",
	ship: "confirm the ship gate: push and open a draft PR",
	override: "ship over a gate the review never cleared, with a reason",
	confirm: "confirm the arena winner (ranked default or a pick)",
	reconcile: "merge the arena's good parts, then review",
	extend: "buy N more rounds and keep going",
	resume: "pick the run back up, detached",
	stop: "halt the run, keeping branch and worktrees",
	rule: "record an operator ruling on the run",
	reassign: "move a seat to another provider, then continue",
	cleanup: "remove worktrees if the work is merged",
};

const ORDER = Object.keys(VERBS) as Verb[];

const GATES = new Set([
	"awaiting_plan_approval",
	"awaiting_winner_confirm",
	"awaiting_reconcile",
	"awaiting_ship_confirm",
	"awaiting_round_extension",
]);
const AT_REST = new Set([...GATES, "done", "plan_rejected", "plan_approved", "stopped", "failed", "stuck", "quota", "escalated"]);
// Where a plain `spar ship --confirm` is refused and spar allows the override.
const OVERRIDE_PHASES = new Set(["awaiting_reconcile", "awaiting_round_extension", "stuck", "failed", "quota"]);
// Where `implement --run --reassign` is accepted: a parked, resumable run.
const REASSIGN_PHASES = new Set(["stopped", "failed", "stuck", "quota", "plan_approved"]);

export function isGate(phase: string): boolean {
	return GATES.has(phase);
}

// Mirrors pi/extensions/spar/gates.ts `actionsFor`, the source of truth for which verbs
// apply when. A hint only: spar refuses anything this gets wrong, and the popup shows that.
export function actionsFor(run: Pick<RunStatus, "phase" | "holds_seat" | "resumable">): Verb[] {
	const verbs = new Set<Verb>();
	if (!run.holds_seat) {
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
	const inFlight = !AT_REST.has(run.phase) && run.phase !== "shipping";
	if (inFlight || (run.phase === "awaiting_reconcile" && run.holds_seat)) verbs.add("stop");
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

export function buildArgv(verb: Verb, id: string, input: ActionInput = {}): string[] {
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
			return ["implement", "--run", id, "--max-rounds", `+${input.rounds ?? 1}`, "--detach", "--json"];
		case "resume":
			return ["resume", id, "--detach", "--json"];
		case "stop":
			return ["stop", id, "--json"];
		case "rule":
			return ["rule", "add", "--run", id, ...(input.waive ?? []).flatMap((w) => ["--waive", w]), "--json", "--", input.text ?? ""];
		case "reassign":
			return ["implement", "--run", id, "--reassign", `${input.seat}=${input.provider}`, "--detach", "--json"];
		case "cleanup":
			return ["cleanup", id, "--if-merged", "--json"];
	}
}

// Pushes, agent dispatches and gh calls: started detached so closing the popup does not
// kill them half-way.
export const BACKGROUND = new Set<Verb>(["ship", "override", "reconcile", "cleanup"]);

const DETACHED = new Set<Verb>(["extend", "resume", "reassign"]);

export function displayCommand(args: string[]): string {
	const quote = (a: string) => (/^[A-Za-z0-9_@%+=:,./-]+$/.test(a) ? a : `'${a.replace(/'/g, `'\\''`)}'`);
	return ["spar", ...args].map(quote).join(" ");
}

export interface Raw {
	code: number | null;
	stdout: string;
	stderr: string;
	missing?: boolean;
}

const EXIT_MEANING: Record<number, string> = { 0: "ok", 1: "failed", 2: "waiting on a human gate", 3: "stuck", 4: "provider quota" };

// `ship --confirm --json` prints the confirmation and then the ship result, so the result
// is the last top-level object: the last line opening one at column 0.
function lastJson(stdout: string): Record<string, unknown> | undefined {
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
}

export function outcome(verb: Verb, id: string, raw: Raw): Outcome {
	if (raw.missing) return { ok: false, text: "the spar binary was not found" };
	const json = lastJson(raw.stdout);
	const phase = typeof json?.phase === "string" ? json.phase : undefined;
	// `reject` exits 1 by design once the plan is rejected.
	const rejected = verb === "reject" && raw.code === 1 && phase === "plan_rejected";
	const ok = json?.ok !== false && (raw.code === 0 || raw.code === 2 || rejected);
	const head = `spar ${verb} ${id}`;
	if (ok) {
		const facts: string[] = [];
		if (DETACHED.has(verb)) facts.push(`continuing detached; follow with spar wait ${id}`);
		else if (phase) facts.push(`phase ${phase}`);
		if (typeof json?.pr_url === "string") facts.push(json.pr_url);
		if (typeof json?.removed === "number") facts.push(`${json.removed} worktree(s) removed`);
		if (raw.code === 2) facts.push("waiting on a human gate");
		return { ok, text: `${head}: done${facts.length ? `, ${facts.join(", ")}` : ""}` };
	}
	const code = raw.code === null ? "killed" : `exit ${raw.code}: ${EXIT_MEANING[raw.code] ?? "unknown"}`;
	const detail = [typeof json?.error === "string" ? json.error : "", raw.stderr.trim(), json ? "" : raw.stdout.trim()].filter(Boolean).join("\n");
	return { ok, text: `${head} failed (${code})${detail ? `\n${detail}` : ""}` };
}

// Mirrors implement's re-entry: a round opened but never dispatched is reused, anything
// else begins the next one.
export function nextRound(run: Pick<RunStatus, "round" | "dispatched_round">): string {
	const reuse = run.round > 0 && (run.dispatched_round ?? 0) < run.round;
	return reuse ? `re-runs round ${run.round}, which never dispatched` : `starts round ${run.round + 1}`;
}

export function effect(verb: Verb, run: RunStatus, input: ActionInput): string {
	switch (verb) {
		case "approve":
			return "Moves the run to plan_approved. Implementation starts when you resume it.";
		case "reject":
			return "Moves the run to plan_rejected and removes its worktrees that hold no work. The plan stays in the run's artifacts; approve can still approve it later.";
		case "ship":
			return "Pushes the run's branch and opens a draft PR. It never merges.";
		case "override":
			return `At ${run.phase} a plain ship is refused: the review never cleared. This pushes the implementer HEAD and opens a draft PR listing every open finding; the override is recorded.`;
		case "confirm":
			return "Moves the run to the ship gate with that winner.";
		case "reconcile":
			return "Dispatches a reconciler to merge the good parts of every attempt, then a review panel, then parks at the ship gate.";
		case "extend":
			return `Raises the ceiling to round ${run.round + (input.rounds ?? 1)} and continues the run detached.`;
		case "resume":
			return `Picks the run back up from ${run.phase}, detached.`;
		case "stop":
			return "Signals the orchestrator and its slots and parks the run at stopped. Branch and worktrees are kept.";
		case "rule":
			return `Records "${input.text ?? ""}"${input.waive?.length ? `, waiving ${input.waive.join(", ")}` : ""}. Takes effect at the next reviewer or implementer dispatch.`;
		case "reassign":
			return `The seat keeps its worktree, branch and history; the new provider starts cold. The run continues detached and ${nextRound(run)}.`;
		case "cleanup":
			return "Removes the run's worktrees only if its work is in its base. Refused otherwise.";
	}
}
