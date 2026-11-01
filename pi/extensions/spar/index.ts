import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import type { AutocompleteItem } from "@earendil-works/pi-tui";
import { Footer } from "./footer.ts";
import { detailLines, listRows, listText, plain, sortRuns } from "./format.ts";
import { type Brief, type BusMessage, isRunId, isStale, type RunRow, type RunStatus, sparJson } from "./spar.ts";
import { pickRun, showDetail, themePaint } from "./ui.ts";

const STATUS_KEY = "spar";

export default function (pi: ExtensionAPI) {
	let footer: Footer | undefined;
	let cwd = process.cwd();

	pi.on("session_start", async (_event, ctx) => {
		footer?.stop();
		footer = undefined;
		cwd = ctx.cwd;
		if (!ctx.hasUI) return;
		const current = new Footer({
			cwd: ctx.cwd,
			paint: themePaint(ctx.ui.theme),
			setStatus: (text) => ctx.ui.setStatus(STATUS_KEY, text),
		});
		footer = current;
		void current.start();
	});

	pi.on("session_shutdown", async () => {
		footer?.stop();
		footer = undefined;
	});

	pi.registerCommand("spar", {
		description: "spar runs: /spar lists them, /spar <id> shows one",
		getArgumentCompletions: async (prefix) => {
			const runs = footer?.runs ?? (await listRuns(cwd)) ?? [];
			const now = new Date();
			const items: AutocompleteItem[] = sortRuns(runs, now)
				.filter((r) => r.id.startsWith(prefix.trim()))
				.map((r) => {
					const flag = r.needs_human ? (isStale(r, now) ? ", needs you (stale)" : ", needs you") : "";
					return { value: r.id, label: r.id, description: `${r.phase}${flag}` };
				});
			return items.length ? items : null;
		},
		handler: async (args, ctx) => {
			const id = args.trim().split(/\s+/)[0] ?? "";
			if (id && !isRunId(id)) {
				const message = `spar: "${id}" is not a run id. Usage: /spar [run-id]`;
				if (ctx.mode === "tui") ctx.ui.notify(message, "error");
				else emit(ctx, message);
				return;
			}
			if (ctx.mode !== "tui") {
				emit(ctx, id ? await detailText(ctx.cwd, id) : await runListText(ctx.cwd));
				return;
			}
			let next: string | undefined = id || (await chooseRun(ctx));
			while (next) {
				const detail = await loadDetail(ctx.cwd, next);
				if (typeof detail === "string") {
					ctx.ui.notify(detail, "error");
					return;
				}
				const choice = await showDetail(ctx, (paint) => detailLines({ ...detail, now: new Date() }, paint));
				next = choice === "back" ? await chooseRun(ctx) : undefined;
			}
			void footer?.refresh();
		},
	});
}

async function listRuns(cwd: string): Promise<RunRow[] | undefined> {
	const runs = await sparJson<RunRow[]>(["status", "--json"], cwd);
	return runs.ok ? runs.value : undefined;
}

async function chooseRun(ctx: ExtensionContext): Promise<string | undefined> {
	const runs = await sparJson<RunRow[]>(["status", "--json"], ctx.cwd);
	if (!runs.ok) {
		ctx.ui.notify(failure(runs.missing, runs.message), "error");
		return undefined;
	}
	if (runs.value.length === 0) {
		ctx.ui.notify("spar: no runs in this project", "info");
		return undefined;
	}
	const now = new Date();
	const waiting = runs.value.filter((r) => r.needs_human && !isStale(r, now)).length;
	const stale = runs.value.filter((r) => r.needs_human && isStale(r, now)).length;
	return pickRun(ctx, listRows(runs.value, now), waiting, stale);
}

async function runListText(cwd: string): Promise<string> {
	const runs = await sparJson<RunRow[]>(["status", "--json"], cwd);
	if (!runs.ok) return failure(runs.missing, runs.message);
	return listText(runs.value, new Date());
}

interface Detail {
	status: RunStatus;
	brief: Brief | undefined;
	bus: BusMessage[] | undefined;
}

async function loadDetail(cwd: string, id: string): Promise<Detail | string> {
	const [status, brief, bus] = await Promise.all([
		sparJson<RunStatus>(["status", id, "--json"], cwd),
		sparJson<Brief>(["brief", id, "--json"], cwd),
		sparJson<BusMessage[]>(["bus", "log", "--run", id, "--json"], cwd),
	]);
	if (!status.ok) return failure(status.missing, status.message);
	return {
		status: status.value,
		brief: brief.ok ? brief.value : undefined,
		bus: bus.ok ? bus.value : undefined,
	};
}

async function detailText(cwd: string, id: string): Promise<string> {
	const detail = await loadDetail(cwd, id);
	if (typeof detail === "string") return detail;
	return detailLines({ ...detail, now: new Date() }, plain).join("\n");
}

function failure(missing: boolean, message: string): string {
	return missing ? "spar: the spar binary is not on PATH" : `spar: ${message}`;
}

function emit(ctx: ExtensionContext, text: string): void {
	if (ctx.mode === "print") process.stdout.write(`${text}\n`);
	else if (ctx.hasUI) ctx.ui.notify(text, "info");
	else process.stderr.write(`${text}\n`);
}
