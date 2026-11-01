import type { Theme } from "@earendil-works/pi-coding-agent";
import { type Component, Text, truncateToWidth } from "@earendil-works/pi-tui";
import { age, costLabel, describeEvent, listRows, money, type Paint, plain, runFlag, type Tone } from "./format.ts";
import type { Brief, BusMessage, Finding, Provider, RunRow, RunStatus, SparEvent } from "./spar.ts";

export interface FleetSeat {
	seat: string;
	role: string;
	provider: string;
	model?: string | null;
	source?: string;
	projected?: boolean;
}

export interface RunDetail extends RunStatus {
	fleet?: FleetSeat[] | null;
}

export interface BriefDetail extends Brief {
	phase?: string;
	round?: number;
	workflow?: string;
	error?: string | null;
	run_exit_code?: number;
}

export interface StatsGroup {
	key: string;
	runs?: number;
	dispatches?: number;
	outcomes?: Record<string, number>;
	usage: { cost_usd: number | null; billed_tokens?: number };
}

export interface StatsDetail {
	since?: string | null;
	by?: string;
	runs?: number;
	groups: StatsGroup[];
	run_level_only?: { runs?: number; cost_usd: number | null };
}

export interface Outcome {
	run_id: string | null;
	phase: string | null;
	exit_code: number | null;
	meaning: string;
	error?: string | null;
	dry_run?: boolean;
	fleet?: FleetSeat[];
	next_command?: string;
	gates_off?: string[];
}

export type ToolDetails =
	| { kind: "runs"; runs: RunRow[] }
	| { kind: "run"; run: RunDetail }
	| { kind: "brief"; brief: BriefDetail }
	| { kind: "events"; run: string; events: SparEvent[]; total: number }
	| { kind: "logs"; run: string; slot?: string; lines: string[]; total: number }
	| { kind: "providers"; providers: Provider[] }
	| { kind: "stats"; stats: StatsDetail }
	| { kind: "launch"; command: string; outcome?: Outcome; refused?: string; declined?: string }
	| { kind: "wait"; outcome: Outcome; waitedTimeout: boolean }
	| { kind: "bus"; messages: BusMessage[]; total: number }
	| { kind: "bus_sent"; id?: string; to: string; run?: string }
	| { kind: "skill"; section?: string; text: string }
	| { kind: "error"; message: string; exit_code?: number | null };

const SEVERITIES: Finding["severity"][] = ["critical", "major", "minor"];
const SEVERITY_TONE: Record<Finding["severity"], Tone> = { critical: "error", major: "warning", minor: "muted" };

function first(text: string | null | undefined): string {
	return (text ?? "").split("\n").find((l) => l.trim() !== "")?.trim() ?? "";
}

