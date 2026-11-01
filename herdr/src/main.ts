import { act, terminalIo } from "./act.ts";
import { loadConfig } from "./config.ts";
import { notify } from "./herdr.ts";
import { ensureBoard, followLog, openActPopup, openSeatLogs, runBoard } from "./panes.ts";
import { describeSync, reapRestored, sync } from "./sync.ts";
import { runView } from "./view.ts";

function env(name: string): string {
	const value = process.env[name];
	if (!value) throw new Error(`${name} is not set`);
	return value;
}

async function main(command: string | undefined): Promise<void> {
	const config = loadConfig();
	switch (command) {
		case "sync": {
			const text = describeSync(await sync(config));
			process.stdout.write(`${text}\n`);
			await notify("spar sync", text.replace(/^spar sync: /, ""));
			return;
		}
		case "startup":
			process.stdout.write(`spar startup: closed ${await reapRestored()} restored pane(s)\n`);
			process.stdout.write(`${describeSync(await sync(config))}\n`);
			if (config.board_on_startup) await ensureBoard();
			return;
		case "board-open":
			return ensureBoard();
		case "board":
			return runBoard(config);
		case "run":
			return runView(env("SPAR_HERDR_RUN"), env("SPAR_HERDR_ROOT"));
		case "act-open":
			return openActPopup();
		case "act":
			await act(terminalIo(), config, process.env.SPAR_HERDR_RUN, process.env.SPAR_HERDR_ROOT);
			process.exit(0);
		case "logs-open": {
			const text = await openSeatLogs();
			process.stdout.write(`${text}\n`);
			return;
		}
		case "logs":
			return followLog(env("SPAR_HERDR_RUN"), env("SPAR_HERDR_ROOT"), env("SPAR_HERDR_SLOT"));
		default:
			throw new Error(`unknown command "${command ?? ""}"; expected sync, startup, board, board-open, run, act, act-open, logs or logs-open`);
	}
}

main(process.argv[2]).catch(async (error: Error) => {
	process.stderr.write(`spar monitor: ${error.message}\n`);
	if (process.env.HERDR_PLUGIN_ACTION_ID) await notify("spar", error.message);
	process.exit(1);
});
