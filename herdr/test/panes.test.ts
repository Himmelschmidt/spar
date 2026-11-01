import assert from "node:assert/strict";
import { tmpdir } from "node:os";
import { afterEach, test } from "node:test";
import { openActPopup, openSeatLogs } from "../src/panes.ts";
import { cleanup, installHerdr, installSpar, status } from "./helpers.ts";

const ROOT = tmpdir();

afterEach(() => {
	delete process.env.HERDR_PLUGIN_CONTEXT_JSON;
	cleanup();
});

function runPaneWorld() {
	process.env.HERDR_PLUGIN_CONTEXT_JSON = JSON.stringify({ workspace_id: "w1", focused_pane_id: "w1:p1" });
	return installHerdr({
		workspaces: [{ workspace_id: "w1", label: "alpha" }],
		tabs: [{ tab_id: "w1:t1", workspace_id: "w1", label: "spar" }],
		panes: [{ pane_id: "w1:p1", tab_id: "w1:t1", workspace_id: "w1", cwd: ROOT, tokens: { spar_run: "r1", spar_root: ROOT } }],
	});
}

test("seat logs open one pane per current-round seat in a logs tab", async () => {
	installSpar({
		"status r1 --json": status("r1", {
			round: 2,
			slots: [
				{ id: "impl", role: "implementer", provider: "cli:muse", status: "running", round: 2 },
				{ id: "review-0", role: "reviewer", provider: "cli:claude", status: "done", round: 2 },
				{ id: "old", role: "reviewer", provider: "cli:codex", status: "done", round: 1 },
			],
		}),
	});
	const herdr = runPaneWorld();
	await openSeatLogs();
	const world = herdr.world();
	const logs = world.panes.filter((p) => p.entrypoint === "logs");
	assert.deepEqual(logs.map((p) => p.env!.SPAR_HERDR_SLOT), ["impl", "review-0"]);
	assert.equal(new Set(logs.map((p) => p.tab_id)).size, 1);
	assert.equal(world.tabs.find((t) => t.tab_id === logs[0]!.tab_id)!.label, "logs r1");
});

test("seat logs refuse a pane that is not a run pane", async () => {
	installSpar({});
	process.env.HERDR_PLUGIN_CONTEXT_JSON = JSON.stringify({ focused_pane_id: "w1:p1" });
	installHerdr({ workspaces: [], tabs: [], panes: [{ pane_id: "w1:p1", tab_id: "w1:t1", workspace_id: "w1" }] });
	await assert.rejects(openSeatLogs(), /not a spar run pane/);
});

test("act opens the popup entrypoint for the focused run", async () => {
	installSpar({});
	const herdr = runPaneWorld();
	await openActPopup();
	assert.deepEqual(herdr.world().popups, [{ entrypoint: "act", env: { SPAR_HERDR_RUN: "r1", SPAR_HERDR_ROOT: ROOT } }]);
});
