import { execFile } from "node:child_process";
import { existsSync } from "node:fs";

export function sparBin(): string {
	return process.env.SPAR_BIN || "spar";
}

const RUN_ID = /^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$/;

export function isRunId(value: string): boolean {
	return RUN_ID.test(value);
}

export type SparResult<T> = { ok: true; value: T } | { ok: false; missing: boolean; notFound: boolean; message: string };

export function sparJson<T>(args: string[], cwd: string, timeoutMs = 30_000): Promise<SparResult<T>> {
	return new Promise((resolve) => {
		execFile(sparBin(), args, { cwd, timeout: timeoutMs, maxBuffer: 256 * 1024 * 1024 }, (error, stdout, stderr) => {
			if (error) {
				// A spawn in a directory that no longer exists fails ENOENT too: the project is gone.
				if (!existsSync(cwd)) return resolve({ ok: false, missing: false, notFound: true, message: `project root ${cwd} is gone` });
				const missing = (error as NodeJS.ErrnoException).code === "ENOENT";
				const message = clean(stderr || error.message).trim().split("\n")[0] ?? "";
				resolve({ ok: false, missing, notFound: /\bnot found\b/.test(message), message });
				return;
			}
			try {
				resolve({ ok: true, value: cleanDeep(JSON.parse(stdout) as T) });
			} catch {
				resolve({ ok: false, missing: false, notFound: false, message: `spar ${args[0]}: unparseable JSON` });
			}
		});
	});
}

// Escape sequences and C0/C1 controls from spar's output must not reach the terminal: a
// task or log line could otherwise repaint the pane or hide text.
const ESCAPES = /\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)?|\x1b[PX^_][^\x1b]*(?:\x1b\\)?|\x1b[\s\S]?/g;
const CONTROLS = /[\x00-\x08\x0b-\x1f\x7f-\x9f]/g;

export function clean(text: string): string {
	return text.replace(ESCAPES, "").replace(CONTROLS, "");
}

export function cleanDeep<T>(value: T): T {
	if (typeof value === "string") return clean(value) as T;
	if (Array.isArray(value)) return value.map(cleanDeep) as T;
	if (value && typeof value === "object") {
		return Object.fromEntries(Object.entries(value).map(([k, v]) => [k, cleanDeep(v)])) as T;
	}
	return value;
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
	project_root?: string | null;
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
}

export interface RunStatus extends RunRow {
	max_rounds?: number;
	error?: string | null;
	archived_at?: string | null;
	winner_slot?: string | null;
	dispatched_round?: number;
	slots: Slot[];
}

export interface Finding {
	slot_id: string;
	severity: "critical" | "major" | "minor";
	file?: string | null;
	line?: number | null;
	text: string;
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
	error?: string | null;
	artifacts?: { name: string; path: string }[];
	open_findings: Finding[];
	recent_events: SparEvent[];
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

export type AgentState = "blocked" | "working" | "idle";

export function agentState(run: RunRow, now: Date): AgentState {
	if (needsYou(run) && !isStale(run, now)) return "blocked";
	return run.holds_seat ? "working" : "idle";
}

export function flag(run: RunRow, now: Date): string {
	if (needsYou(run)) return isStale(run, now) ? "stale" : "NEEDS YOU";
	return run.holds_seat ? "running" : "";
}

// A run gets a pane while it needs you, holds a seat, or moved within the recent window
// (finished runs stay visible long enough to be seen).
export function relevant(run: RunRow, now: Date, recentMs: number): boolean {
	return agentState(run, now) !== "idle" || now.getTime() - Date.parse(run.updated_at) < recentMs;
}

export function firstLine(text: string | null | undefined): string {
	return (text ?? "").split("\n").find((l) => l.trim() !== "")?.trim() ?? "";
}
