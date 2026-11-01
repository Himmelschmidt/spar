import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import type { RunRow, RunStatus } from "../src/spar.ts";

const HERE = dirname(fileURLToPath(import.meta.url));
export const FAKE_SPAR = join(HERE, "fake-spar.cjs");
export const FAKE_HERDR = join(HERE, "fake-herdr.cjs");
export const MAIN = join(HERE, "..", "src", "main.ts");
const dirs: string[] = [];

function scratch(prefix: string): string {
	const dir = mkdtempSync(join(tmpdir(), prefix));
	dirs.push(dir);
	return dir;
}

export interface FakeSpar {
	dir: string;
	argv(): string[][];
}

export function installSpar(responses: Record<string, unknown>): FakeSpar {
	const dir = scratch("spar-herdr-spar-");
	writeFileSync(join(dir, "responses.json"), JSON.stringify({ "events --tail-cursor --json": { cursor: "1" }, ...responses }));
	writeFileSync(join(dir, "calls.log"), "");
	process.env.FAKE_SPAR_DIR = dir;
	process.env.SPAR_BIN = FAKE_SPAR;
	return {
		dir,
		argv: () =>
			existsSync(join(dir, "argv.jsonl"))
				? readFileSync(join(dir, "argv.jsonl"), "utf8").split("\n").filter(Boolean).map((l) => JSON.parse(l) as string[])
				: [],
	};
}

export interface World {
	workspaces: { workspace_id: string; label: string }[];
	tabs: { tab_id: string; workspace_id: string; label: string }[];
	panes: {
		pane_id: string;
		tab_id: string;
		workspace_id: string;
		cwd?: string;
		label?: string;
		agent_status?: string;
		focused?: boolean;
		tokens?: Record<string, string>;
		entrypoint?: string;
		env?: Record<string, string>;
	}[];
	popups?: { entrypoint: string; env: Record<string, string> }[];
}

export interface FakeHerdr {
	dir: string;
	argv(): string[][];
	world(): World;
	setWorld(world: World): void;
}

export function installHerdr(world: World = { workspaces: [], tabs: [], panes: [] }): FakeHerdr {
	const dir = scratch("spar-herdr-herdr-");
	writeFileSync(join(dir, "world.json"), JSON.stringify(world));
	process.env.FAKE_HERDR_DIR = dir;
	process.env.HERDR_BIN_PATH = FAKE_HERDR;
	process.env.HERDR_PLUGIN_STATE_DIR = scratch("spar-herdr-state-");
	process.env.HERDR_PLUGIN_CONFIG_DIR = scratch("spar-herdr-config-");
	return {
		dir,
		argv: () =>
			existsSync(join(dir, "argv.jsonl"))
				? readFileSync(join(dir, "argv.jsonl"), "utf8").split("\n").filter(Boolean).map((l) => JSON.parse(l) as string[])
				: [],
		world: () => JSON.parse(readFileSync(join(dir, "world.json"), "utf8")) as World,
		setWorld: (w) => writeFileSync(join(dir, "world.json"), JSON.stringify(w)),
	};
}

export function cleanup(): void {
	for (const dir of dirs.splice(0)) rmSync(dir, { recursive: true, force: true });
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
		project_name: "alpha",
		project_root: "/p/alpha",
		...over,
	};
}

export function status(id: string, over: Partial<RunStatus> = {}): RunStatus {
	return {
		...row(id),
		max_rounds: 6,
		slots: [
			{ id: "impl", role: "implementer", provider: "cli:muse", model: "muse-spark-1.3-contributor", status: "running", round: 1, pid_alive: true },
			{ id: "review-0-cli-claude", role: "reviewer", provider: "cli:claude", model: "opus", status: "done", round: 1 },
		],
		...over,
	};
}
