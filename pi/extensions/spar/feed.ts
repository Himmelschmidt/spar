import { type ChildProcess, spawn } from "node:child_process";
import { createInterface } from "node:readline";
import { type SparEvent, sparBin, sparJson } from "./spar.ts";

// The O146 contract: one JSON object per line, `{run_id, cursor, ...event}`, where `cursor` is
// an opaque string (`1,<run>:<offset>,...`) passed back verbatim as `--since`. Everything that
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
	if (typeof raw.run_id !== "string" || typeof raw.cursor !== "string") return undefined;
	return { ...(raw as unknown as SparEvent), run_id: raw.run_id, cursor: raw.cursor };
}

export const TAIL_CURSOR_ARGS = ["events", "--tail-cursor", "--json"];

export function feedArgs(cursor: string): string[] {
	return ["events", "--follow", "--json", "--exit-on-stdin-eof", "--since", cursor];
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
	private starting: Promise<void> | undefined;
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

	// Resolves once a child is following from a known cursor. The first start takes the
	// tail cursor before following, so a child that dies before its first event resumes
	// from there instead of from whatever "now" is at the restart.
	start(): Promise<void> {
		if (this.stopped || this.child) return Promise.resolve();
		this.starting ??= this.connect().finally(() => {
			this.starting = undefined;
		});
		return this.starting;
	}

	private async connect(): Promise<void> {
		if (this.cursor === undefined) {
			const tail = await sparJson<{ cursor: string }>(TAIL_CURSOR_ARGS, this.options.cwd);
			if (this.stopped) return;
			if (!tail.ok || typeof tail.value.cursor !== "string") {
				if (!tail.ok && tail.missing) this.stopped = true;
				this.scheduleRestart();
				return;
			}
			this.cursor = tail.value.cursor;
		}
		this.spawnChild(this.cursor);
	}

	private spawnChild(cursor: string): void {
		// stdin is a pipe that only closes when pi dies, which ends a follower started
		// with --exit-on-stdin-eof even when pi is SIGKILLed.
		const child = spawn(sparBin(), feedArgs(cursor), {
			cwd: this.options.cwd,
			stdio: ["pipe", "pipe", "ignore"],
			windowsHide: true,
		});
		const killOnExit = () => child.kill("SIGTERM");
		process.once("exit", killOnExit);
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
			process.removeListener("exit", killOnExit);
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
			void this.start();
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
