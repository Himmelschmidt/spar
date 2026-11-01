import { spawn } from "node:child_process";
import { createInterface } from "node:readline";

// Test-only: drives `pi --mode rpc` as the operator would. Answers each select dialog
// with the next value of SPAR_PI_ANSWERS (e.g. "Cancel,Launch"), prints every dialog and
// every tool result, and exits when the agent settles.
const [, , ...piArgs] = process.argv;
const answers = (process.env.SPAR_PI_ANSWERS ?? "").split(",").filter(Boolean);
const pi = spawn(process.env.PI_BIN ?? "pi", ["--mode", "rpc", ...piArgs], { stdio: ["pipe", "pipe", "inherit"] });
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
	if (record.type === "extension_ui_request" && record.method === "select") {
		const answer = answers.shift();
		console.log(`--- dialog: ${record.title}\n--- options: ${(record.options as string[]).join(" | ")}\n--- operator answers: ${answer ?? "(nothing)"}`);
		send({ type: "extension_ui_response", id: record.id, ...(answer ? { value: answer } : { cancelled: true }) });
	}
	if (record.type === "tool_execution_end") {
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
