import assert from "node:assert/strict";
import { after, test } from "node:test";
import { Footer } from "../extensions/spar/footer.ts";
import { plain } from "../extensions/spar/format.ts";
import { alive, cleanup, installFake, installNoSpar, restorePath, row, until } from "./helpers.ts";

after(cleanup);

const statsBody = { groups: [{ key: "cli:claude", usage: { cost_usd: 2 } }], run_level_only: { runs: 0, cost_usd: null } };
const here = (initialized: boolean) => ({ project_root: "/p", spar_initialized: initialized });

test("footer shows runs, waiting runs, paused providers and cost, and refreshes on events", async () => {
	const fake = installFake({
		"status --here --json": here(true),
		"status --json": [row("a", { holds_seat: true }), row("b", { needs_human: true })],
		stats: statsBody,
		"provider list --json": [
			{ name: "claude", quota_status: "available" },
			{ name: "codex", quota_status: "paused_quota", paused_until: "2026-11-01T12:00:00Z" },
		],
		events: [
			{
				lines: [{ run_id: "a", cursor: "1,a:10", ts: "2026-11-01T10:00:00Z", type: "phase", phase: "review" }],
				hold: true,
			},
		],
	});
	const texts: (string | undefined)[] = [];
	const footer = new Footer({ cwd: fake.dir, paint: plain, setStatus: (t) => texts.push(t), debounceMs: 10 });
	try {
		await footer.start();
		await until(() => texts.includes("spar | 1 running | 1 need you | paused codex | $2.00 today"));
		await until(() => footer.feedCursor === "1,a:10");
		await until(() => fake.calls().filter((c) => c === "status --json").length >= 2);
		assert.ok(fake.calls().some((c) => /^stats --since \d+m --json$/.test(c)));
		await until(() => fake.pid(0) !== undefined);
	} finally {
		footer.stop();
		footer.stop();
	}
	await until(() => !alive(fake.pid(0)!));
});

test("footer re-reads status on its tick even when no events arrive", async () => {
	const fake = installFake({
		"status --here --json": here(true),
		"status --json": [row("a")],
		stats: statsBody,
		"provider list --json": [],
		events: [{ lines: [], hold: true }],
	});
	const footer = new Footer({ cwd: fake.dir, paint: plain, setStatus: () => {}, refreshMs: 20, debounceMs: 5 });
	try {
		await footer.start();
		await until(() => fake.calls().filter((c) => c === "status --json").length >= 3);
	} finally {
		footer.stop();
	}
	await until(() => fake.pid(0) === undefined || !alive(fake.pid(0)!));
});

test("footer stays quiet in an uninitialized checkout and retries the probe", async () => {
	const fake = installFake({
		"status --here --json": { __seq: [here(false), here(false), here(true)] },
		"status --json": [row("a", { needs_human: true })],
		stats: statsBody,
		"provider list --json": [],
		events: [{ lines: [], hold: true }],
	});
	const texts: (string | undefined)[] = [];
	const footer = new Footer({ cwd: fake.dir, paint: plain, setStatus: (t) => texts.push(t), probeBackoffMs: 10 });
	try {
		await footer.start();
		assert.equal(texts.length, 0);
		assert.deepEqual(fake.calls(), ["status --here --json"]);
		await until(() => texts.includes("spar | 0 running | 1 need you | $2.00 today"));
		assert.equal(fake.calls().filter((c) => c === "status --here --json").length, 3);
	} finally {
		footer.stop();
	}
	await until(() => fake.pid(0) === undefined || !alive(fake.pid(0)!));
});

test("footer stays quiet when spar is not installed", async () => {
	installNoSpar();
	const texts: (string | undefined)[] = [];
	const footer = new Footer({ cwd: process.cwd(), paint: plain, setStatus: (t) => texts.push(t) });
	await footer.start();
	footer.stop();
	restorePath();
	assert.deepEqual(texts, []);
});
