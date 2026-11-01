import { spawn } from "node:child_process";
import { createInterface } from "node:readline";

// Test-only: drives `pi --mode rpc` as the operator would. Answers each confirm dialog
// with the next value of SPAR_PI_CONFIRMS (e.g. "no,yes"), prints every dialog and every
// spar tool result, and exits when the agent settles.
const [, , ...piArgs] = process.argv;
const answers = (process.env.SPAR_PI_CONFIRMS ?? "").split(",").filter(Boolean);
const pi = spawn("pi", ["--mode", "rpc", ...piArgs], { stdio: ["pipe", "pipe", "inherit"] });
const send = (value: unknown) => pi.stdin.write(`${JSON.stringify(value)}\n`);
const deadline = setTimeout(() => {
	console.log("TIMEOUT");
	pi.kill();
	process.exit(1);
}, 180_000);

createInterface({ input: pi.stdout }).on("line", (line) => {
	let record: Record<string, unknown>;
	try {
		record = JSON.parse(line);
	} catch {
		return;
	}
	if (record.type === "extension_ui_request" && record.method === "confirm") {
		const answer = answers.shift() === "yes";
		console.log(`--- dialog: ${record.title}\n${record.message}\n--- operator answers: ${answer ? "yes" : "no"}`);
		send({ type: "extension_ui_response", id: record.id, confirmed: answer });
	}
	if (record.type === "tool_execution_end" && String(record.toolName).startsWith("spar_")) {
		const result = record.result as { content?: { text?: string }[] };
		console.log(`--- ${record.toolName}${record.isError ? " (error)" : ""}\n${result.content?.[0]?.text ?? ""}`);
	}
	if (record.type === "agent_settled") {
		clearTimeout(deadline);
		pi.stdin.end();
		pi.kill("SIGTERM");
	}
});
pi.on("exit", () => process.exit(0));
send({ id: "prompt-1", type: "prompt", message: "drive spar" });
