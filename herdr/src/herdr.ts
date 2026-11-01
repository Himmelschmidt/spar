import { execFile } from "node:child_process";

export function herdrBin(): string {
	return process.env.HERDR_BIN_PATH || "herdr";
}

export const PLUGIN_ID = "spar.monitor";
export const SOURCE = "custom:spar";
export const AGENT = "spar";

export type HerdrResult = { ok: true; result: Record<string, unknown> } | { ok: false; message: string };

export function herdr(args: string[]): Promise<HerdrResult> {
	return new Promise((resolve) => {
		execFile(herdrBin(), args, { timeout: 15_000, maxBuffer: 64 * 1024 * 1024 }, (error, stdout, stderr) => {
			if (error) {
				resolve({ ok: false, message: (stderr || error.message).trim().split("\n")[0] ?? "" });
				return;
			}
			try {
				const parsed = JSON.parse(stdout) as { result?: Record<string, unknown> };
				resolve({ ok: true, result: parsed.result ?? {} });
			} catch {
				resolve({ ok: true, result: {} });
			}
		});
	});
}

export interface PaneInfo {
	pane_id: string;
	tab_id: string;
	workspace_id: string;
	cwd?: string;
	label?: string;
	agent?: string;
	agent_status?: string;
	focused?: boolean;
	tokens?: Record<string, string>;
}

export interface TabInfo {
	tab_id: string;
	workspace_id: string;
	label: string;
}

export interface WorkspaceInfo {
	workspace_id: string;
	label: string;
}

export interface Rect {
	width: number;
	height: number;
}

export interface Layout {
	tab_id: string;
	panes: { pane_id: string; rect: Rect }[];
}

export interface Snapshot {
	workspaces: WorkspaceInfo[];
	tabs: TabInfo[];
	panes: PaneInfo[];
	layouts: Layout[];
	focused_workspace_id?: string;
}

export async function snapshot(): Promise<Snapshot> {
	const res = await herdr(["api", "snapshot"]);
	if (!res.ok) throw new Error(`herdr api snapshot: ${res.message}`);
	const snap = (res.result.snapshot ?? {}) as Partial<Snapshot>;
	return {
		workspaces: snap.workspaces ?? [],
		tabs: snap.tabs ?? [],
		panes: snap.panes ?? [],
		layouts: snap.layouts ?? [],
		focused_workspace_id: snap.focused_workspace_id,
	};
}

export interface PaneOpen {
	entrypoint: string;
	env: Record<string, string>;
	cwd: string;
	placement?: "tab" | "split";
	workspace?: string;
	target?: string;
	direction?: "right" | "down";
}

export async function openPane(open: PaneOpen): Promise<PaneInfo> {
	const args = ["plugin", "pane", "open", "--plugin", PLUGIN_ID, "--entrypoint", open.entrypoint, "--cwd", open.cwd, "--no-focus"];
	if (open.placement) args.push("--placement", open.placement);
	if (open.workspace) args.push("--workspace", open.workspace);
	if (open.target) args.push("--target-pane", open.target);
	if (open.direction) args.push("--direction", open.direction);
	for (const [k, v] of Object.entries(open.env)) args.push("--env", `${k}=${v}`);
	const res = await herdr(args);
	if (!res.ok) throw new Error(`herdr plugin pane open: ${res.message}`);
	const pane = (res.result.plugin_pane as { pane?: PaneInfo } | undefined)?.pane;
	if (!pane) throw new Error("herdr plugin pane open: no pane in the response");
	return pane;
}

export async function layoutOf(paneId: string): Promise<Layout | undefined> {
	const res = await herdr(["pane", "layout", "--pane", paneId]);
	return res.ok ? (res.result.layout as Layout | undefined) : undefined;
}

// The pane to split next: the biggest in the tab, cut along its longer side. A cell is
// about twice as tall as it is wide, hence the factor.
export function splitTarget(layout: Layout | undefined): { target: string; direction: "right" | "down" } | undefined {
	if (!layout || layout.panes.length === 0) return undefined;
	const biggest = layout.panes.reduce((a, b) => (b.rect.width * b.rect.height > a.rect.width * a.rect.height ? b : a));
	return { target: biggest.pane_id, direction: biggest.rect.width >= biggest.rect.height * 2 ? "right" : "down" };
}

export async function notify(title: string, body: string): Promise<void> {
	await herdr(["notification", "show", title, "--body", body]);
}

export interface PluginContext {
	workspace_id?: string;
	focused_pane_id?: string;
}

export function pluginContext(): PluginContext {
	try {
		return JSON.parse(process.env.HERDR_PLUGIN_CONTEXT_JSON ?? "{}") as PluginContext;
	} catch {
		return {};
	}
}
