import { execFile, spawn } from "node:child_process";
import { closeSync, mkdirSync, mkdtempSync, openSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createInterface } from "node:readline";
import type { Config } from "./config.ts";
import { type ActionInput, actionsFor, BACKGROUND, buildArgv, displayCommand, effect, isGate, isStallPark, outcome, type Raw, VERBS, type Verb } from "./gates.ts";
import { type Brief, clean, firstLine, flag, type RunRow, type RunStatus, sparBin, sparJson } from "./spar.ts";
import { listRuns } from "./sync.ts";
import { paint, renderView } from "./view.ts";
import { wrap } from "./wrap.ts";

export interface Io {
	ask(question: string): Promise<string | undefined>;
	print(text: string): void;
	width: number;
}

export function terminalIo(): Io {
	const rl = createInterface({ input: process.stdin, output: process.stdout });
	let closed = false;
	rl.on("close", () => {
		closed = true;
	});
	return {
		ask: (question) =>
			closed
				? Promise.resolve(undefined)
				: new Promise((resolve) => {
						const onClose = () => resolve(undefined);
						rl.once("close", onClose);
						rl.question(question, (answer) => {
							rl.removeListener("close", onClose);
							resolve(answer.trim());
						});
					}),
		print: (text) => process.stdout.write(`${text}\n`),
		get width() {
			return Math.max(20, process.stdout.columns || 80);
		},
	};
}

function runForeground(args: string[], cwd: string): Promise<Raw> {
	return new Promise((resolve) => {
		execFile(sparBin(), args, { cwd, timeout: 120_000, maxBuffer: 64 * 1024 * 1024 }, (error, stdout, stderr) => {
			const err = error as (NodeJS.ErrnoException & { code?: number | string }) | null;
			if (err?.code === "ENOENT") return resolve({ code: null, stdout: "", stderr: "", missing: true });
			resolve({ code: err ? (typeof err.code === "number" ? err.code : null) : 0, stdout, stderr });
		});
	});
}

// Detached into its own session with output in files: closing the popup neither kills the
// push half-way nor leaves it writing to a closed terminal.
function runBackground(args: string[], cwd: string): { dir: string; done: Promise<Raw> } {
	const base = process.env.HERDR_PLUGIN_STATE_DIR || tmpdir();
	mkdirSync(base, { recursive: true });
	const dir = mkdtempSync(join(base, "job-"));
	const out = openSync(join(dir, "stdout"), "w");
	const err = openSync(join(dir, "stderr"), "w");
	const done = new Promise<Raw>((resolve) => {
		const child = spawn(sparBin(), args, { cwd, detached: true, stdio: ["ignore", out, err] });
		closeSync(out);
		closeSync(err);
		child.unref();
		child.on("error", (e: NodeJS.ErrnoException) => resolve({ code: null, stdout: "", stderr: e.message, missing: e.code === "ENOENT" }));
		child.on("exit", (code) =>
			resolve({ code, stdout: readFileSync(join(dir, "stdout"), "utf8"), stderr: readFileSync(join(dir, "stderr"), "utf8") }),
		);
	});
	return { dir, done };
}

