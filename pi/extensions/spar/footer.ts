import { EventFeed, type FeedEvent } from "./feed.ts";
import { footerText, isPaused, type Paint } from "./format.ts";
import {
	type Here,
	minutesSinceLocalMidnight,
	type Provider,
	type RunRow,
	type Stats,
	sparJson,
	statsCost,
} from "./spar.ts";

// Applied at once so the footer moves before the debounced re-read confirms it: an
// archived run leaves the counts, and an abandoned one is no longer running.
export function applyEvent(runs: RunRow[], event: FeedEvent): RunRow[] {
	if (event.type === "archived") return runs.filter((r) => r.id !== event.run_id);
	if (event.type === "abandoned") {
		return runs.map((r) => (r.id === event.run_id ? { ...r, holds_seat: false, abandoned: true } : r));
	}
	return runs;
}

export interface FooterOptions {
	cwd: string;
	paint: Paint;
	setStatus: (text: string | undefined) => void;
	onEvent?: (event: FeedEvent) => void;
	// The first successful status read, and when the feed took its starting cursor: a run
	// last changed before then has no event coming for the state it is already in.
	onFirstRuns?: (runs: RunRow[], feedStart: Date) => void;
	debounceMs?: number;
	refreshMs?: number;
	providerPollMs?: number;
	feedBackoffMs?: number;
	probeBackoffMs?: number;
	probeMaxBackoffMs?: number;
}

export class Footer {
	runs: RunRow[] | undefined;
	private paused: Provider[] = [];
	private costToday: number | undefined;
	private stopped = false;
	private feed: EventFeed | undefined;
	private debounce: NodeJS.Timeout | undefined;
	private probeTimer: NodeJS.Timeout | undefined;
	private timers: NodeJS.Timeout[] = [];
	private refreshing: Promise<void> | undefined;
	private refreshAgain = false;
	private feedStart: Date | undefined;
	private probeBackoffMs: number;
	private readonly options: FooterOptions;

	constructor(options: FooterOptions) {
		this.options = options;
		this.probeBackoffMs = options.probeBackoffMs ?? 30_000;
	}

	async start(): Promise<void> {
		const here = await sparJson<Here>(["status", "--here", "--json"], this.options.cwd);
		if (this.stopped) return;
		if (!here.ok || !here.value.spar_initialized) {
			this.retryProbe();
			return;
		}
		let connects = 0;
		this.feed = new EventFeed({
			cwd: this.options.cwd,
			onEvent: (event) => {
				this.options.onEvent?.(event);
				if (this.runs) {
					this.runs = applyEvent(this.runs, event);
					this.render();
				}
				this.scheduleRefresh();
			},
			onConnect: () => {
				if (connects++ > 0) this.scheduleRefresh();
			},
			minBackoffMs: this.options.feedBackoffMs,
		});
		this.feedStart = new Date();
		await this.feed.start();
		if (this.stopped) return;
		this.every(this.options.refreshMs ?? 60_000, () => this.scheduleRefresh());
		this.every(this.options.providerPollMs ?? 300_000, () => void this.pollProviders());
		void this.refresh();
		void this.pollProviders();
	}

	stop(): void {
		this.stopped = true;
		this.feed?.stop();
		this.feed = undefined;
		if (this.debounce) clearTimeout(this.debounce);
		if (this.probeTimer) clearTimeout(this.probeTimer);
		for (const timer of this.timers) clearInterval(timer);
		this.debounce = undefined;
		this.probeTimer = undefined;
		this.timers = [];
	}

	get feedCursor(): string | undefined {
		return this.feed?.cursor;
	}

	private retryProbe(): void {
		const delay = this.probeBackoffMs;
		this.probeBackoffMs = Math.min(delay * 2, this.options.probeMaxBackoffMs ?? 600_000);
		this.probeTimer = setTimeout(() => {
			this.probeTimer = undefined;
			void this.start();
		}, delay);
		this.probeTimer.unref();
	}

	private every(ms: number, tick: () => void): void {
		const timer = setInterval(tick, ms);
		timer.unref();
		this.timers.push(timer);
	}

	scheduleRefresh(): void {
		if (this.stopped || this.debounce) return;
		this.debounce = setTimeout(() => {
			this.debounce = undefined;
			void this.refresh();
		}, this.options.debounceMs ?? 500);
		this.debounce.unref();
	}

	refresh(): Promise<void> {
		if (this.stopped) return Promise.resolve();
		if (this.refreshing) {
			this.refreshAgain = true;
			return this.refreshing;
		}
		this.refreshing = (async () => {
			do {
				this.refreshAgain = false;
				const runs = await sparJson<RunRow[]>(["status", "--json"], this.options.cwd);
				if (this.stopped) return;
				if (runs.ok) {
					const first = this.runs === undefined;
					this.runs = runs.value;
					if (first && this.feedStart) this.options.onFirstRuns?.(runs.value, this.feedStart);
				}
				const since = `${minutesSinceLocalMidnight(new Date())}m`;
				const stats = await sparJson<Stats>(["stats", "--since", since, "--json"], this.options.cwd);
				if (this.stopped) return;
				if (stats.ok) this.costToday = statsCost(stats.value);
				this.render();
			} while (this.refreshAgain && !this.stopped);
		})().finally(() => {
			this.refreshing = undefined;
		});
		return this.refreshing;
	}

	private async pollProviders(): Promise<void> {
		const providers = await sparJson<Provider[]>(["provider", "list", "--json"], this.options.cwd, 30_000);
		if (this.stopped || !providers.ok) return;
		this.paused = providers.value.filter(isPaused);
		this.render();
	}

	private render(): void {
		if (this.stopped || !this.runs) return;
		this.options.setStatus(
			footerText(
				{ runs: this.runs, paused: this.paused, costToday: this.costToday, now: new Date() },
				this.options.paint,
			),
		);
	}
}
