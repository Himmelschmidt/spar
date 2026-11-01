import type { Brief, BusMessage, CostLine, Finding, Provider, RunRow, RunStatus, SparEvent } from "./spar.ts";

export type Tone = "accent" | "warning" | "error" | "success" | "muted" | "dim";
export type Paint = (tone: Tone, text: string) => string;
export const plain: Paint = (_tone, text) => text;

export function isPaused(provider: Provider): boolean {
	return provider.quota_status !== "available";
}

export interface FooterState {
	runs: RunRow[];
	paused: Provider[];
	costToday: number | undefined;
}

export function footerText(state: FooterState, paint: Paint): string {
	const running = state.runs.filter((r) => r.holds_seat).length;
	const waiting = state.runs.filter((r) => r.needs_human).length;
	const parts = [paint("accent", "spar"), `${running} running`];
	if (waiting > 0) parts.push(paint("warning", `${waiting} need you`));
	if (state.paused.length > 0) parts.push(paint("error", `paused ${state.paused.map((p) => p.name).join(",")}`));
	if (state.costToday !== undefined) parts.push(paint("dim", `${money(state.costToday)} today`));
	return parts.join(paint("dim", " | "));
}

export function sortRuns<T extends RunRow>(runs: T[]): T[] {
	const rank = (r: RunRow) => (r.needs_human ? 0 : r.holds_seat ? 1 : 2);
	return [...runs].sort((a, b) => rank(a) - rank(b) || Date.parse(b.updated_at) - Date.parse(a.updated_at));
}

export function money(usd: number): string {
	return `$${usd.toFixed(2)}`;
}

export function costLabel(cost: CostLine | undefined): string {
	if (!cost || cost.cost_usd === null || cost.dispatches === 0) return "-";
	return cost.cost_reported === "all" ? money(cost.cost_usd) : `${money(cost.cost_usd)}+`;
}

export function age(iso: string, now: Date): string {
	const secs = Math.max(0, Math.floor((now.getTime() - Date.parse(iso)) / 1000));
	if (secs < 60) return `${secs}s`;
	if (secs < 3600) return `${Math.floor(secs / 60)}m`;
	if (secs < 86_400) return `${Math.floor(secs / 3600)}h`;
	return `${Math.floor(secs / 86_400)}d`;
}

