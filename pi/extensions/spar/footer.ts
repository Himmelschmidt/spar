import { EventFeed } from "./feed.ts";
import { footerText, isPaused, type Paint } from "./format.ts";
import { minutesSinceLocalMidnight, type Provider, type RunRow, type Stats, sparJson, statsCost } from "./spar.ts";

export interface FooterOptions {
	cwd: string;
	paint: Paint;
	setStatus: (text: string | undefined) => void;
	debounceMs?: number;
	providerPollMs?: number;
	feedBackoffMs?: number;
}

export class Footer {
	runs: RunRow[] | undefined;
	private paused: Provider[] = [];
	private costToday: number | undefined;
	private stopped = false;
	private feed: EventFeed | undefined;
	private debounce: NodeJS.Timeout | undefined;
	private providerTimer: NodeJS.Timeout | undefined;
	private refreshing: Promise<void> | undefined;
	private refreshAgain = false;
	private readonly options: FooterOptions;

	constructor(options: FooterOptions) {
		this.options = options;
	}

	async start(): Promise<void> {
		const probe = await this.fetchStats();
		if (this.stopped || !probe) return;
		await this.refresh();
		if (this.stopped) return;
		void this.pollProviders();
		this.providerTimer = setInterval(() => void this.pollProviders(), this.options.providerPollMs ?? 60_000);
		this.providerTimer.unref();
		let connects = 0;
		this.feed = new EventFeed({
			cwd: this.options.cwd,
			onEvent: () => this.scheduleRefresh(),
			onConnect: () => {
				if (connects++ > 0) this.scheduleRefresh();
			},
			minBackoffMs: this.options.feedBackoffMs,
		});
		this.feed.start();
	}

	stop(): void {
		this.stopped = true;
		this.feed?.stop();
		this.feed = undefined;
		if (this.debounce) clearTimeout(this.debounce);
		if (this.providerTimer) clearInterval(this.providerTimer);
		this.debounce = undefined;
		this.providerTimer = undefined;
	}

	get feedCursor(): string | undefined {
		return this.feed?.cursor;
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
				if (runs.ok) this.runs = runs.value;
				await this.fetchStats();
				this.render();
			} while (this.refreshAgain && !this.stopped);
		})().finally(() => {
			this.refreshing = undefined;
		});
		return this.refreshing;
	}

	private async fetchStats(): Promise<boolean> {
		const since = `${minutesSinceLocalMidnight(new Date())}m`;
		const stats = await sparJson<Stats>(["stats", "--since", since, "--json"], this.options.cwd);
		if (stats.ok) this.costToday = statsCost(stats.value);
		return stats.ok;
	}

	private async pollProviders(): Promise<void> {
		const providers = await sparJson<Provider[]>(["provider", "list", "--json"], this.options.cwd, 30_000);
		if (!providers.ok) return;
		this.paused = providers.value.filter(isPaused);
		this.render();
	}

	private render(): void {
		if (this.stopped || !this.runs) return;
		this.options.setStatus(
			footerText({ runs: this.runs, paused: this.paused, costToday: this.costToday }, this.options.paint),
		);
	}
}
