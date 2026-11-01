import { spawn } from "node:child_process";
import { homedir } from "node:os";
import { createInterface } from "node:readline";
import { type Config, loadState, saveState, withLock } from "./config.ts";
import { Feed } from "./feed.ts";
import { herdr, layoutOf, openPane, PLUGIN_ID, type PaneInfo, pluginContext, SOURCE, snapshot, splitTarget } from "./herdr.ts";
import { clean, firstLine, flag, isRunId, type RunRow, type RunStatus, relevant, sparBin, sparJson } from "./spar.ts";
import { describeSync, owns, type SyncResult, syncOnce } from "./sync.ts";
import { age } from "./view.ts";
import { wrap } from "./wrap.ts";

export interface FocusedRun {
	id: string;
	root: string;
	workspace: string;
}

// The run shown in the pane the action was invoked on, if that pane is one of this plugin's
// run panes. The project root comes from spar by run id: herdr cuts token values at 80
// characters, so a root carried in a token can be wrong.
export async function focusedRun(): Promise<FocusedRun | undefined> {
	const paneId = pluginContext().focused_pane_id ?? process.env.HERDR_PANE_ID;
	if (!paneId) return undefined;
	const got = await herdr(["pane", "get", paneId]);
	if (!got.ok) return undefined;
	const pane = got.result.pane as PaneInfo | undefined;
	const id = pane?.tokens?.spar_run;
	if (!pane || !id || !isRunId(id) || !owns(pane, id)) return undefined;
	const status = await sparJson<RunStatus>(["status", id, "--json"], homedir());
	if (!status.ok) throw new Error(`spar status ${id}: ${status.message}`);
	const root = status.value.project_root;
	if (!root) throw new Error(`spar status ${id} names no project root`);
	return { id, root, workspace: pane.workspace_id };
}

export async function openActPopup(): Promise<void> {
	const run = await focusedRun();
	const args = ["plugin", "pane", "open", "--plugin", PLUGIN_ID, "--entrypoint", "act"];
	if (run) args.push("--cwd", run.root, "--env", `SPAR_HERDR_RUN=${run.id}`, "--env", `SPAR_HERDR_ROOT=${run.root}`);
	const res = await herdr(args);
	if (!res.ok) throw new Error(`herdr plugin pane open: ${res.message}`);
}

const MAX_LOG_PANES = 6;

export async function openSeatLogs(): Promise<string> {
	const run = await focusedRun();
	if (!run) throw new Error("the focused pane is not a spar run pane");
	const status = await sparJson<RunStatus>(["status", run.id, "--json"], run.root);
	if (!status.ok) throw new Error(`spar status ${run.id}: ${status.message}`);
	const current = status.value.slots.filter((s) => s.round === status.value.round);
	const seats = (current.length > 0 ? current : status.value.slots).slice(0, MAX_LOG_PANES);
	if (seats.length === 0) throw new Error(`run ${run.id} has no seats yet`);
	let tabPane: string | undefined;
	for (const seat of seats) {
		const env = { SPAR_HERDR_RUN: run.id, SPAR_HERDR_ROOT: run.root, SPAR_HERDR_SLOT: seat.id };
		if (!tabPane) {
			const pane = await openPane({ entrypoint: "logs", env, cwd: run.root, placement: "tab", workspace: run.workspace });
			await herdr(["tab", "rename", pane.tab_id, `logs ${run.id}`]);
			tabPane = pane.pane_id;
			continue;
		}
		const split = splitTarget(await layoutOf(tabPane)) ?? { target: tabPane, direction: "right" as const };
		await openPane({ entrypoint: "logs", env, cwd: run.root, placement: "split", target: split.target, direction: split.direction });
	}
	return `spar: ${seats.length} log pane(s) for run ${run.id}`;
}

// One seat's log, followed until the pane closes. If spar stops following, the pane
// stays with the reason until Enter, rather than vanishing.
export async function followLog(id: string, root: string, slot: string): Promise<void> {
	const pane = process.env.HERDR_PANE_ID;
	if (pane) await herdr(["pane", "report-metadata", pane, "--source", SOURCE, "--title", `${id} ${slot}`, "--token", `spar_log=${id}/${slot}`]);
	process.stdout.write(`spar logs ${id} ${slot} --follow\n`);
	// Line by line through `clean`: a seat's raw output could otherwise carry escape
	// sequences that repaint the pane or spoof the agent state herdr reads from it.
	const child = spawn(sparBin(), ["logs", id, slot, "--follow"], { cwd: root, stdio: ["ignore", "pipe", "pipe"] });
	for (const stream of [child.stdout, child.stderr]) {
		createInterface({ input: stream }).on("line", (line) => process.stdout.write(`${clean(line)}\n`));
	}
	const quit = () => {
		child.kill("SIGTERM");
		process.exit(0);
	};
	for (const sig of ["SIGHUP", "SIGTERM", "SIGINT"] as const) process.on(sig, quit);
	child.on("error", (e) => process.stdout.write(`spar logs: ${e.message}\n`));
	child.on("close", (code) => {
		process.stdout.write(`\n[spar logs ended${code === null ? "" : `, exit ${code}`}; press Enter to close]`);
		process.stdin.once("data", () => process.exit(0));
		process.stdin.resume();
	});
}