async function inputsFor(io: Io, verb: Verb, run: RunStatus): Promise<ActionInput | undefined> {
	switch (verb) {
		case "reject": {
			const reason = await io.ask("Reason (optional, enter to skip): ");
			return reason === undefined ? undefined : { reason: reason || undefined };
		}
		case "override": {
			const reason = await io.ask("Reason for shipping over the gate (required): ");
			if (!reason) {
				if (reason !== undefined) io.print("An override needs a reason; nothing ran.");
				return undefined;
			}
			return { reason };
		}
		case "confirm": {
			const implementers = run.slots.filter((s) => s.role === "implementer").map((s) => s.id);
			if (implementers.length === 0) return {};
			io.print(`  0) ranked default${run.winner_slot ? ` (${run.winner_slot})` : ""}`);
			implementers.forEach((s, i) => io.print(`  ${i + 1}) ${s}`));
			const pick = await io.ask("Winner [0]: ");
			if (pick === undefined) return undefined;
			const n = pick === "" ? 0 : Number(pick);
			if (!Number.isInteger(n) || n < 0 || n > implementers.length) {
				io.print(`"${pick}" is not on the list; nothing ran.`);
				return undefined;
			}
			return { winner: n === 0 ? undefined : implementers[n - 1] };
		}
		case "extend": {
			if (isStallPark(run)) return {};
			const text = await io.ask("Rounds to add [1]: ");
			if (text === undefined) return undefined;
			const rounds = text === "" ? 1 : Number(text.replace(/^\+/, ""));
			if (!Number.isInteger(rounds) || rounds < 1) {
				io.print(`"${text}" is not a whole number of rounds >= 1; nothing ran.`);
				return undefined;
			}
			return { rounds };
		}
		case "rule": {
			const text = await io.ask("Ruling text: ");
			if (!text) {
				if (text !== undefined) io.print("A ruling needs text; nothing ran.");
				return undefined;
			}
			const waived = await io.ask("Waive criteria (optional, e.g. AC-2,AC-5): ");
			if (waived === undefined) return undefined;
			const waive = waived.split(/[\s,]+/).filter(Boolean);
			const bad = waive.find((w) => !/^AC-\d+$/.test(w));
			if (bad) {
				io.print(`"${bad}" is not a criterion id like AC-2; nothing ran.`);
				return undefined;
			}
			return { text, waive };
		}
		case "reassign": {
			if (run.slots.length === 0) {
				io.print("The run has no seats yet; nothing to reassign.");
				return undefined;
			}
			run.slots.forEach((s, i) => io.print(`  ${i + 1}) ${s.id} ${s.role} ${s.provider}`));
			const pick = Number(await io.ask("Seat: "));
			const seat = run.slots[pick - 1]?.id;
			if (!seat) {
				io.print("Not a seat on the list; nothing ran.");
				return undefined;
			}
			const provider = await io.ask("Provider (e.g. cli:claude@sonnet): ");
			if (!provider || /\s/.test(provider)) {
				if (provider !== undefined) io.print("Name a provider like cli:claude@sonnet; nothing ran.");
				return undefined;
			}
			return { seat, provider };
		}
		default:
			return {};
	}
}

function gateDetail(status: RunStatus, brief: Brief | undefined, width: number): string[] {
	const lines: string[] = [];
	const add = (text: string, indent = 2) => lines.push(...wrap(text, width, indent));
	const findings = brief?.open_findings ?? [];
	if (findings.length > 0) {
		add("open findings");
		for (const f of findings.slice(0, 10)) {
			add(`  ${f.severity} ${f.file ? `${f.file}${f.line ? `:${f.line}` : ""} ` : ""}${firstLine(f.text)}`, 4);
		}
		if (findings.length > 10) add(`  +${findings.length - 10} more (spar brief ${status.id})`);
	}
	if (status.phase === "awaiting_winner_confirm" || status.phase === "awaiting_reconcile") {
		const implementers = status.slots.filter((s) => s.role === "implementer").map((s) => s.id);
		add(`candidates: ${implementers.join(", ") || "none"}${status.winner_slot ? `; ranked winner ${status.winner_slot}` : ""}`);
	}
	const docs = (brief?.artifacts ?? []).map((a) => a.name).filter((n) => !n.startsWith("review-"));
	if (docs.length > 0) add(`artifacts: ${docs.join(", ")} (b shows the full brief)`);
	return lines;
}

async function showFullBrief(id: string, root: string): Promise<void> {
	const raw = await runForeground(["brief", id, "--full"], root);
	const text = clean(raw.stdout || raw.stderr).replace(/\r/g, "");
	await new Promise<void>((resolve) => {
		const pager = spawn(process.env.PAGER || "less", [], { stdio: ["pipe", "inherit", "inherit"] });
		pager.on("error", () => {
			process.stdout.write(`${text}\n`);
			resolve();
		});
		pager.on("close", () => resolve());
		pager.stdin!.on("error", () => {});
		pager.stdin!.end(text);
	});
}

