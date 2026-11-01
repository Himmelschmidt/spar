import { execFile } from "node:child_process";

export function sparBin(): string {
	return process.env.SPAR_PI_BIN || "spar";
}

const RUN_ID = /^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$/;

export function isRunId(value: string): boolean {
	return RUN_ID.test(value);
}

// spar's needs_human is true at every gate, including awaiting_reconcile while a
// reconcile is running under it. A run something is driving does not need you yet.
export function needsYou(run: { needs_human: boolean; holds_seat: boolean }): boolean {
	return run.needs_human && !run.holds_seat;
}

export const STALE_MS = 7 * 86_400_000;

export function isStale(run: { updated_at: string }, now: Date): boolean {
	return now.getTime() - Date.parse(run.updated_at) > STALE_MS;
}

export type SparResult<T> =
	| { ok: true; value: T }
	| { ok: false; missing: boolean; message: string };

export function sparJson<T>(args: string[], cwd: string, timeoutMs = 15_000): Promise<SparResult<T>> {
	return new Promise((resolve) => {
		execFile(
			sparBin(),
			args,
			{ cwd, timeout: timeoutMs, maxBuffer: 256 * 1024 * 1024, windowsHide: true },
			(error, stdout, stderr) => {
				if (error) {
					const missing = (error as NodeJS.ErrnoException).code === "ENOENT";
					const message = (stderr || error.message).trim().split("\n")[0] ?? "";
					resolve({ ok: false, missing, message });
					return;
				}
				try {
					resolve({ ok: true, value: JSON.parse(stdout) as T });
				} catch {
					resolve({ ok: false, missing: false, message: `spar ${args[0]}: unparseable JSON` });
				}
			},
		);
	});
}

export interface RunRow {
	id: string;
	phase: string;
	workflow: string;
	round: number;
	needs_human: boolean;
	holds_seat: boolean;
	resumable: boolean;
	abandoned: boolean;
	dry_run: boolean;
	updated_at: string;
	task?: string | null;
	project_name?: string | null;
	cost_usd?: number | null;
}

export interface Here {
	project_root: string | null;
	spar_initialized: boolean;
}

export interface CostLine {
	key: string;
	provider?: string | null;
	dispatches: number;
	dispatches_without_cost: number;
	billed_tokens: number;
	cost_usd: number | null;
	cost_reported: string;
}

export interface Slot {
	id: string;
	role: string;
	provider: string;
	model?: string | null;
	status: string;
	round: number;
	pid_alive?: boolean;
	stalled?: boolean;
	quota_hit?: boolean;
	auth_fault?: boolean;
	silent_for_secs?: number | null;
}

export interface RunStatus extends RunRow {
	created_at?: string;
	max_rounds?: number;
	error?: string | null;
	slots: Slot[];
	cost?: { total: CostLine; by_provider: CostLine[] };
}

export interface Finding {
	slot_id: string;
	severity: "critical" | "major" | "minor";
	file?: string | null;
	line?: number | null;
	text: string;
	follow_up?: boolean;
	synthetic?: boolean;
	possibly_addressed?: boolean;
}

export interface SparEvent {
	ts: string;
	type: string;
	phase?: string;
	prev_phase?: string;
	slot?: string;
	status?: string;
	message?: string;
	round?: number;
}

export interface Brief {
	run: string;
	next_command: string;
	max_rounds?: number;
	fleet?: string[];
	open_findings: Finding[];
	recent_events: SparEvent[];
}

export interface BusMessage {
	ts: string;
	from: string;
	to: string;
	kind: string;
	body: string;
	subject?: string | null;
}

export interface Provider {
	name: string;
	quota_status: string;
	quota_hint?: string | null;
	paused_until?: string | null;
}

export interface Stats {
	groups: { key: string; usage: { cost_usd: number | null } }[];
	run_level_only?: { cost_usd: number | null };
}

export function statsCost(stats: Stats): number {
	const groups = stats.groups.reduce((sum, g) => sum + (g.usage.cost_usd ?? 0), 0);
	return groups + (stats.run_level_only?.cost_usd ?? 0);
}

export function minutesSinceLocalMidnight(now: Date): number {
	const midnight = new Date(now);
	midnight.setHours(0, 0, 0, 0);
	return Math.max(1, Math.ceil((now.getTime() - midnight.getTime()) / 60_000));
}
