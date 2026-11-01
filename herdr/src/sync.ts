import { homedir } from "node:os";
import { basename } from "node:path";
import { type Config, inScope, loadState, type RunPane, type State, saveState, withLock } from "./config.ts";
import { herdr, layoutOf, openPane, type Snapshot, snapshot, splitTarget } from "./herdr.ts";
import { type RunRow, relevant, sparJson } from "./spar.ts";

export const RUN_TAB = "spar";

export interface SyncResult {
	runs: RunRow[];
	opened: string[];
	adopted: string[];
	closed: string[];
	workspaces: string[];
	errors: string[];
}

export async function listRuns(config: Config): Promise<RunRow[]> {
	const listed = await sparJson<RunRow[]>(["status", "--all", "--json"], homedir());
	if (!listed.ok) throw new Error(listed.missing ? `spar binary not found: ${process.env.SPAR_BIN || "spar"}` : `spar status --all: ${listed.message}`);
	return listed.value.filter((r) => r.project_root && inScope(config, r));
}

async function ensureWorkspace(root: string, label: string, snap: Snapshot, state: State, result: SyncResult): Promise<string> {
	const known = state.workspaces[root];
	if (known && snap.workspaces.some((w) => w.workspace_id === known)) return known;
	const candidates = new Set(snap.panes.filter((p) => p.cwd === root).map((p) => p.workspace_id));
	const reuse = snap.workspaces.find((w) => candidates.has(w.workspace_id) && w.label === label) ?? snap.workspaces.find((w) => candidates.has(w.workspace_id));
	if (reuse) {
		state.workspaces[root] = reuse.workspace_id;
		return reuse.workspace_id;
	}
	const created = await herdr(["workspace", "create", "--cwd", root, "--label", label, "--no-focus"]);
	if (!created.ok) throw new Error(`herdr workspace create: ${created.message}`);
	const id = (created.result.workspace as { workspace_id?: string } | undefined)?.workspace_id;
	if (!id) throw new Error("herdr workspace create: no workspace in the response");
	snap.workspaces.push({ workspace_id: id, label });
	state.workspaces[root] = id;
	result.workspaces.push(id);
	return id;
}

// The tab holding this workspace's run panes: one that already has a run pane in it, else
// one labelled `spar`.
function runTab(workspace: string, snap: Snapshot, state: State): string | undefined {
	const ours = new Set(Object.values(state.runs).map((r) => r.pane_id));
	const withRun = snap.panes.find((p) => p.workspace_id === workspace && ours.has(p.pane_id));
	if (withRun) return withRun.tab_id;
	return snap.tabs.find((t) => t.workspace_id === workspace && t.label === RUN_TAB)?.tab_id;
}

async function openRunPane(run: RunRow, root: string, workspace: string, snap: Snapshot, state: State): Promise<string> {
	const env = { SPAR_HERDR_RUN: run.id, SPAR_HERDR_ROOT: root };
	const tab = runTab(workspace, snap, state);
	const inTab = tab ? snap.panes.find((p) => p.tab_id === tab) : undefined;
	if (!tab || !inTab) {
		const pane = await openPane({ entrypoint: "run", env, cwd: root, placement: "tab", workspace });
		await herdr(["tab", "rename", pane.tab_id, RUN_TAB]);
		snap.tabs.push({ tab_id: pane.tab_id, workspace_id: workspace, label: RUN_TAB });
		snap.panes.push(pane);
		return pane.pane_id;
	}
	const split = splitTarget(await layoutOf(inTab.pane_id)) ?? { target: inTab.pane_id, direction: "right" as const };
	const pane = await openPane({ entrypoint: "run", env, cwd: root, placement: "split", target: split.target, direction: split.direction });
	snap.panes.push(pane);
	return pane.pane_id;
}