function stamp(iso: string): string {
	const d = new Date(iso);
	const pad = (n: number) => String(n).padStart(2, "0");
	return `${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

function table(rows: string[][]): string[] {
	const widths: number[] = [];
	for (const row of rows) row.forEach((cell, i) => (widths[i] = Math.max(widths[i] ?? 0, cell.length)));
	return rows.map((row) => row.map((cell, i) => (i === row.length - 1 ? cell : cell.padEnd(widths[i]!))).join("  ").trimEnd());
}

function seatLabel(provider: string, model: string | null | undefined): string {
	return model && !provider.includes("@") ? `${provider}@${model}` : provider;
}

export function fleetLines(fleet: FleetSeat[], paint: Paint): string[] {
	return table(
		fleet.map((s) => [s.role, s.projected ? "projected" : "now", seatLabel(s.provider, s.model), s.seat, s.source ?? ""]),
	).map((l) => `  ${paint("muted", l)}`);
}

export function findingLines(findings: Finding[], paint: Paint): string[] {
	if (findings.length === 0) return [paint("dim", "  none")];
	const lines: string[] = [];
	for (const severity of SEVERITIES) {
		const group = findings.filter((f) => f.severity === severity);
		if (group.length === 0) continue;
		lines.push(`  ${paint(SEVERITY_TONE[severity], `${severity} (${group.length})`)}`);
		for (const f of group) {
			const where = f.file ? `${f.file}${f.line ? `:${f.line}` : ""} ` : "";
			const notes = [f.slot_id, f.follow_up ? "follow-up" : "", f.possibly_addressed ? "possibly addressed" : ""]
				.filter(Boolean)
				.join(", ");
			lines.push(`    - ${where}${first(f.text)} ${paint("dim", `(${notes})`)}`);
		}
	}
	return lines;
}

function runLines(run: RunDetail, paint: Paint, now: Date): string[] {
	const flag = runFlag(run, now);
	const lines = [
		`${paint("accent", `run ${run.id}`)}  ${run.workflow}  phase ${run.phase}  round ${run.round}${run.max_rounds ? `/${run.max_rounds}` : ""}${flag ? `  ${paint(flag === "running" ? "success" : "warning", flag)}` : ""}`,
		paint("dim", `updated ${age(run.updated_at, now)} ago${run.dry_run ? ", dry run" : ""}`),
	];
	const task = first(run.task);
	if (task) lines.push(`task: ${task}`);
	if (run.error) lines.push(paint("error", `error: ${first(run.error)}`));
	const total = run.cost?.total;
	if (total && total.dispatches > 0) lines.push(`cost: ${costLabel(total)}`);
	lines.push(paint("accent", "seats"));
	if (run.slots.length === 0) lines.push(paint("dim", "  none dispatched yet"));
	const rows = run.slots.map((s) => {
		const flags = [s.stalled ? "stalled" : "", s.quota_hit ? "quota" : "", s.auth_fault ? "auth" : ""].filter(Boolean).join(",");
		return [s.role, s.status, `r${s.round}`, seatLabel(s.provider, s.model), `${s.id}${flags ? ` [${flags}]` : ""}`];
	});
	const tones = run.slots.map((s): Tone => (s.status === "running" ? "success" : s.status === "failed" ? "error" : "muted"));
	table(rows).forEach((l, i) => lines.push(`  ${paint(tones[i]!, l)}`));
	const projected = (run.fleet ?? []).filter((s) => s.projected);
	if (projected.length) {
		lines.push(paint("accent", "projected seats"));
		lines.push(...fleetLines(projected, paint));
	}
	return lines;
}

function briefLines(brief: BriefDetail, paint: Paint): string[] {
	const lines = [
		`${paint("accent", `run ${brief.run}`)}${brief.workflow ? `  ${brief.workflow}` : ""}${brief.phase ? `  phase ${brief.phase}` : ""}${brief.round !== undefined ? `  round ${brief.round}${brief.max_rounds ? `/${brief.max_rounds}` : ""}` : ""}`,
	];
	if (brief.error) lines.push(paint("error", `error: ${first(brief.error)}`));
	if (brief.fleet?.length) lines.push(paint("dim", `fleet: ${brief.fleet.join("  ")}`));
	lines.push(paint("accent", `findings (${brief.open_findings.length} open)`));
	lines.push(...findingLines(brief.open_findings, paint));
	const events = brief.recent_events.slice(-5);
	if (events.length) {
		lines.push(paint("accent", "recent events"));
		for (const e of events) lines.push(`  ${paint("dim", stamp(e.ts))}  ${describeEvent(e)}`);
	}
	lines.push(`${paint("accent", "next:")} ${brief.next_command}`);
	return lines;
}

function statsLines(stats: StatsDetail, paint: Paint): string[] {
	const lines = [
		paint("accent", `${stats.runs ?? 0} runs${stats.since ? ` since ${stamp(stats.since)}` : ""}, by ${stats.by ?? "provider"}`),
	];
	const rows = stats.groups.map((g) => {
		const outcomes = Object.entries(g.outcomes ?? {})
			.filter(([, n]) => n > 0)
			.map(([k, n]) => `${k} ${n}`)
			.join(", ");
		return [g.key, `${g.runs ?? 0} runs`, `${g.dispatches ?? 0} dispatches`, g.usage.cost_usd === null ? "-" : money(g.usage.cost_usd), outcomes];
	});
	lines.push(...table(rows).map((l) => `  ${l}`));
	const runLevel = stats.run_level_only?.cost_usd;
	if (runLevel) lines.push(paint("dim", `  run-level cost not split by group: ${money(runLevel)}`));
	return lines;
}

function outcomeLines(outcome: Outcome, paint: Paint): string[] {
	const tone: Tone = outcome.exit_code === 0 ? "success" : outcome.exit_code === 2 ? "warning" : "error";
	const lines = [
		`${paint("accent", outcome.run_id ? `run ${outcome.run_id}` : "no run")}${outcome.phase ? `  phase ${outcome.phase}` : ""}${outcome.dry_run ? paint("dim", "  dry run") : ""}`,
		`${paint(tone, `exit ${outcome.exit_code ?? "?"}`)}: ${outcome.meaning}`,
	];
	if (outcome.error) lines.push(paint("error", `error: ${first(outcome.error)}`));
	if (outcome.gates_off?.length) lines.push(paint("warning", `gates the config passes without the operator: ${outcome.gates_off.join("; ")}`));
	if (outcome.fleet?.length) {
		lines.push(paint("accent", "fleet"));
		lines.push(...fleetLines(outcome.fleet, paint));
	}
	if (outcome.next_command) lines.push(`${paint("accent", "next:")} ${outcome.next_command}`);
	return lines;
}

export function detailLines(details: ToolDetails, paint: Paint, now = new Date()): string[] {
	switch (details.kind) {
		case "runs":
			return details.runs.length === 0
				? ["no runs"]
				: listRows(details.runs, now).map((r) => {
						const task = r.description.length > 100 ? `${r.description.slice(0, 97)}...` : r.description;
						const line = `${r.label.trimEnd()}  ${task}`.trimEnd();
						const run = details.runs.find((x) => x.id === r.id)!;
						return run.needs_human ? paint("warning", line) : run.holds_seat ? paint("success", line) : line;
					});
		case "run":
			return runLines(details.run, paint, now);
		case "brief":
			return briefLines(details.brief, paint);
		case "events": {
			const head = details.total > details.events.length ? [paint("dim", `last ${details.events.length} of ${details.total} events`)] : [];
			if (details.events.length === 0) return [`run ${details.run}: no events`];
			return [...head, ...details.events.map((e) => `${paint("dim", stamp(e.ts))}  ${describeEvent(e)}`)];
		}
		case "logs": {
			const head = paint("dim", `${details.run}${details.slot ? ` ${details.slot}` : ""}: last ${details.lines.length} of ${details.total} lines`);
			return [head, ...details.lines];
		}
		case "providers":
			return table(
				details.providers.map((p) => [
					p.name,
					p.quota_status,
					p.paused_until ? `until ${stamp(p.paused_until)}` : "",
					p.quota_hint ?? "",
				]),
			).map((l, i) => (details.providers[i]!.quota_status === "available" ? l : paint("error", l)));
		case "stats":
			return statsLines(details.stats, paint);
		case "launch":
			if (details.declined) return [paint("warning", `not launched: ${details.declined}`), paint("dim", details.command)];
			if (details.refused) return [paint("error", `refused: ${details.refused}`), paint("dim", details.command)];
			return [paint("dim", details.command), ...outcomeLines(details.outcome!, paint)];
		case "wait":
			return outcomeLines(details.outcome, paint);
		case "bus":
			if (details.messages.length === 0) return ["no bus messages"];
			return details.messages.map((m) => `${paint("dim", `${stamp(m.ts)} ${m.kind}`)}  ${m.from} -> ${m.to}: ${first(m.body)}`);
		case "bus_sent":
			return [`sent${details.id ? ` ${details.id}` : ""} to ${details.to}${details.run ? ` (run ${details.run})` : ""}`];
		case "skill":
			return details.text.split("\n");
		case "error":
			return [paint("error", details.message)];
	}
}

export function modelText(details: ToolDetails): string {
	return detailLines(details, plain).join("\n");
}

const COLLAPSED = 14;

export function renderLines(lines: string[], expanded: boolean, paint: Paint): string[] {
	if (expanded || lines.length <= COLLAPSED) return lines;
	return [...lines.slice(0, COLLAPSED), paint("dim", `... ${lines.length - COLLAPSED} more lines, expand to see all`)];
}

export function renderDetails(details: ToolDetails | undefined, expanded: boolean, theme: Theme, fallback: string): Component {
	const paint: Paint = (tone, text) => theme.fg(tone, text);
	if (!details) return new Text(theme.fg("dim", first(fallback)), 0, 0);
	const lines = renderLines(detailLines(details, paint), expanded, paint);
	if (details.kind === "skill" || details.kind === "logs" || details.kind === "error") return new Text(lines.join("\n"), 0, 0);
	// Tables and one-line-per-item lists read worse wrapped than cut at the edge.
	return { render: (width) => lines.map((l) => truncateToWidth(l, width)), invalidate() {} };
}
