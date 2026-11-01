import assert from "node:assert/strict";
import { after, test } from "node:test";
import type { ExtensionAPI, ExtensionCommandContext } from "@earendil-works/pi-coding-agent";
import extension from "../extensions/spar/index.ts";
import type { Brief } from "../extensions/spar/spar.ts";
import { installFake, cleanup, row, status } from "./helpers.ts";

after(cleanup);

type Command = Parameters<ExtensionAPI["registerCommand"]>[1];

function load(): Command {
	let command: Command | undefined;
	const pi = {
		on: () => () => {},
		registerTool: () => {},
		registerFlag: () => {},
		registerCommand: (name: string, options: Command) => {
			if (name === "spar") command = options;
		},
	} as unknown as ExtensionAPI;
	extension(pi);
	return command!;
}

async function printed(run: () => Promise<void>): Promise<string> {
	const original = process.stdout.write.bind(process.stdout);
	let out = "";
	process.stdout.write = ((chunk: unknown, ...rest: never[]) => {
		if (typeof chunk !== "string") return original(chunk as Uint8Array, ...rest);
		out += chunk;
		return true;
	}) as typeof process.stdout.write;
	try {
		await run();
	} finally {
		process.stdout.write = original;
	}
	return out;
}

const brief: Brief = {
	run: "bbbb2222",
	next_command: "spar ship bbbb2222",
	open_findings: [{ slot_id: "review-0", severity: "major", text: "off by one", file: "src/x.rs", line: 9 }],
	recent_events: [],
};

function fake() {
	return installFake({
		"status --json": [row("aaaa1111", { cost_usd: 12.5 }), row("bbbb2222", { needs_human: true })],
		"status bbbb2222 --json": status("bbbb2222", { needs_human: true }),
		"brief bbbb2222 --json": brief,
		"bus log --run bbbb2222 --json": [],
	});
}

const printCtx = (cwd: string) => ({ mode: "print", hasUI: false, cwd }) as unknown as ExtensionCommandContext;

test("/spar in print mode lists runs with the waiting one first", async () => {
	const f = fake();
	const out = await printed(() => load().handler("", printCtx(f.dir)));
	const lines = out.trim().split("\n");
	assert.match(lines[0]!, /^bbbb2222 .*NEEDS YOU/);
	assert.match(lines[1]!, /^aaaa1111 .*\$12\.50 +loop$/);
	assert.deepEqual(f.calls(), ["status --json"], "costs come from the list rows");
});

test("/spar rejects an argument that is not a run id instead of passing it to spar", async () => {
	const f = fake();
	const out = await printed(() => load().handler("--all", printCtx(f.dir)));
	assert.equal(out, 'spar: "--all" is not a run id. Usage: /spar [run-id]\n');
	assert.deepEqual(f.calls(), []);
});

test("/spar <id> in print mode prints the run detail", async () => {
	const f = fake();
	const out = await printed(() => load().handler("bbbb2222", printCtx(f.dir)));
	assert.match(out, /^run bbbb2222 /);
	assert.match(out, /major \(1\)\n {4}- src\/x\.rs:9 off by one \(review-0\)/);
	assert.match(out, /next: spar ship bbbb2222\n$/);
});

test("/spar <unknown id> reports the spar error", async () => {
	const f = fake();
	const out = await printed(() => load().handler("zzzz", printCtx(f.dir)));
	assert.equal(out, "spar: error: no fake response for \"status zzzz --json\"\n");
});

test("argument completion offers run ids, waiting runs first", async () => {
	const f = fake();
	process.chdir(f.dir);
	const items = await load().getArgumentCompletions!("");
	assert.deepEqual(
		items?.map((i) => i.value),
		["bbbb2222", "aaaa1111"],
	);
	assert.deepEqual(
		(await load().getArgumentCompletions!("aa"))?.map((i) => i.value),
		["aaaa1111"],
	);
	assert.equal(await load().getArgumentCompletions!("q"), null);
});
