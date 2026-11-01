import { type ChildProcess, spawn } from "node:child_process";
import { createInterface } from "node:readline";
import { SPAR_BIN, type SparEvent } from "./spar.ts";

// The O146 contract: one JSON object per line, `{run_id, cursor, ...event}`. Everything that
// depends on that shape lives in this file.
export interface FeedEvent extends SparEvent {
	run_id: string;
	cursor: string;
}

export function parseFeedLine(line: string): FeedEvent | undefined {
	const trimmed = line.trim();
	if (!trimmed.startsWith("{")) return undefined;
	let value: unknown;
	try {
		value = JSON.parse(trimmed);
	} catch {
		return undefined;
	}
	if (typeof value !== "object" || value === null) return undefined;
	const raw = value as Record<string, unknown>;
	if (typeof raw.run_id !== "string" || raw.cursor === undefined || raw.cursor === null) return undefined;
	const cursor = typeof raw.cursor === "string" ? raw.cursor : JSON.stringify(raw.cursor);
	return { ...(raw as unknown as SparEvent), run_id: raw.run_id, cursor };
}

export function feedArgs(cursor: string | undefined): string[] {
	const args = ["events", "--follow", "--json"];
	if (cursor !== undefined) args.push("--since", cursor);
	return args;
}

export interface FeedOptions {
	cwd: string;
	onEvent: (event: FeedEvent) => void;
	onConnect?: () => void;
	minBackoffMs?: number;
	maxBackoffMs?: number;
}

export class EventFeed {
	private child: ChildProcess | undefined;
	private timer: NodeJS.Timeout | undefined;
	private stopped = false;
	private backoffMs: number;
	private readonly minBackoffMs: number;
	private readonly maxBackoffMs: number;
	private readonly options: FeedOptions;
	cursor: string | undefined;

	constructor(options: FeedOptions) {
		this.options = options;
		this.minBackoffMs = options.minBackoffMs ?? 2_000;
		this.maxBackoffMs = options.maxBackoffMs ?? 60_000;
		this.backoffMs = this.minBackoffMs;
	}

	start(): void {
		if (this.stopped || this.child) return;
		const child = spawn(SPAR_BIN, feedArgs(this.cursor), {
			cwd: this.options.cwd,
			stdio: ["ignore", "pipe", "ignore"],
			windowsHide: true,
		});
		this.child = child;
		this.options.onConnect?.();

		createInterface({ input: child.stdout! }).on("line", (line) => {
			const event = parseFeedLine(line);
			if (!event) return;
			this.cursor = event.cursor;
			this.backoffMs = this.minBackoffMs;
			this.options.onEvent(event);
		});

		let missing = false;
		child.on("error", (error: NodeJS.ErrnoException) => {
			if (error.code === "ENOENT") missing = true;
		});
		child.on("close", () => {
			if (this.child === child) this.child = undefined;
			if (missing) this.stopped = true;
			this.scheduleRestart();
		});
	}

	private scheduleRestart(): void {
		if (this.stopped || this.timer) return;
		const delay = this.backoffMs;
		this.backoffMs = Math.min(this.backoffMs * 2, this.maxBackoffMs);
		this.timer = setTimeout(() => {
			this.timer = undefined;
			this.start();
		}, delay);
		this.timer.unref();
	}

	stop(): void {
		this.stopped = true;
		if (this.timer) clearTimeout(this.timer);
		this.timer = undefined;
		const child = this.child;
		this.child = undefined;
		if (child && child.exitCode === null && child.signalCode === null) child.kill("SIGTERM");
	}

	get running(): boolean {
		return this.child !== undefined;
	}
}
