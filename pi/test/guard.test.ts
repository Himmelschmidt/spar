import assert from "node:assert/strict";
import { afterEach, test } from "node:test";
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import { classify, judgeBash, sparInvocations } from "../extensions/spar/guard.ts";
import { guardBash, UNATTENDED_ENV, UNATTENDED_FLAG } from "../extensions/spar/tools.ts";

afterEach(() => {
	delete process.env[UNATTENDED_ENV];
});

test("finds spar wherever a shell would run it", () => {
	const cases: [string, string[][]][] = [
		["spar approve ab12", [["approve", "ab12"]]],
		["cd /x && spar ship ab12 --confirm", [["ship", "ab12", "--confirm"]]],
		["/home/u/.cargo/bin/spar approve ab12; echo ok", [["approve", "ab12"]]],
		["SPAR_DRY_RUN=1 FOO=bar spar plan -t x", [["plan", "-t", "x"]]],
		["env -u X timeout 5m spar stop ab12", [["stop", "ab12"]]],
		["echo $(spar approve ab12)", [["approve", "ab12"]]],
		["x=`spar reject ab12`", [["reject", "ab12"]]],
		['bash -c "spar ship ab12 --override-gate --reason r"', [["ship", "ab12", "--override-gate", "--reason", "r"]]],
		["sudo -E nohup spar cleanup ab12 > /dev/null 2>&1 &", [["cleanup", "ab12"]]],
		["spar status --json | jq . && spar brief ab12", [["status", "--json"], ["brief", "ab12"]]],
		["true || ( spar resume ab12 )", [["resume", "ab12"]]],
		["grep spar README.md", []],
		["echo spar approve ab12", []],
		["cat notes > spar", []],
		["# spar approve ab12\nls", []],
		["sparkle approve", []],
	];
	for (const [command, expected] of cases) assert.deepEqual(sparInvocations(command), expected, command);
});

test("classifies read-only, launch and operator commands", () => {
	const read = [
		"status --json",
		"brief ab12 --json",
		"events --run ab12 --json",
		"logs ab12 impl",
		"stats --since 7d",
		"wait ab12 --timeout 10m",
		"skills get core",
		"provider list --json",
		"bus log --run ab12",
		"bus inbox impl --run ab12",
		"rule list --run ab12",
		"plan -t x --providers cli:claude --validate-only --json",
		"--cwd /x status",
		"--help",
		"approve --help",
		"",
	];
	const launch = ["plan -t x --providers cli:claude", "implement -t x", "implement --run ab12", "run --workflow review -t x"];
	const operator = [
		"approve ab12",
		"reject ab12 --reason no",
		"ship ab12 --confirm",
		"confirm ab12",
		"reconcile ab12",
		"rule add --run ab12 text",
		"cleanup --all",
		"amend --run ab12 --contract c",
		"stop ab12",
		"resume ab12",
		"implement --run ab12 --max-rounds 14",
		"implement --run ab12 --reassign implementer=cli:claude",
		"implement --run=ab12 --max-rounds=+2",
		"provider pause cli:claude",
		"bus inbox impl --claim",
		"daemon stop",
		"archive --all",
	];
	for (const c of read) assert.equal(classify(c ? c.split(" ") : []), "read", c);
	for (const c of launch) assert.equal(classify(c.split(" ")), "launch", c);
	for (const c of operator) assert.equal(classify(c.split(" ")), "operator", c);
	assert.equal(judgeBash("spar status && spar plan -t x").kind, "launch");
	assert.equal(judgeBash("spar plan -t x; spar approve ab12").kind, "operator");
	assert.equal(judgeBash("spar status --json").kind, "none");
});

function ctx(answer?: string): ExtensionContext & { asked: string[] } {
	const asked: string[] = [];
	return {
		hasUI: answer !== undefined,
		asked,
		signal: undefined,
		ui: {
			select: async (title: string, options: string[]) => {
				asked.push(`${title}\n[${options.join("|")}]`);
				return answer;
			},
		},
	} as unknown as ExtensionContext & { asked: string[] };
}

const pi = (flags: Record<string, boolean> = {}) => ({ getFlag: (n: string) => flags[n] }) as unknown as ExtensionAPI;

test("bash gate and launch commands fail closed with no UI", async () => {
	const gate = await guardBash(pi(), "spar approve ab12", ctx());
	assert.equal(gate?.block, true);
	assert.match(gate!.reason, /operator action.*no operator UI.*\/spar/);
	const launch = await guardBash(pi(), "spar plan -t x --providers cli:claude", ctx());
	assert.equal(launch?.block, true);
	assert.match(launch!.reason, /spends money/);
	assert.equal(await guardBash(pi(), "spar status --json && spar brief ab12", ctx()), undefined);
});

test("the operator sees the exact command, Cancel first, and decides", async () => {
	const c = ctx("Cancel");
	const blocked = await guardBash(pi(), "cd /x && spar ship ab12 --confirm", c);
	assert.equal(blocked?.block, true);
	assert.match(c.asked[0]!, /spar ship ab12 --confirm\n\nfull bash command:\ncd \/x && spar ship ab12 --confirm\n\[Cancel\|Run it\]$/);
	assert.equal(await guardBash(pi(), "spar ship ab12 --confirm", ctx("Run it")), undefined);
	const shown = ctx("Cancel");
	await guardBash(pi(), "spar approve ab12 # \x1b[2K", shown);
	assert.doesNotMatch(shown.asked[0]!, /\x1b/);
});

test("unattended launches pass bash launches through, never operator actions", async () => {
	assert.equal(await guardBash(pi({ [UNATTENDED_FLAG]: true }), "spar plan -t x --providers cli:claude", ctx()), undefined);
	process.env[UNATTENDED_ENV] = "1";
	assert.equal((await guardBash(pi(), "spar approve ab12", ctx()))?.block, true);
});