// Brings herdr in line with spar: every relevant run has exactly one run pane, in a `spar`
// tab of its project's workspace. A pane whose run stopped mattering is closed once it is
// seen (herdr's `idle`, not its unseen `done`) and not focused.
export async function syncOnce(config: Config, now = new Date()): Promise<SyncResult> {
	const runs = await listRuns(config);
	const result: SyncResult = { runs, opened: [], adopted: [], closed: [], workspaces: [], errors: [] };
	const recentMs = config.recent_hours * 3_600_000;
	const wanted = new Map(runs.filter((r) => relevant(r, now, recentMs)).map((r) => [r.id, r]));
	const snap = await snapshot();
	const state = loadState();
	const live = new Map(snap.panes.map((p) => [p.pane_id, p]));

	for (const [id, entry] of Object.entries(state.runs)) {
		if (!live.has(entry.pane_id)) delete state.runs[id];
	}
	for (const pane of snap.panes) {
		const id = pane.tokens?.spar_run;
		if (!id || state.runs[id]) continue;
		state.runs[id] = { pane_id: pane.pane_id, root: pane.tokens?.spar_root ?? pane.cwd ?? "" };
		result.adopted.push(id);
	}
	for (const [id, entry] of Object.entries(state.runs)) {
		if (wanted.has(id)) continue;
		const pane = live.get(entry.pane_id);
		if (!pane || pane.focused || pane.agent_status === "done" || pane.agent_status === "blocked" || pane.agent_status === "working") continue;
		const closed = await herdr(["pane", "close", entry.pane_id]);
		if (!closed.ok) {
			result.errors.push(`close ${entry.pane_id}: ${closed.message}`);
			continue;
		}
		delete state.runs[id];
		snap.panes = snap.panes.filter((p) => p.pane_id !== entry.pane_id);
		result.closed.push(id);
	}

	const missing = [...wanted.values()].filter((r) => !state.runs[r.id]);
	const byRoot = new Map<string, RunRow[]>();
	for (const run of missing) byRoot.set(run.project_root!, [...(byRoot.get(run.project_root!) ?? []), run]);
	for (const [root, group] of byRoot) {
		try {
			const workspace = await ensureWorkspace(root, group[0]!.project_name || basename(root), snap, state, result);
			for (const run of group) {
				const pane_id = await openRunPane(run, root, workspace, snap, state);
				state.runs[run.id] = { pane_id, root } satisfies RunPane;
				result.opened.push(run.id);
				saveState(state);
			}
		} catch (error) {
			result.errors.push((error as Error).message);
		}
	}
	saveState(state);
	return result;
}

// The manifest titles of this plugin's long-lived panes. herdr restores a pane after a
// server restart as a plain shell under the same id and label; its process is gone.
const PANE_TITLES = new Set(["spar run", "spar board", "spar seat log"]);

// Run at startup: a pane with one of those titles but none of the tokens its process
// reports is such a shell, and is closed so sync can reopen the run panes. Panes that
// survived a live handoff still carry their tokens and are left alone.
export async function reapRestored(): Promise<number> {
	const snap = await snapshot();
	let closed = 0;
	for (const pane of snap.panes) {
		if (!pane.label || !PANE_TITLES.has(pane.label)) continue;
		if (Object.keys(pane.tokens ?? {}).some((k) => k.startsWith("spar_"))) continue;
		if ((await herdr(["pane", "close", pane.pane_id])).ok) closed++;
	}
	return closed;
}

export async function sync(config: Config): Promise<SyncResult | undefined> {
	return withLock(() => syncOnce(config));
}

export function describeSync(result: SyncResult | undefined): string {
	if (!result) return "spar sync: another sync held the lock; nothing done";
	const parts = [
		`${result.opened.length} opened`,
		`${result.adopted.length} adopted`,
		`${result.closed.length} closed`,
		`${result.workspaces.length} workspaces created`,
	];
	const errors = result.errors.length ? `\n${result.errors.join("\n")}` : "";
	return `spar sync: ${parts.join(", ")}${errors}`;
}
