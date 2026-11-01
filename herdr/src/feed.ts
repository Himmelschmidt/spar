import { type ChildProcess, spawn } from "node:child_process";
import { createInterface } from "node:readline";
import { clean, type SparEvent, sparBin, sparJson } from "./spar.ts";

export interface FeedEvent extends SparEvent {
	run_id: string;
	cursor: string;
}

export function parseFeedLine(line: string): FeedEvent | undefined {
	if (!line.trim().startsWith("{")) return undefined;
	try {
		const raw = JSON.parse(line) as Record<string, unknown>;
		if (typeof raw.run_id !== "string" || typeof raw.cursor !== "string") return undefined;
		return raw as unknown as FeedEvent;
	} catch {
		return undefined;
	}
}

export interface FeedOptions {
	cwd: string;
	run?: string;
	onEvent: (event: FeedEvent) => void;
	// The feed could not start or died without an event; it keeps retrying with backoff.
	onError?: (message: string) => void;
	minBackoffMs?: number;
	maxBackoffMs?: number;
}

// `spar events --follow` for one project (or one run), restarted with backoff from the last
// cursor when it dies. stdin is a pipe so the follower exits with us even on SIGKILL.
export class Feed {
	private child: ChildProcess | undefined;
	private timer: NodeJS.Timeout | undefined;
	private stopped = false;
	private backoffMs: number;
	private cursor: string | undefined;
	private readonly options: FeedOptions;

	constructor(options: FeedOptions) {
		this.options = options;
		this.backoffMs = options.minBackoffMs ?? 2_000;
	}

	private get scope(): string[] {
		return this.options.run ? ["--run", this.options.run] : [];
	}

	async start(): Promise<void> {
		if (this.stopped || this.child) return;
		if (this.cursor === undefined) {
			const tail = await sparJson<{ cursor: string }>(["events", "--tail-cursor", "--json", ...this.scope], this.options.cwd);
			if (this.stopped) return;
			if (!tail.ok || typeof tail.value.cursor !== "string") {
				this.options.onError?.(tail.ok ? "spar events --tail-cursor printed no cursor" : tail.message);
				this.retry();
				return;
			}
			this.cursor = tail.value.cursor;
		}
		const child = spawn(sparBin(), ["events", "--follow", "--json", "--exit-on-stdin-eof", ...this.scope, "--since", this.cursor], {
			cwd: this.options.cwd,
			stdio: ["pipe", "pipe", "pipe"],
		});
		this.child = child;
		let stderr = "";
		let events = 0;
		child.stderr!.setEncoding("utf8");
		child.stderr!.on("data", (d: string) => {
			if (stderr.length < 4096) stderr += d;
		});
		createInterface({ input: child.stdout! }).on("line", (line) => {
			const event = parseFeedLine(line);
			if (!event || this.stopped) return;
			this.cursor = event.cursor;
			events++;
			this.backoffMs = this.options.minBackoffMs ?? 2_000;
			this.options.onEvent(event);
		});
		child.on("error", (e) => {
			stderr ||= e.message;
		});
		child.on("close", (code) => {
			if (this.child === child) this.child = undefined;
			if (!this.stopped && events === 0) this.options.onError?.(clean(stderr).trim().split("\n")[0] || `spar events exited ${code ?? "on a signal"}`);
			this.retry();
		});
	}

	private retry(): void {
		if (this.stopped || this.timer) return;
		const delay = this.backoffMs;
		this.backoffMs = Math.min(this.backoffMs * 2, this.options.maxBackoffMs ?? 60_000);
		this.timer = setTimeout(() => {
			this.timer = undefined;
			void this.start();
		}, delay);
	}

	stop(): void {
		this.stopped = true;
		if (this.timer) clearTimeout(this.timer);
		this.child?.kill("SIGTERM");
		this.child = undefined;
	}
}
