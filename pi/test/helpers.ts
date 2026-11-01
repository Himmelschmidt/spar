import { existsSync, mkdtempSync, readFileSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import type { RunRow, RunStatus } from "../extensions/spar/spar.ts";

const FAKE = join(dirname(fileURLToPath(import.meta.url)), "fake-spar.cjs");
const ORIGINAL_PATH = process.env.PATH ?? "";
const dirs: string[] = [];

function scratch(prefix: string): string {
	const dir = mkdtempSync(join(tmpdir(), prefix));
	dirs.push(dir);
	return dir;
}

export interface Fake {
	dir: string;
	calls(): string[];
	argv(): string[][];
	pid(session: number): number | undefined;
}

export function installFake(responses: Record<string, unknown>): Fake {
	const dir = scratch("spar-pi-test-");
	symlinkSync(FAKE, join(dir, "spar"));
	writeFileSync(join(dir, "responses.json"), JSON.stringify({ "events --tail-cursor --json": { cursor: "1" }, ...responses }));
	writeFileSync(join(dir, "calls.log"), "");
	process.env.FAKE_SPAR_DIR = dir;
	delete process.env.SPAR_PI_BIN;
	process.env.PATH = `${dir}:${ORIGINAL_PATH}`;
	return {
		dir,
		calls: () => readFileSync(join(dir, "calls.log"), "utf8").split("\n").filter(Boolean),
		argv: () =>
			existsSync(join(dir, "argv.jsonl"))
				? readFileSync(join(dir, "argv.jsonl"), "utf8").split("\n").filter(Boolean).map((l) => JSON.parse(l) as string[])
				: [],
		pid: (session) => {
			const file = join(dir, `events-${session}.pid`);
			return existsSync(file) ? Number(readFileSync(file, "utf8")) : undefined;
		},
	};
}

export function installNoSpar(): void {
	delete process.env.SPAR_PI_BIN;
	process.env.PATH = scratch("spar-pi-empty-");
}

export function restorePath(): void {
	process.env.PATH = ORIGINAL_PATH;
}

export function cleanup(): void {
	restorePath();
	for (const dir of dirs.splice(0)) rmSync(dir, { recursive: true, force: true });
}

export async function until(check: () => boolean, timeoutMs = 15_000): Promise<void> {
	const start = Date.now();
	while (!check()) {
		if (Date.now() - start > timeoutMs) throw new Error("timed out waiting for condition");
		await new Promise((r) => setTimeout(r, 20));
	}
}

export function alive(pid: number): boolean {
	try {
		process.kill(pid, 0);
		return true;
	} catch {
		return false;
	}
}

const HOUR = 3_600_000;

export function row(id: string, over: Partial<RunRow> = {}): RunRow {
	return {
		id,
		phase: "review",
		workflow: "loop",
		round: 1,
		needs_human: false,
		holds_seat: false,
		resumable: false,
		abandoned: false,
		dry_run: false,
		updated_at: new Date(Date.now() - HOUR).toISOString(),
		task: `task for ${id}\nmore`,
		cost_usd: null,
		...over,
	};
}

export function status(id: string, over: Partial<RunStatus> = {}): RunStatus {
	return {
		...row(id),
		max_rounds: 6,
		created_at: new Date(Date.now() - 3 * HOUR).toISOString(),
		slots: [
			{ id: "impl", role: "implementer", provider: "cli:muse", model: "muse-spark", status: "running", round: 2, pid_alive: true },
			{ id: "review-0-cli-claude", role: "reviewer", provider: "cli:claude", model: "opus", status: "done", round: 2 },
		],
		cost: {
			total: { key: "total", dispatches: 4, dispatches_without_cost: 1, billed_tokens: 10, cost_usd: 12.5, cost_reported: "partial" },
			by_provider: [
				{ key: "cli:claude", provider: "cli:claude", dispatches: 3, dispatches_without_cost: 0, billed_tokens: 9, cost_usd: 12.5, cost_reported: "all" },
			],
		},
		...over,
	};
}
