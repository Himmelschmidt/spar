import assert from "node:assert/strict";
import { after, test } from "node:test";
import { EventFeed, type FeedEvent, feedArgs, parseFeedLine } from "../extensions/spar/feed.ts";
import { alive, installFake, cleanup, until } from "./helpers.ts";

after(cleanup);

const event = (cursor: string, extra: object = {}) => ({
	run_id: "r1",
	cursor,
	ts: "2026-11-01T10:00:00Z",
	type: "phase",
	phase: "review",
	prev_phase: "dispatch",
	round: 2,
	...extra,
});

test("parses O146 event lines and skips anything else", () => {
	assert.deepEqual(parseFeedLine(JSON.stringify(event("1,r1:42"))), event("1,r1:42"));
	assert.equal(parseFeedLine(JSON.stringify(event("1,r1:42,r2:7")))?.cursor, "1,r1:42,r2:7");
	assert.equal(parseFeedLine(JSON.stringify({ ...event("x"), cursor: 4 })), undefined);
	assert.equal(parseFeedLine("not json"), undefined);
	assert.equal(parseFeedLine("{broken"), undefined);
	assert.equal(parseFeedLine(JSON.stringify({ type: "phase" })), undefined);
});

test("feed args follow from a cursor and die with their parent", () => {
	assert.deepEqual(feedArgs("1,r1:42,r2:7"), ["events", "--follow", "--json", "--exit-on-stdin-eof", "--since", "1,r1:42,r2:7"]);
});

test("reconnects with --since the last cursor after the child exits, and stop kills it", async () => {
	const fake = installFake({
		events: [
			{ lines: [event("1,r1:10"), "garbage", event("1,r1:20", { type: "gate" })] },
			{ lines: [event("1,r1:30")], hold: true },
		],
	});
	const exitListeners = process.listenerCount("exit");
	const seen: FeedEvent[] = [];
	let connects = 0;
	const feed = new EventFeed({ cwd: fake.dir, onEvent: (e) => seen.push(e), onConnect: () => connects++, minBackoffMs: 10 });
	feed.start();
	try {
		await until(() => seen.length === 3);
	} finally {
		if (seen.length !== 3) feed.stop();
	}
	assert.deepEqual(
		fake.calls().filter((c) => c.startsWith("events")),
		[
			"events --tail-cursor --json",
			"events --follow --json --exit-on-stdin-eof --since 1",
			"events --follow --json --exit-on-stdin-eof --since 1,r1:20",
		],
	);
	assert.equal(connects, 2);
	assert.equal(feed.cursor, "1,r1:30");
	const pid = fake.pid(1)!;
	assert.ok(alive(pid));
	assert.equal(process.listenerCount("exit"), exitListeners + 1, "a live child is killed if pi exits");
	feed.stop();
	feed.stop();
	await until(() => !alive(pid));
	assert.equal(feed.running, false);
	await until(() => process.listenerCount("exit") === exitListeners);
});

test("a follower that drops before its first event resumes from the tail cursor, not from now", async () => {
	const fake = installFake({
		"events --tail-cursor --json": { cursor: "1,r1:5" },
		events: [{ lines: [] }, { lines: [event("1,r1:9")], hold: true }],
	});
	const seen: FeedEvent[] = [];
	const feed = new EventFeed({ cwd: fake.dir, onEvent: (e) => seen.push(e), minBackoffMs: 10 });
	await feed.start();
	try {
		await until(() => seen.length === 1);
	} finally {
		feed.stop();
	}
	assert.deepEqual(
		fake.calls().filter((c) => c.startsWith("events")),
		[
			"events --tail-cursor --json",
			"events --follow --json --exit-on-stdin-eof --since 1,r1:5",
			"events --follow --json --exit-on-stdin-eof --since 1,r1:5",
		],
	);
});

test("the follower exits when its parent's stdin pipe closes", async () => {
	const fake = installFake({ events: [{ lines: [], hold: true }] });
	const feed = new EventFeed({ cwd: fake.dir, onEvent: () => {}, minBackoffMs: 10_000 });
	await feed.start();
	await until(() => fake.pid(0) !== undefined);
	const pid = fake.pid(0)!;
	const child = (feed as unknown as { child: import("node:child_process").ChildProcess }).child;
	child.stdin!.end();
	try {
		await until(() => !alive(pid));
	} finally {
		feed.stop();
	}
});

test("a failed tail-cursor read retries instead of following from now", async () => {
	const fake = installFake({
		"events --tail-cursor --json": { __seq: [{ __exit: 1, __stderr: "error: boom" }, { cursor: "1,r1:3" }] },
		events: [{ lines: [], hold: true }],
	});
	const feed = new EventFeed({ cwd: fake.dir, onEvent: () => {}, minBackoffMs: 10 });
	await feed.start();
	try {
		await until(() => fake.calls().some((c) => c.startsWith("events --follow")));
	} finally {
		feed.stop();
	}
	assert.deepEqual(
		fake.calls().filter((c) => c.startsWith("events")),
		["events --tail-cursor --json", "events --tail-cursor --json", "events --follow --json --exit-on-stdin-eof --since 1,r1:3"],
	);
});