async function pickRun(io: Io, config: Config): Promise<RunRow | undefined> {
	const now = new Date();
	const runs = (await listRuns(config)).filter((r) => flag(r, now) === "NEEDS YOU" || r.holds_seat);
	if (runs.length === 0) {
		io.print("No run needs you or holds a seat.");
		return undefined;
	}
	runs.forEach((r, i) => {
		for (const l of wrap(`${i + 1}) ${flag(r, now)} ${r.phase} ${r.id} ${r.project_name ?? ""} ${firstLine(r.task)}`, io.width, 4)) io.print(l);
	});
	const pick = Number(await io.ask("Run: "));
	return runs[pick - 1];
}

// The gate popup: what the run is waiting on, the verbs that apply now, and an explicit
// typed confirmation before any of them runs. Returns the verbs it ran.
export async function act(io: Io, config: Config, id?: string, root?: string): Promise<Verb[]> {
	const ran: Verb[] = [];
	if (!id || !root) {
		const run = await pickRun(io, config);
		if (!run?.project_root) return ran;
		id = run.id;
		root = run.project_root;
	}
	for (;;) {
		const status = await sparJson<RunStatus>(["status", id, "--json"], root);
		if (!status.ok) {
			io.print(`spar status ${id}: ${status.message}`);
			await io.ask("Enter to close: ");
			return ran;
		}
		const brief = await sparJson<Brief>(["brief", id, "--json"], root);
		const run = status.value;
		io.print(paint(renderView({ status: run, brief: brief.ok ? brief.value : undefined, now: new Date(), width: io.width })));
		for (const l of gateDetail(run, brief.ok ? brief.value : undefined, io.width)) io.print(l);
		const verbs = actionsFor(run);
		io.print("");
		if (!isGate(run.phase) && flag(run, new Date()) !== "NEEDS YOU") io.print(`Not at a gate (${run.phase}).`);
		verbs.forEach((v, i) => {
			for (const l of wrap(`${i + 1}) ${v}: ${VERBS[v]}`, io.width, 4)) io.print(l);
		});
		io.print("b) full brief   r) refresh   q) close");
		const choice = await io.ask("> ");
		if (choice === undefined || choice === "q") return ran;
		if (choice === "r" || choice === "") continue;
		if (choice === "b") {
			await showFullBrief(id, root);
			continue;
		}
		const verb = verbs[Number(choice) - 1];
		if (!verb) {
			io.print(`"${choice}" is not on the list.`);
			continue;
		}
		const input = await inputsFor(io, verb, run);
		if (!input) continue;
		const args = buildArgv(verb, id, input, run);
		for (const l of wrap(`Runs: ${displayCommand(args)}`, io.width, 6)) io.print(l);
		for (const l of wrap(effect(verb, run, input), io.width, 0)) io.print(l);
		const yes = await io.ask("Type yes to run it, anything else cancels: ");
		if (yes === undefined) return ran;
		if (yes !== "yes") {
			io.print("Cancelled, nothing ran.");
			continue;
		}
		// The run may have moved while the operator read and typed.
		const now = await sparJson<RunStatus>(["status", id, "--json"], root);
		if (!now.ok || !actionsFor(now.value).includes(verb) || now.value.phase !== run.phase) {
			io.print(now.ok ? `The run moved to ${now.value.phase}${now.value.holds_seat ? " and is running" : ""}; ${verb} no longer applies. Nothing ran.` : `spar status ${id}: ${now.message}. Nothing ran.`);
			continue;
		}
		let raw: Raw;
		if (BACKGROUND.has(verb)) {
			const job = runBackground(args, root);
			io.print(`Started in the background (output in ${job.dir}); closing this popup does not stop it.`);
			raw = await job.done;
			if (raw.code === 0) rmSync(job.dir, { recursive: true, force: true });
		} else {
			io.print("Running...");
			raw = await runForeground(args, root);
		}
		ran.push(verb);
		const result = outcome(verb, id, raw);
		for (const l of result.text.split("\n").flatMap((t) => wrap(t, io.width, 2))) io.print(l);
		const next = await io.ask("Enter to go back, q to close: ");
		if (next === undefined || next === "q") return ran;
	}
}
