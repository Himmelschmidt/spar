import { Feed } from "./feed.ts";
import { actionsFor, isGate } from "./gates.ts";
import { AGENT, herdr, SOURCE } from "./herdr.ts";
import { type AgentState, agentState, type Brief, firstLine, flag, type RunStatus, type SparEvent, sparJson } from "./spar.ts";
import { wrap } from "./wrap.ts";

export type Tone = "head" | "warn" | "dim" | "plain";
export interface Line {
	text: string;
	tone: Tone;
}

export function age(iso: string, now: Date): string {
	const secs = Math.max(0, Math.floor((now.getTime() - Date.parse(iso)) / 1000));
	if (secs < 60) return `${secs}s`;
	if (secs < 3600) return `${Math.floor(secs / 60)}m`;
	if (secs < 86_400) return `${Math.floor(secs / 3600)}h`;
	return `${Math.floor(secs / 86_400)}d`;
}

function clock(iso: string): string {
	const d = new Date(iso);
	return `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
}

export function describeEvent(e: SparEvent): string {
	switch (e.type) {
		case "phase":
			return `phase ${e.prev_phase ? `${e.prev_phase} -> ` : ""}${e.phase ?? "?"}${e.round !== undefined ? ` (round ${e.round})` : ""}`;
		case "slot":
			return `slot ${e.slot ?? "?"} ${e.status ?? ""}${e.message ? `: ${firstLine(e.message)}` : ""}`.trimEnd();
		case "gate":
			return `gate ${e.phase ?? ""} ${firstLine(e.message)}`.replace(/\s+/g, " ").trim();
		case "abandoned":
			return `abandoned${e.phase ? ` in ${e.phase}` : ""}`;
		default:
			return `${e.type}${e.slot ? ` ${e.slot}` : ""}${e.message ? `: ${firstLine(e.message)}` : ""}`;
	}
}

function seatProvider(s: RunStatus["slots"][number]): string {
	return s.model && !s.provider.includes("@") ? `${s.provider}@${s.model}` : s.provider;
}

function seatFlags(s: RunStatus["slots"][number]): string {
	const flags = [s.pid_alive ? "alive" : "", s.stalled ? "STALLED" : "", s.quota_hit ? "QUOTA" : "", s.auth_fault ? "AUTH" : ""].filter(Boolean);
	return flags.length ? ` [${flags.join(",")}]` : "";
}

export interface ViewInput {
	status: RunStatus;
	brief?: Brief;
	now: Date;
	width: number;
	height?: number;
	error?: string;
}

function build(input: ViewInput, eventCount: number, withTask: boolean): Line[] {
	const { status, brief, now, width } = input;
	const out: Line[] = [];
	const add = (text: string, tone: Tone = "plain", indent = 2) => {
		for (const l of wrap(text, width, indent)) out.push({ text: l, tone });
	};
	const mark = flag(status, now);
	add(`spar ${status.id}${mark ? `  ${mark}` : ""}`, "head");
	const maxRounds = status.max_rounds ?? brief?.max_rounds;
	add(`${status.phase} | round ${status.round}${maxRounds ? `/${maxRounds}` : ""} | ${status.workflow} | ${age(status.updated_at, now)} ago`);
	const task = firstLine(status.task);
	if (task && withTask) add(`task: ${task}`, "dim");
	const error = firstLine(status.error ?? brief?.error);
	if (error) add(`error: ${error}`, "warn");
	if (isGate(status.phase) || mark === "NEEDS YOU") {
		add(`gate: ${status.phase}; can ${actionsFor(status).join(", ")}`, "warn");
	}
	if (input.error) add(`! ${input.error}`, "warn");

	add(`seats (${status.slots.length})`, "head");
	for (const s of status.slots) add(`  ${s.status}${seatFlags(s)} ${s.id} r${s.round} ${seatProvider(s)}`, "plain", 4);

	const findings = brief?.open_findings ?? [];
	if (findings.length > 0) {
		const counts = (["critical", "major", "minor"] as const)
			.map((sev) => [sev, findings.filter((f) => f.severity === sev).length] as const)
			.filter(([, n]) => n > 0)
			.map(([sev, n]) => `${n} ${sev}`);
		add(`findings: ${findings.length} open (${counts.join(", ")})`, findings.some((f) => f.severity === "critical") ? "warn" : "plain");
	} else if (brief) add("findings: none open", "dim");

	const events = (brief?.recent_events ?? []).slice(-eventCount);
	if (eventCount > 0 && events.length > 0) {
		add("recent", "head");
		for (const e of events) add(`  ${clock(e.ts)} ${describeEvent(e)}`, "dim", 8);
	}
	add(`next: ${brief?.next_command ?? `spar brief ${status.id}`}`, "plain", 6);
	return out;
}

const FITS: [number, boolean][] = [
	[5, true],
	[3, true],
	[1, true],
	[0, true],
	[0, false],
];

// Narrow panes wrap instead of truncating; a short pane drops recent events, then the
// task, then cuts the middle so the header and the next command always show.
export function renderView(input: ViewInput): Line[] {
	const height = input.height ?? Number.POSITIVE_INFINITY;
	let lines: Line[] = [];
	for (const [events, task] of FITS) {
		lines = build(input, events, task);
		if (lines.length <= height) return lines;
	}
	const next = lines.findIndex((l) => l.text.startsWith("next: "));
	const tail = lines.slice(next);
	return [...lines.slice(0, Math.max(1, height - tail.length)), ...tail].slice(0, Math.max(1, height));
}

const TONES: Record<Tone, [string, string]> = {
	head: ["\x1b[1m", "\x1b[22m"],
	warn: ["\x1b[33m", "\x1b[39m"],
	dim: ["\x1b[2m", "\x1b[22m"],
	plain: ["", ""],
};

export function paint(lines: Line[]): string {
	return lines.map((l) => `${TONES[l.tone][0]}${l.text}${TONES[l.tone][1]}`).join("\n");
}

export interface Reported {
	state?: AgentState;
	phase?: string;
}

export async function report(pane: string, status: RunStatus, root: string, now: Date, last: Reported): Promise<void> {
	const state = agentState(status, now);
	if (state === last.state && status.phase === last.phase) return;
	const agent = await herdr(["pane", "report-agent", pane, "--source", SOURCE, "--agent", AGENT, "--state", state, "--message", status.phase]);
	const meta = await herdr([
		"pane",
		"report-metadata",
		pane,
		"--source",
		SOURCE,
		"--display-agent",
		`spar ${status.id.slice(0, 8)} ${status.workflow}`,
		"--title",
		`spar ${status.id}`,
		"--state-label",
		"blocked=needs you",
		"--token",
		`summary=${status.phase}`,
		"--token",
		`spar_run=${status.id}`,
		"--token",
		`spar_root=${root}`,
	]);
	if (agent.ok && meta.ok) {
		last.state = state;
		last.phase = status.phase;
	}
}

const REFRESH_MS = 30_000;
const MISSING_CONFIRMATIONS = 2;

// The long-lived pane for one run. Exits (closing its pane) once the run is archived or
// gone; stays up through spar errors, retrying with backoff.
export async function runView(id: string, root: string): Promise<void> {
	const pane = process.env.HERDR_PANE_ID;
	const reported: Reported = {};
	let status: RunStatus | undefined;
	let brief: Brief | undefined;
	let error: string | undefined;
	let missing = 0;
	let backoffMs = 2_000;
	let timer: NodeJS.Timeout | undefined;
	let due = 0;
	let running = false;
	let again = false;
	let finished = false;

	const draw = () => {
		const width = Math.max(20, process.stdout.columns || 80);
		const height = Math.max(3, process.stdout.rows || 24);
		const body = status
			? paint(renderView({ status, brief, now: new Date(), width, height, error }))
			: wrap(error ? `spar ${id}: ${error}` : `spar ${id}: loading`, width).join("\n");
		process.stdout.write(`\x1b[H\x1b[2J${body}`);
	};

	const feed = new Feed({ cwd: root, run: id, onEvent: () => schedule(300) });

	const finish = (code: number) => {
		if (finished) return;
		finished = true;
		feed.stop();
		if (timer) clearTimeout(timer);
		process.stdout.write("\x1b[?25h\x1b[?1049l");
		process.exit(code);
	};

	// Never pushes a pending refresh later, so a run emitting events back to back still
	// gets refreshed.
	const schedule = (ms: number) => {
		if (finished) return;
		const at = Date.now() + ms;
		if (timer && at >= due) return;
		if (timer) clearTimeout(timer);
		due = at;
		timer = setTimeout(() => {
			timer = undefined;
			void refresh();
		}, ms);
	};

	const refresh = async () => {
		if (running) {
			again = true;
			return;
		}
		running = true;
		const next = await sparJson<RunStatus>(["status", id, "--json"], root);
		if (next.ok) {
			missing = 0;
			backoffMs = 2_000;
			error = undefined;
			status = next.value;
			if (status.archived_at) return finish(0);
			const b = await sparJson<Brief>(["brief", id, "--json"], root);
			if (b.ok) brief = b.value;
			if (pane) await report(pane, status, root, new Date(), reported);
		} else if (next.notFound && ++missing >= MISSING_CONFIRMATIONS) {
			return finish(0);
		} else {
			error = next.missing ? `spar binary not found (${process.env.SPAR_BIN || "spar"}); retrying` : next.message;
		}
		draw();
		running = false;
		if (again) {
			again = false;
			schedule(0);
		} else if (error) {
			schedule(backoffMs);
			backoffMs = Math.min(backoffMs * 2, 60_000);
		} else schedule(REFRESH_MS);
	};

	for (const sig of ["SIGHUP", "SIGTERM", "SIGINT"] as const) process.on(sig, () => finish(0));
	process.stdout.on("error", () => finish(0));
	process.stdout.on("resize", draw);
	process.stdout.write("\x1b[?1049h\x1b[?25l");
	draw();
	await refresh();
	await feed.start();
}
