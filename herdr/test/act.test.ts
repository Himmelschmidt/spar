import assert from "node:assert/strict";
import { tmpdir } from "node:os";
import { afterEach, test } from "node:test";
import { act, type Io } from "../src/act.ts";
import { cleanup, installHerdr, installSpar, row, status } from "./helpers.ts";

afterEach(cleanup);

const config = { recent_hours: 24, board_on_startup: false, sync_interval_secs: 60 };
const ROOT = tmpdir();
const BRIEF = { run: "r1", next_command: "spar approve r1", open_findings: [], recent_events: [] };

function scripted(answers: string[]): Io & { out: string[] } {
	const out: string[] = [];
	return {
		out,
		width: 72,
		ask: async (q) => {
			out.push(q);
			return answers.shift();
		},
		print: (t) => out.push(t),
	};
}

const plain = (io: { out: string[] }) => io.out.join("\n").replace(/\x1b\[[0-9;]*m/g, "");

test("the popup lists only the verbs valid at the gate", async () => {
	installSpar({ "status r1 --json": status("r1", { phase: "awaiting_plan_approval", needs_human: true }), "brief r1 --json": BRIEF });
	installHerdr();
	const io = scripted(["q"]);
	await act(io, config, "r1", ROOT);
	const text = plain(io);
	assert.match(text, /1\) approve: approve the plan/);
	assert.match(text, /2\) reject: /);
	assert.match(text, /3\) rule: /);
	assert.doesNotMatch(text, /\) ship: /);
});

test("a verb runs only after a typed yes", async () => {
	const spar = installSpar({
		"status r1 --json": status("r1", { phase: "awaiting_plan_approval", needs_human: true }),
		"brief r1 --json": BRIEF,
		"approve r1 --json": { phase: "plan_approved" },
	});
	installHerdr();
	const no = scripted(["1", "no", "q"]);
	assert.deepEqual(await act(no, config, "r1", ROOT), []);
	assert.ok(!spar.argv().some((a) => a[0] === "approve"));
	assert.match(plain(no), /Cancelled, nothing ran/);

	const yes = scripted(["1", "yes", "q"]);
	assert.deepEqual(await act(yes, config, "r1", ROOT), ["approve"]);
	assert.deepEqual(spar.argv().find((a) => a[0] === "approve"), ["approve", "r1", "--json"]);
	assert.match(plain(yes), /Runs: spar approve r1 --json/);
	assert.match(plain(yes), /spar approve r1: done, phase plan_approved/);
});

test("ship passes --confirm once the human confirms in the popup", async () => {
	const spar = installSpar({
		"status r1 --json": status("r1", { phase: "awaiting_ship_confirm", needs_human: true }),
		"brief r1 --json": BRIEF,
		"ship r1 --confirm --json": { phase: "done", pr_url: "https://example.test/pr/1" },
	});
	installHerdr();
	const io = scripted(["1", "y", "q"]);
	assert.deepEqual(await act(io, config, "r1", ROOT), ["ship"]);
	assert.deepEqual(spar.argv().find((a) => a[0] === "ship"), ["ship", "r1", "--confirm", "--json"]);
	assert.match(plain(io), /pr\/1/);
});

test("an override without a reason runs nothing", async () => {
	const spar = installSpar({ "status r1 --json": status("r1", { phase: "failed", needs_human: true }), "brief r1 --json": BRIEF });
	installHerdr();
	const io = scripted(["1", "", "q"]);
	assert.deepEqual(await act(io, config, "r1", ROOT), []);
	assert.ok(!spar.argv().some((a) => a[0] === "ship"));
});

test("with no run in context the popup offers the runs that need you", async () => {
	installSpar({
		"status --all --json": [row("r1", { phase: "awaiting_plan_approval", needs_human: true, project_root: ROOT }), row("r2", { phase: "done", project_root: ROOT })],
		"status r1 --json": status("r1", { phase: "awaiting_plan_approval", needs_human: true }),
		"brief r1 --json": BRIEF,
	});
	installHerdr();
	const io = scripted(["1", "q"]);
	await act(io, config);
	const text = plain(io);
	assert.match(text, /1\) NEEDS YOU awaiting_plan_approval r1/);
	assert.doesNotMatch(text, /r2/);
	assert.match(text, /1\) approve/);
});
