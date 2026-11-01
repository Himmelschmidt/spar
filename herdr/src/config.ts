import { createHash } from "node:crypto";
import { closeSync, existsSync, fstatSync, linkSync, mkdirSync, openSync, readFileSync, readSync, renameSync, rmSync, statSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

export interface Config {
	spar_bin?: string;
	projects?: string[];
	recent_hours: number;
	board_on_startup: boolean;
	sync_interval_secs: number;
}

const MIN_SYNC_SECS = 10;
const DEFAULTS: Config = { recent_hours: 24, board_on_startup: true, sync_interval_secs: 60 };

function configDir(): string {
	return process.env.HERDR_PLUGIN_CONFIG_DIR || join(homedir(), ".config", "herdr", "plugins", "config", "spar.monitor");
}

function stateDir(): string {
	return process.env.HERDR_PLUGIN_STATE_DIR || join(homedir(), ".local", "state", "herdr", "plugins", "spar.monitor");
}

// Also points SPAR_BIN at the configured binary unless the environment already set one,
// so every spar call in this process uses it.
export function loadConfig(): Config {
	const path = join(configDir(), "config.json");
	let file: Partial<Config> = {};
	if (existsSync(path)) {
		try {
			file = JSON.parse(readFileSync(path, "utf8")) as Partial<Config>;
		} catch (error) {
			throw new Error(`spar monitor: ${path} is not valid JSON: ${(error as Error).message}`);
		}
	}
	const config = { ...DEFAULTS, ...file };
	config.sync_interval_secs = Math.max(MIN_SYNC_SECS, Number(config.sync_interval_secs) || DEFAULTS.sync_interval_secs);
	config.recent_hours = Math.max(0, Number(config.recent_hours) || 0);
	if (!process.env.SPAR_BIN && config.spar_bin) process.env.SPAR_BIN = config.spar_bin;
	return config;
}

export function inScope(config: Config, run: { project_root?: string | null; project_name?: string | null }): boolean {
	if (!config.projects || config.projects.length === 0) return true;
	return config.projects.some((p) => p === run.project_root || p === run.project_name);
}

export interface RunPane {
	pane_id: string;
	root: string;
}

export interface State {
	runs: Record<string, RunPane>;
	workspaces: Record<string, string>;
	// Runs whose pane the operator closed, with the run's updated_at at the time: reopened
	// only once the run moves again.
	dismissed: Record<string, string>;
	board_pane?: string;
}

// The state directory is per user, but pane and workspace ids are per herdr server, so
// each server (named by its socket) gets its own state file and lock.
function serverKey(): string {
	const socket = process.env.HERDR_SOCKET_PATH;
	return socket ? createHash("sha256").update(socket).digest("hex").slice(0, 16) : "default";
}

function statePath(): string {
	return join(stateDir(), `state-${serverKey()}.json`);
}

export function loadState(): State {
	try {
		const raw = JSON.parse(readFileSync(statePath(), "utf8")) as Partial<State>;
		return { runs: raw.runs ?? {}, workspaces: raw.workspaces ?? {}, dismissed: raw.dismissed ?? {}, board_pane: raw.board_pane };
	} catch {
		return { runs: {}, workspaces: {}, dismissed: {} };
	}
}

export function saveState(state: State): void {
	mkdirSync(stateDir(), { recursive: true });
	const tmp = `${statePath()}.${process.pid}`;
	writeFileSync(tmp, `${JSON.stringify(state, null, 2)}\n`);
	renameSync(tmp, statePath());
}

function alive(pid: number): boolean {
	try {
		process.kill(pid, 0);
		return true;
	} catch (error) {
		return (error as NodeJS.ErrnoException).code === "EPERM";
	}
}

function readLock(path: string): { pid: number; ino: number } | undefined {
	let fd: number;
	try {
		fd = openSync(path, "r");
	} catch {
		return undefined;
	}
	try {
		const buf = Buffer.alloc(32);
		const n = readSync(fd, buf, 0, buf.length, 0);
		return { pid: Number(buf.subarray(0, n).toString("utf8").trim()), ino: fstatSync(fd).ino };
	} finally {
		closeSync(fd);
	}
}

// Moves a dead holder's lock aside. Another waiter may have replaced it with a live lock
// between our read and the rename; the inode tells, and a live lock taken by mistake is
// linked back (link never overwrites).
function takeOver(path: string, deadIno: number): void {
	const aside = `${path}.stale.${process.pid}`;
	try {
		renameSync(path, aside);
	} catch {
		return;
	}
	if (statSync(aside).ino !== deadIno) {
		try {
			linkSync(aside, path);
		} catch {
			// A third holder already took the slot.
		}
	}
	rmSync(aside, { force: true });
}

// The startup hook, the board and a manual sync can overlap; two syncs racing would both
// see a run without a pane and open two. Returns undefined when another holder kept the
// lock past the wait.
export async function withLock<T>(work: () => Promise<T>, waitMs = 20_000): Promise<T | undefined> {
	mkdirSync(stateDir(), { recursive: true });
	const path = join(stateDir(), `sync-${serverKey()}.lock`);
	// Written whole first and linked into place, so the lock never exists without its pid.
	const tmp = `${path}.${process.pid}`;
	writeFileSync(tmp, String(process.pid));
	const start = Date.now();
	for (;;) {
		try {
			linkSync(tmp, path);
			break;
		} catch (error) {
			if ((error as NodeJS.ErrnoException).code !== "EEXIST") throw error;
			const held = readLock(path);
			if (!held) continue;
			if (!Number.isInteger(held.pid) || held.pid <= 0 || !alive(held.pid)) {
				takeOver(path, held.ino);
				continue;
			}
			if (Date.now() - start > waitMs) {
				rmSync(tmp, { force: true });
				return undefined;
			}
			await new Promise((r) => setTimeout(r, 200));
		}
	}
	rmSync(tmp, { force: true });
	try {
		return await work();
	} finally {
		rmSync(path, { force: true });
	}
}
