import type { FeedEvent } from "./feed.ts";
import { needsYou, type RunRow } from "./spar.ts";

export interface Alert {
	runId: string;
	message: string;
	level: "warning" | "error";
}

// Not `awaiting_reconcile`: only `spar reconcile` sets it, as it starts, and it holds the
// run until the ship gate. Alerting there would ask the operator for what they just did.
const GATES = new Set([
	"awaiting_plan_approval",
	"awaiting_winner_confirm",
	"awaiting_ship_confirm",
	"awaiting_round_extension",
]);
const BROKEN = new Set(["failed", "stuck", "escalated", "quota"]);
// The approval handoff: spar counts it as needing the operator (nothing implements an
// approved plan until someone resumes it), and so does the footer.
const HANDOFF = "plan_approved";

function attentionKey(phase: string | undefined, abandoned: boolean): string | undefined {
	if (!phase) return abandoned ? "abandoned" : undefined;
	if (abandoned) return `abandoned:${phase}`;
	if (GATES.has(phase) || BROKEN.has(phase) || phase === HANDOFF) return phase;
	return undefined;
}

function describe(key: string): string {
	if (key.startsWith("abandoned")) {
		const phase = key.split(":")[1];
		return `abandoned${phase ? ` in ${phase}` : ""}, no live orchestrator`;
	}
	if (GATES.has(key)) return `waiting at ${key}`;
	if (key === HANDOFF) return "plan approved, waiting to be implemented (resume it)";
	return key;
}

// One alert per run per state it newly enters. A gate event repeats the phase event
// before it and a restarted daemon can report the same abandonment twice; both map to
// the state already alerted. Leaving for a state that needs nobody clears it, so the
// next entry alerts again.
export class AttentionTracker {
	private known = new Map<string, string>();

	seed(runs: RunRow[], before: Date): void {
		for (const run of runs) {
			if (this.known.has(run.id) || Date.parse(run.updated_at) >= before.getTime()) continue;
			const key = needsYou(run) ? attentionKey(run.phase, run.abandoned) : undefined;
			if (key) this.known.set(run.id, key);
		}
	}

	onEvent(event: FeedEvent): Alert | undefined {
		let key: string | undefined;
		if (event.type === "abandoned") key = attentionKey(event.phase, true);
		else if (event.type === "phase" || event.type === "gate") key = attentionKey(event.phase, false);
		else if (event.type === "archived") {
			this.known.delete(event.run_id);
			return undefined;
		} else return undefined;

		if (!key) {
			if (event.type === "phase") this.known.delete(event.run_id);
			return undefined;
		}
		if (this.known.get(event.run_id) === key) return undefined;
		this.known.set(event.run_id, key);
		return {
			runId: event.run_id,
			message: `spar: run ${event.run_id} needs you: ${describe(key)}. /spar ${event.run_id}`,
			level: GATES.has(key) || key === HANDOFF ? "warning" : "error",
		};
	}
}
