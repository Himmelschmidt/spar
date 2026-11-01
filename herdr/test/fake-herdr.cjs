#!/usr/bin/env node
// A stand-in for the herdr CLI: records argv and keeps a small workspace/tab/pane world in
// world.json so sync can be run twice against the same state.
const fs = require("node:fs");
const path = require("node:path");

const dir = process.env.FAKE_HERDR_DIR;
const args = process.argv.slice(2);
fs.appendFileSync(path.join(dir, "argv.jsonl"), `${JSON.stringify(args)}\n`);
const worldPath = path.join(dir, "world.json");
const world = JSON.parse(fs.readFileSync(worldPath, "utf8"));
world.next ??= 100;

const opt = (name) => {
	const i = args.indexOf(name);
	return i >= 0 ? args[i + 1] : undefined;
};
const opts = (name) => args.flatMap((a, i) => (a === name ? [args[i + 1]] : []));
const save = () => fs.writeFileSync(worldPath, JSON.stringify(world));
const ok = (result) => {
	save();
	process.stdout.write(JSON.stringify({ id: "cli", result }));
	process.exit(0);
};
const fail = (message) => {
	process.stderr.write(JSON.stringify({ error: { message } }));
	process.exit(1);
};
const pane = (id) => world.panes.find((p) => p.pane_id === id);
const layout = (tab) => ({
	tab_id: tab,
	panes: world.panes.filter((p) => p.tab_id === tab).map((p) => ({ pane_id: p.pane_id, rect: { width: 100, height: 30 } })),
});
const newTab = (workspace, label) => {
	const tab = { tab_id: `${workspace}:t${world.next++}`, workspace_id: workspace, label };
	world.tabs.push(tab);
	return tab;
};
const newPane = (tab, cwd, extra = {}) => {
	const p = { pane_id: `${tab.workspace_id}:p${world.next++}`, tab_id: tab.tab_id, workspace_id: tab.workspace_id, cwd, agent_status: "unknown", ...extra };
	world.panes.push(p);
	return p;
};

const cmd = args.slice(0, 2).join(" ");
if (world.fail && world.fail[cmd]) fail(world.fail[cmd]);
switch (cmd) {
	case "api snapshot":
		ok({ snapshot: { workspaces: world.workspaces, tabs: world.tabs, panes: world.panes, layouts: [...new Set(world.panes.map((p) => p.tab_id))].map(layout), focused_workspace_id: world.workspaces[0]?.workspace_id } });
	case "workspace create": {
		const ws = { workspace_id: `w${world.next++}`, label: opt("--label") ?? "ws" };
		world.workspaces.push(ws);
		const tab = newTab(ws.workspace_id, "1");
		const root = newPane(tab, opt("--cwd"));
		ok({ workspace: ws, tab, root_pane: root });
	}
	case "tab rename": {
		const tab = world.tabs.find((t) => t.tab_id === args[2]);
		if (!tab) fail("tab_not_found");
		tab.label = args.slice(3).join(" ");
		ok({ type: "ok" });
	}
	case "plugin pane": {
		const env = Object.fromEntries(opts("--env").map((e) => [e.slice(0, e.indexOf("=")), e.slice(e.indexOf("=") + 1)]));
		const titles = { run: "spar run", board: "spar board", logs: "spar seat log" };
		const extra = { entrypoint: opt("--entrypoint"), label: titles[opt("--entrypoint")], env };
		const placement = opt("--placement");
		if (!placement) {
			world.popups = [...(world.popups ?? []), extra];
			ok({ type: "ok" });
		}
		if (placement === "tab") {
			const p = newPane(newTab(opt("--workspace"), "x"), opt("--cwd"), extra);
			ok({ plugin_pane: { entrypoint: extra.entrypoint, pane: p } });
		}
		const target = pane(opt("--target-pane"));
		if (!target) fail("pane_not_found");
		const p = newPane(world.tabs.find((t) => t.tab_id === target.tab_id), opt("--cwd"), extra);
		ok({ plugin_pane: { entrypoint: extra.entrypoint, pane: p } });
	}
	case "pane layout": {
		const p = pane(opt("--pane"));
		if (!p) fail("pane_not_found");
		ok({ layout: layout(p.tab_id) });
	}
	case "pane get": {
		const p = pane(args[2]);
		if (!p) fail("pane_not_found");
		ok({ pane: p });
	}
	case "pane close":
		world.panes = world.panes.filter((p) => p.pane_id !== args[2]);
		ok({ type: "ok" });
	case "pane report-agent": {
		const p = pane(args[2]);
		if (!p) fail("pane_not_found");
		p.agent = opt("--agent");
		p.agent_status = opt("--state");
		ok({ type: "ok" });
	}
	case "pane report-metadata": {
		const p = pane(args[2]);
		if (!p) fail("pane_not_found");
		// herdr cuts token values at 80 characters.
		p.tokens = { ...(p.tokens ?? {}), ...Object.fromEntries(opts("--token").map((t) => [t.slice(0, t.indexOf("=")), t.slice(t.indexOf("=") + 1, t.indexOf("=") + 81)])) };
		ok({ type: "ok" });
	}
	default:
		ok({ type: "ok" });
}
