import assert from "node:assert/strict";
import { after, test } from "node:test";
import { Footer } from "../extensions/spar/footer.ts";
import { plain } from "../extensions/spar/format.ts";
import { alive, cleanup, installFake, installNoSpar, restorePath, row, until } from "./helpers.ts";

after(cleanup);

const statsBody = { groups: [{ key: "cli:claude", usage: { cost_usd: 2 } }], run_level_only: { runs: 0, cost_usd: null } };

test("footer shows runs, waiting runs, paused providers and cost, and refreshes on events", async () => {
	const fake = installFake({
		"status --json": [row("a", { holds_seat: true }), row("b", { needs_human: true })],
		stats: statsBody,
		"provider list --json": [
			{ name: "claude", quota_status: "available" },
			{ name: "codex", quota_status: "paused_quota", paused_until: "2026-11-01T12:00:00Z" },
		],
		events: [{ lines: [{ run_id: "a", cursor: "a:1", ts: "2026-11-01T10:00:00Z", type: "phase", phase: "review" }], hold: true }],
	});
	const texts: (string | undefined)[] = [];
	const footer = new Footer({ cwd: fake.dir, paint: plain, setStatus: (t) => texts.push(t), debounceMs: 10 });
	try {
		await footer.start();
		await until(() => texts.includes("spar | 1 running | 1 need you | paused codex | $2.00 today"));
		await until(() => footer.feedCursor === "a:1");
		await until(() => fake.calls().filter((c) => c === "status --json").length >= 2);
		assert.ok(fake.calls().some((c) => /^stats --since \d+m --json$/.test(c)));
		await until(() => fake.pid(0) !== undefined);
	} finally {
		footer.stop();
		footer.stop();
	}
	await until(() => !alive(fake.pid(0)!));
});

test("footer stays quiet outside a spar project", async () => {
	const fake = installFake({ stats: { __exit: 1, __stderr: "error: not inside a project" } });
	const texts: (string | undefined)[] = [];
	const footer = new Footer({ cwd: fake.dir, paint: plain, setStatus: (t) => texts.push(t) });
	await footer.start();
	footer.stop();
	assert.deepEqual(texts, []);
	assert.deepEqual(
		fake.calls().map((c) => c.split(" ")[0]),
		["stats"],
	);
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