function stamp(iso: string): string {
	const d = new Date(iso);
	const pad = (n: number) => String(n).padStart(2, "0");
	return `${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

function firstLine(text: string | null | undefined): string {
	return (text ?? "").split("\n").find((l) => l.trim() !== "")?.trim() ?? "";
}

export interface ListRow {
	id: string;
	label: string;
	description: string;
}

export function listRows(runs: RunRow[], costs: Map<string, CostLine | undefined>, now: Date): ListRow[] {
	const sorted = sortRuns(runs);
	const phaseWidth = Math.max(5, ...sorted.map((r) => r.phase.length));
	const workflowWidth = Math.max(8, ...sorted.map((r) => r.workflow.length));
	return sorted.map((r) => {
		const flag = r.needs_human ? "NEEDS YOU" : r.holds_seat ? "running" : "";
		const label = [
			r.id,
			r.phase.padEnd(phaseWidth),
			`r${r.round}`.padEnd(4),
			age(r.updated_at, now).padStart(4),
			flag.padEnd(9),
			costLabel(costs.get(r.id)).padStart(9),
			r.workflow.padEnd(workflowWidth),
		].join("  ");
		return { id: r.id, label, description: firstLine(r.task) };
	});
}

export function listText(runs: RunRow[], costs: Map<string, CostLine | undefined>, now: Date): string {
	if (runs.length === 0) return "spar: no runs in this project";
	return listRows(runs, costs, now)
		.map((r) => r.label.trimEnd())
		.join("\n");
}

export function describeEvent(e: SparEvent): string {
	switch (e.type) {
		case "phase":
			return `phase ${e.prev_phase ? `${e.prev_phase} -> ` : ""}${e.phase ?? "?"}${e.round !== undefined ? ` (round ${e.round})` : ""}`;
		case "slot":
			return `slot ${e.slot ?? "?"} ${e.status ?? ""}${e.message ? `: ${firstLine(e.message)}` : ""}`.trimEnd();
		case "gate":
			return `gate ${e.phase ?? ""} ${firstLine(e.message)}`.replace(/\s+/g, " ").trim();
		default:
			return `${e.type}${e.slot ? ` ${e.slot}` : ""}: ${firstLine(e.message)}`;
	}
}

const SEVERITIES: Finding["severity"][] = ["critical", "major", "minor"];
const SEVERITY_TONE: Record<Finding["severity"], Tone> = { critical: "error", major: "warning", minor: "muted" };

function seatFlags(slot: RunStatus["slots"][number]): string {
	const flags: string[] = [];
	if (slot.pid_alive) flags.push("alive");
	if (slot.stalled) flags.push("stalled");
	if (slot.quota_hit) flags.push("quota");
	if (slot.auth_fault) flags.push("auth");
	return flags.length ? ` [${flags.join(",")}]` : "";
}

export interface DetailInput {
	status: RunStatus;
	brief: Brief | undefined;
	bus: BusMessage[] | undefined;
	now: Date;
}

export function detailLines({ status, brief, bus, now }: DetailInput, paint: Paint): string[] {
	const lines: string[] = [];
	const heading = (t: string) => {
		lines.push("");
		lines.push(paint("accent", t));
	};
	const maxRounds = status.max_rounds ?? brief?.max_rounds;
	const flag = status.needs_human ? paint("warning", "  NEEDS YOU") : status.holds_seat ? paint("success", "  running") : "";
	lines.push(
		`${paint("accent", `run ${status.id}`)}  ${status.workflow}  phase ${status.phase}  round ${status.round}${maxRounds ? `/${maxRounds}` : ""}${flag}`,
	);
	lines.push(paint("dim", `updated ${age(status.updated_at, now)} ago${status.created_at ? `, created ${stamp(status.created_at)}` : ""}`));
	const task = firstLine(status.task);
	if (task) lines.push(`task: ${task}`);
	if (status.error) lines.push(paint("error", `error: ${firstLine(status.error)}`));

	const total = status.cost?.total;
	if (total && total.dispatches > 0) {
		const partial =
			total.cost_reported === "all" ? "" : ` (${total.dispatches_without_cost} of ${total.dispatches} dispatches report no cost)`;
		const byProvider = (status.cost?.by_provider ?? [])
			.map((p) => `${p.key} ${costLabel(p)}`)
			.join(", ");
		lines.push(`cost: ${costLabel(total)}${partial}${byProvider ? `  ${paint("dim", byProvider)}` : ""}`);
	}

	heading("seats");
	if (status.slots.length === 0) lines.push(paint("dim", "  none dispatched yet"));
	const roleWidth = Math.max(4, ...status.slots.map((s) => s.role.length));
	const idWidth = Math.max(4, ...status.slots.map((s) => s.id.length));
	const seatProvider = (s: RunStatus["slots"][number]) =>
		s.model && !s.provider.includes("@") ? `${s.provider}@${s.model}` : s.provider;
	const providerWidth = Math.max(8, ...status.slots.map((s) => seatProvider(s).length));
	for (const s of status.slots) {
		const tone: Tone = s.status === "running" ? "success" : s.status === "failed" ? "error" : "muted";
		lines.push(
			`  ${s.role.padEnd(roleWidth)}  ${s.id.padEnd(idWidth)}  ${seatProvider(s).padEnd(providerWidth)}  ${paint(tone, s.status)}  r${s.round}${seatFlags(s)}`,
		);
	}
	if (brief?.fleet?.length) lines.push(paint("dim", `  fleet: ${brief.fleet.join("  ")}`));

	const findings = brief?.open_findings ?? [];
	heading(`findings (last review panel, ${findings.length} open)`);
	if (findings.length === 0) lines.push(paint("dim", "  none"));
	for (const severity of SEVERITIES) {
		const group = findings.filter((f) => f.severity === severity);
		if (group.length === 0) continue;
		lines.push(`  ${paint(SEVERITY_TONE[severity], `${severity} (${group.length})`)}`);
		for (const f of group) {
			const where = f.file ? `${f.file}${f.line ? `:${f.line}` : ""} ` : "";
			const notes = [f.slot_id, f.follow_up ? "follow-up" : "", f.possibly_addressed ? "possibly addressed" : ""]
				.filter(Boolean)
				.join(", ");
			const [head, ...rest] = f.text.split("\n").filter((l) => l.trim() !== "");
			lines.push(`    - ${where}${head ?? ""} ${paint("dim", `(${notes})`)}`);
			for (const more of rest) lines.push(`      ${more.trim()}`);
		}
	}

	const events = (brief?.recent_events ?? []).slice(-10);
	heading("recent events");
	if (events.length === 0) lines.push(paint("dim", "  none"));
	for (const e of events) lines.push(`  ${paint("dim", stamp(e.ts))}  ${describeEvent(e)}`);

	const messages = (bus ?? []).slice(-8);
	heading("bus");
	if (messages.length === 0) lines.push(paint("dim", "  none"));
	for (const m of messages) {
		lines.push(`  ${paint("dim", stamp(m.ts))}  ${m.from} -> ${m.to}: ${firstLine(m.body)}`);
	}

	lines.push("");
	lines.push(`${paint("accent", "next:")} ${brief?.next_command ?? `spar brief ${status.id}`}`);
	return lines;
}
