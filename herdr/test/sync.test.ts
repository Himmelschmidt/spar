import assert from "node:assert/strict";
import { afterEach, test } from "node:test";
import { loadState, saveState } from "../src/config.ts";
import { reapRestored, syncOnce } from "../src/sync.ts";
import { cleanup, installHerdr, installSpar, row } from "./helpers.ts";

const config = { recent_hours: 24, board_on_startup: false, sync_interval_secs: 60 };
const DAY = 86_400_000;
const beta = { project_name: "beta", project_root: "/p/beta" };

function runs() {
	return [
		row("a1", { phase: "awaiting_plan_approval", needs_human: true }),
		row("a2", { phase: "dispatch", holds_seat: true }),
		row("b1", { phase: "done", ...beta }),
		row("old", { phase: "done", updated_at: new Date(Date.now() - 3 * DAY).toISOString() }),
	];
}

const MUTATING = ["workspace create", "tab create", "tab rename", "plugin pane", "pane close", "pane split"];
const mutations = (argv: string[][]) => argv.filter((a) => MUTATING.includes(a.slice(0, 2).join(" ")));

afterEach(cleanup);

test("sync opens one run pane per relevant run, grouped in a spar tab per project", async () => {
	installSpar({ "status --all --json": runs() });
	const herdr = installHerdr();
	const result = await syncOnce(config);
	assert.deepEqual(result.opened.sort(), ["a1", "a2", "b1"]);
	assert.equal(result.workspaces.length, 2);
	const world = herdr.world();
	const alpha = world.workspaces.find((w) => w.label === "alpha")!;
	const sparTabs = world.tabs.filter((t) => t.label === "spar");
	assert.equal(sparTabs.length, 2);
	const runPanes = world.panes.filter((p) => p.entrypoint === "run");
	assert.deepEqual(runPanes.map((p) => p.env!.SPAR_HERDR_RUN).sort(), ["a1", "a2", "b1"]);
	const alphaRuns = runPanes.filter((p) => p.workspace_id === alpha.workspace_id);
	assert.equal(alphaRuns.length, 2);
	assert.equal(new Set(alphaRuns.map((p) => p.tab_id)).size, 1, "both alpha runs share one tab");
	assert.ok(runPanes.every((p) => p.cwd === p.env!.SPAR_HERDR_ROOT));
	assert.deepEqual(Object.keys(loadState().runs).sort(), ["a1", "a2", "b1"]);
});

test("a second sync creates nothing", async () => {
	installSpar({ "status --all --json": runs() });
	const herdr = installHerdr();
	await syncOnce(config);
	const before = herdr.argv().length;
	const second = await syncOnce(config);
	assert.deepEqual(second.opened, []);
	assert.deepEqual(second.workspaces, []);
	assert.deepEqual(mutations(herdr.argv().slice(before)), []);
});

test("a workspace already open on the project root is reused, not duplicated", async () => {
	installSpar({ "status --all --json": runs() });
	const herdr = installHerdr({
		workspaces: [{ workspace_id: "w1", label: "mine" }],
		tabs: [{ tab_id: "w1:t1", workspace_id: "w1", label: "1" }],
		panes: [{ pane_id: "w1:p1", tab_id: "w1:t1", workspace_id: "w1", cwd: "/p/alpha" }],
	});
	const result = await syncOnce(config);
	assert.equal(result.workspaces.length, 1, "only beta gets a new workspace");
	const creates = herdr.argv().filter((a) => a[0] === "workspace" && a[1] === "create");
	assert.deepEqual(creates.map((a) => a[a.indexOf("--cwd") + 1]), ["/p/beta"]);
	const world = herdr.world();
	const a1 = world.panes.find((p) => p.env?.SPAR_HERDR_RUN === "a1")!;
	assert.equal(a1.workspace_id, "w1");
	assert.equal(world.tabs.find((t) => t.tab_id === a1.tab_id)!.label, "spar");
	assert.ok(world.panes.find((p) => p.pane_id === "w1:p1"), "the user's pane is untouched");
});

test("a run pane found by its tokens is adopted, not reopened", async () => {
	installSpar({ "status --all --json": runs() });
	installHerdr({
		workspaces: [{ workspace_id: "w1", label: "alpha" }],
		tabs: [{ tab_id: "w1:t1", workspace_id: "w1", label: "spar" }],
		panes: [{ pane_id: "w1:p1", tab_id: "w1:t1", workspace_id: "w1", cwd: "/p/alpha", tokens: { spar_run: "a1", spar_root: "/p/alpha" } }],
	});
	const result = await syncOnce(config);
	assert.deepEqual(result.adopted, ["a1"]);
	assert.ok(!result.opened.includes("a1"));
	assert.equal(loadState().runs.a1!.pane_id, "w1:p1");
});

test("a dead mapping is dropped and the run gets a fresh pane", async () => {
	installSpar({ "status --all --json": [runs()[0]!] });
	installHerdr();
	saveState({ runs: { a1: { pane_id: "w9:p9", root: "/p/alpha" } }, workspaces: {} });
	const result = await syncOnce(config);
	assert.deepEqual(result.opened, ["a1"]);
	assert.notEqual(loadState().runs.a1!.pane_id, "w9:p9");
});

test("a pane whose run stopped mattering closes once seen, never while unseen or focused", async () => {
	installSpar({ "status --all --json": [] });
	const pane = (id: string, over: object) => ({ pane_id: id, tab_id: "w1:t1", workspace_id: "w1", cwd: "/p/alpha", ...over });
	const herdr = installHerdr({
		workspaces: [{ workspace_id: "w1", label: "alpha" }],
		tabs: [{ tab_id: "w1:t1", workspace_id: "w1", label: "spar" }],
		panes: [pane("w1:p1", { agent_status: "idle" }), pane("w1:p2", { agent_status: "done" }), pane("w1:p3", { agent_status: "idle", focused: true })],
	});
	saveState({
		runs: { seen: { pane_id: "w1:p1", root: "/p/alpha" }, unseen: { pane_id: "w1:p2", root: "/p/alpha" }, looking: { pane_id: "w1:p3", root: "/p/alpha" } },
		workspaces: {},
	});
	const result = await syncOnce(config);
	assert.deepEqual(result.closed, ["seen"]);
	assert.deepEqual(herdr.world().panes.map((p) => p.pane_id).sort(), ["w1:p2", "w1:p3"]);
});

test("the project allow-list limits which runs get panes", async () => {
	installSpar({ "status --all --json": runs() });
	installHerdr();
	const result = await syncOnce({ ...config, projects: ["beta"] });
	assert.deepEqual(result.opened, ["b1"]);
});

test("startup closes this plugin's panes herdr restored as shells, and only those", async () => {
	installSpar({});
	const pane = (id: string, over: object) => ({ pane_id: id, tab_id: "w1:t1", workspace_id: "w1", ...over });
	const herdr = installHerdr({
		workspaces: [{ workspace_id: "w1", label: "alpha" }],
		tabs: [{ tab_id: "w1:t1", workspace_id: "w1", label: "spar" }],
		panes: [
			pane("w1:p1", { label: "spar run" }),
			pane("w1:p2", { label: "spar board" }),
			pane("w1:p3", { label: "spar run", tokens: { spar_run: "a1", spar_root: "/p/alpha" } }),
			pane("w1:p4", { label: "my shell" }),
		],
	});
	assert.equal(await reapRestored(), 2);
	assert.deepEqual(herdr.world().panes.map((p) => p.pane_id), ["w1:p3", "w1:p4"]);
});