export async function ensureBoard(): Promise<void> {
	const state = loadState();
	if (state.board_pane) {
		const got = await herdr(["pane", "get", state.board_pane]);
		const pane = got.ok ? (got.result.pane as PaneInfo | undefined) : undefined;
		if (pane?.tokens?.spar_board && pane.label === "spar board") return;
	}
	const snap = await snapshot();
	const workspace = pluginContext().workspace_id ?? snap.focused_workspace_id ?? snap.workspaces[0]?.workspace_id;
	if (!workspace) throw new Error("no herdr workspace to open the board in");
	const pane = await openPane({ entrypoint: "board", env: {}, cwd: homedir(), placement: "tab", workspace });
	await herdr(["tab", "rename", pane.tab_id, "spar board"]);
	const fresh = loadState();
	fresh.board_pane = pane.pane_id;
	saveState(fresh);
}

export function boardLines(runs: RunRow[], last: string, now: Date, width: number): string[] {
	const rank = (r: RunRow) => ({ "NEEDS YOU": 0, running: 1 })[flag(r, now)] ?? 2;
	const shown = [...runs].sort((a, b) => rank(a) - rank(b) || Date.parse(b.updated_at) - Date.parse(a.updated_at));
	const out = wrap(`spar board  ${shown.filter((r) => rank(r) === 0).length} need you, ${shown.filter((r) => r.holds_seat).length} running`, width);
	out.push(...wrap(last, width));
	out.push("");
	for (const r of shown) {
		const mark = flag(r, now) || "idle";
		out.push(...wrap(`${mark} ${r.phase} ${r.id} ${r.project_name ?? ""} ${age(r.updated_at, now)} ${firstLine(r.task)}`, width, 4));
	}
	return out;
}

// Startup hooks are one-shot, so this pane is what keeps herdr current: it re-syncs on
// every phase, gate or abandonment in any known project, and on a timer for new projects.
export async function runBoard(config: Config): Promise<void> {
	const pane = process.env.HERDR_PANE_ID;
	if (pane) {
		await herdr(["pane", "report-metadata", pane, "--source", SOURCE, "--token", "spar_board=1"]);
		const state = loadState();
		state.board_pane = pane;
		saveState(state);
	}
	const feeds = new Map<string, Feed>();
	const feedErrors = new Map<string, string>();
	let runs: RunRow[] = [];
	let last = "syncing...";
	let timer: NodeJS.Timeout | undefined;
	let due = 0;
	let running = false;
	let again = false;

	const draw = () => {
		const width = Math.max(20, process.stdout.columns || 80);
		const height = Math.max(3, process.stdout.rows || 24);
		const status = feedErrors.size ? `${last}\nspar events unavailable (${process.env.SPAR_BIN || "spar"} too old?): ${[...feedErrors.values()][0]}` : last;
		process.stdout.write(`\x1b[H\x1b[2J${boardLines(runs, status, new Date(), width).slice(0, height).join("\n")}`);
	};
	const schedule = (ms: number) => {
		const at = Date.now() + ms;
		if (timer && at >= due) return;
		if (timer) clearTimeout(timer);
		due = at;
		timer = setTimeout(() => {
			timer = undefined;
			void cycle();
		}, ms);
	};
	const cycle = async () => {
		if (running) {
			again = true;
			return;
		}
		running = true;
		try {
			const result: SyncResult | undefined = await withLock(() => syncOnce(config));
			if (result) {
				const now = new Date();
				runs = result.runs.filter((r) => relevant(r, now, config.recent_hours * 3_600_000));
				for (const root of new Set(result.runs.map((r) => r.project_root!))) {
					if (feeds.has(root)) continue;
					const feed = new Feed({
						cwd: root,
						onEvent: (e) => {
							feedErrors.delete(root);
							if (["phase", "gate", "abandoned", "archived"].includes(e.type)) schedule(2_000);
						},
						onError: (message) => {
							feedErrors.set(root, message);
							draw();
						},
					});
					feeds.set(root, feed);
					void feed.start();
				}
			}
			last = `${new Date().toLocaleTimeString()} ${describeSync(result)}`;
		} catch (error) {
			last = `${new Date().toLocaleTimeString()} sync failed: ${(error as Error).message}`;
		}
		draw();
		running = false;
		if (again) {
			again = false;
			schedule(0);
		} else schedule(config.sync_interval_secs * 1000);
	};
	const quit = () => {
		for (const feed of feeds.values()) feed.stop();
		process.stdout.write("\x1b[?1049l");
		process.exit(0);
	};
	for (const sig of ["SIGHUP", "SIGTERM", "SIGINT"] as const) process.on(sig, quit);
	process.stdout.on("error", quit);
	process.stdout.on("resize", draw);
	process.stdout.write("\x1b[?1049h");
	draw();
	await cycle();
}
