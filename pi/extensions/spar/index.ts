import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import type { AutocompleteItem } from "@earendil-works/pi-tui";
import { AttentionTracker } from "./attention.ts";
import { Footer } from "./footer.ts";
import { detailLines, listRows, listText, plain, sortRuns } from "./format.ts";
import { actionsFor, isVerb, performAction, report, setReporter, VERBS, verbCommand, verbCompletions } from "./gates.ts";
import { type Brief, type BusMessage, isRunId, isStale, needsYou, type RunRow, type RunStatus, sparJson } from "./spar.ts";
import { pickRun, showDetail, themePaint } from "./ui.ts";

const STATUS_KEY = "spar";

export default function (pi: ExtensionAPI) {
	let footer: Footer | undefined;
	let cwd = process.cwd();

	pi.on("session_start", async (_event, ctx) => {
		footer?.stop();
		footer = undefined;
		cwd = ctx.cwd;
		setReporter(undefined);
		if (!ctx.hasUI) return;
		setReporter((text, level) => ctx.ui.notify(text, level));
		const attention = new AttentionTracker();
		const current = new Footer({
			cwd: ctx.cwd,
			paint: themePaint(ctx.ui.theme),
			setStatus: (text) => ctx.ui.setStatus(STATUS_KEY, text),
			onFirstRuns: (runs, feedStart) => attention.seed(runs, feedStart),
			onEvent: (event) => {
				const alert = attention.onEvent(event);
				if (alert) report(alert.message, alert.level);
			},
			onFatal: (message) => report(message, "warning"),
		});
		footer = current;
		current.start().catch(() => {});
	});

	pi.on("session_shutdown", async () => {
		setReporter(undefined);
		footer?.stop();
		footer = undefined;
	});

	pi.registerCommand("spar", {
		description: "spar runs: /spar lists them, /spar <id> shows one, /spar <verb> <id> acts on one",
		getArgumentCompletions: async (prefix) => {
			const runs = footer?.runs ?? (await listRuns(cwd)) ?? [];
			const gated = verbCompletions(prefix, runs);
			if (gated && /\s/.test(prefix)) return gated.length ? gated : null;
			const now = new Date();
			const items: AutocompleteItem[] = sortRuns(runs, now)
				.filter((r) => r.id.startsWith(prefix.trim()))
				.map((r) => {
					const flag = needsYou(r) ? (isStale(r, now) ? ", needs you (stale)" : ", needs you") : "";
					return { value: r.id, label: r.id, description: `${r.phase}${flag}` };
				});
			items.push(...(gated ?? []));
			return items.length ? items : null;
		},
		handler: async (args, ctx) => {
			const id = args.trim().split(/\s+/)[0] ?? "";
			if (isVerb(id)) {
				const result = await verbCommand(ctx, id, args.trim().slice(id.length));
				if (result?.kind === "started") afterJob(result.done);
				refreshFooter();
				return;
			}
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
				const actions = actionsFor(detail.status).map((v) => ({ key: VERBS[v].key, label: VERBS[v].label, id: v }));
				const choice = await showDetail(ctx, (paint) => detailLines({ ...detail, now: new Date() }, paint), actions);
				if (typeof choice === "object") {
					if (isVerb(choice.action)) {
						const result = await performAction(ctx, choice.action, detail.status);
						if (result.kind === "started") afterJob(result.done);
					}
					continue;
				}
				next = choice === "back" ? await chooseRun(ctx) : undefined;
			}
			refreshFooter();
		},
	});

	function refreshFooter(): void {
		footer?.refresh().catch(() => {});
	}

	function afterJob(done: Promise<unknown>): void {
		done.then(refreshFooter, () => {});
	}
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
	const waiting = runs.value.filter((r) => needsYou(r) && !isStale(r, now)).length;
	const stale = runs.value.filter((r) => needsYou(r) && isStale(r, now)).length;
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
