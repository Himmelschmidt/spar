#!/usr/bin/env node
const fs = require("node:fs");
const path = require("node:path");

const dir = process.env.FAKE_SPAR_DIR;
const args = process.argv.slice(2);
const line = args.join(" ");
fs.appendFileSync(path.join(dir, "calls.log"), `${line}\n`);
fs.appendFileSync(path.join(dir, "argv.jsonl"), `${JSON.stringify(args)}\n`);
const responses = JSON.parse(fs.readFileSync(path.join(dir, "responses.json"), "utf8"));

if (args[0] === "events") {
	const previous = fs
		.readFileSync(path.join(dir, "calls.log"), "utf8")
		.split("\n")
		.filter((l) => l.startsWith("events")).length - 1;
	const sessions = responses.events ?? [];
	const session = sessions[Math.min(previous, sessions.length - 1)] ?? { lines: [] };
	fs.writeFileSync(path.join(dir, `events-${previous}.pid`), String(process.pid));
	for (const l of session.lines) process.stdout.write(`${typeof l === "string" ? l : JSON.stringify(l)}\n`);
	if (session.hold) setInterval(() => {}, 1000);
	return;
}

const key = Object.keys(responses)
	.filter((k) => k !== "events" && (line === k || line.startsWith(`${k} `)))
	.sort((a, b) => b.length - a.length)[0];
if (key === undefined) {
	process.stderr.write(`error: no fake response for "${line}"\n`);
	process.exit(2);
}
let value = responses[key];
if (value && typeof value === "object" && Array.isArray(value.__seq)) {
	const n = fs
		.readFileSync(path.join(dir, "calls.log"), "utf8")
		.split("\n")
		.filter((l) => l === line).length - 1;
	value = value.__seq[Math.min(n, value.__seq.length - 1)];
}
if (value && typeof value === "object" && value.__hold) {
	if (value.__hold === "ignore-term") process.on("SIGTERM", () => {});
	setInterval(() => {}, 1000);
	return;
}
if (value && typeof value === "object" && "__exit" in value) {
	if ("__stdout" in value) process.stdout.write(JSON.stringify(value.__stdout));
	else process.stderr.write(`${value.__stderr ?? "error"}\n`);
	process.exit(value.__exit);
}
if (value && typeof value === "object" && "__text" in value) {
	process.stdout.write(value.__text);
	return;
}
process.stdout.write(JSON.stringify(value));
