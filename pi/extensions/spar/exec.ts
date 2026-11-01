import { spawn } from "node:child_process";
import { sparBin } from "./spar.ts";

export interface SparRun {
	code: number | null;
	stdout: string;
	stderr: string;
	missing: boolean;
	aborted: boolean;
	timedOut: boolean;
	truncated: boolean;
}

export interface RunOptions {
	signal?: AbortSignal;
	timeoutMs?: number;
	maxBytes?: number;
	killGraceMs?: number;
}

const STDERR_CAP = 64 * 1024;

export function runSpar(args: string[], cwd: string, opts: RunOptions = {}): Promise<SparRun> {
	const maxBytes = opts.maxBytes ?? 16 * 1024 * 1024;
	return new Promise((resolve) => {
		if (opts.signal?.aborted) {
			resolve({ code: null, stdout: "", stderr: "", missing: false, aborted: true, timedOut: false, truncated: false });
			return;
		}
		const child = spawn(sparBin(), args, { cwd, stdio: ["ignore", "pipe", "pipe"], windowsHide: true });
		let stdout = "";
		let stderr = "";
		let truncated = false;
		let aborted = false;
		let timedOut = false;
		let missing = false;
		child.stdout.setEncoding("utf8");
		child.stderr.setEncoding("utf8");
		child.stdout.on("data", (chunk: string) => {
			stdout += chunk;
			if (stdout.length > maxBytes) {
				stdout = stdout.slice(stdout.length - maxBytes);
				truncated = true;
			}
		});
		child.stderr.on("data", (chunk: string) => {
			if (stderr.length < STDERR_CAP) stderr += chunk;
		});
		let grace: NodeJS.Timeout | undefined;
		const kill = () => {
			if (child.exitCode !== null || child.signalCode !== null || grace) return;
			child.kill("SIGTERM");
			grace = setTimeout(() => {
				if (child.exitCode === null && child.signalCode === null) child.kill("SIGKILL");
			}, opts.killGraceMs ?? 3_000);
		};
		const onAbort = () => {
			aborted = true;
			kill();
		};
		opts.signal?.addEventListener("abort", onAbort, { once: true });
		const timer = opts.timeoutMs
			? setTimeout(() => {
					timedOut = true;
					kill();
				}, opts.timeoutMs)
			: undefined;
		child.on("error", (err: NodeJS.ErrnoException) => {
			if (err.code === "ENOENT") missing = true;
			else stderr += err.message;
		});
		child.on("close", (code) => {
			if (timer) clearTimeout(timer);
			if (grace) clearTimeout(grace);
			opts.signal?.removeEventListener("abort", onAbort);
			resolve({ code, stdout, stderr, missing, aborted, timedOut, truncated });
		});
	});
}

// Escape sequences and C0/C1 controls from spar's output must not reach the terminal or
// the model: a log line or task could otherwise repaint the screen or hide text.
const ESCAPES = /\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)?|\x1b[PX^_][^\x1b]*(?:\x1b\\)?|\x1b[\s\S]?/g;
const CONTROLS = /[\x00-\x08\x0b-\x1f\x7f-\x9f]/g;

export function clean(text: string): string {
	return text.replace(ESCAPES, "").replace(CONTROLS, "");
}

export function cleanDeep<T>(value: T): T {
	if (typeof value === "string") return clean(value) as T;
	if (Array.isArray(value)) return value.map(cleanDeep) as T;
	if (value && typeof value === "object") {
		return Object.fromEntries(Object.entries(value).map(([k, v]) => [k, cleanDeep(v)])) as T;
	}
	return value;
}

export function hasControls(text: string): boolean {
	return /[\x00-\x08\x0b-\x1f\x7f-\x9f]/.test(text);
}

export function firstLine(text: string): string {
	return clean(text).trim().split("\n")[0] ?? "";
}

export function parseJson<T>(text: string): T | undefined {
	try {
		return cleanDeep(JSON.parse(text) as T);
	} catch {
		return undefined;
	}
}

export function jsonLines<T>(text: string): T[] {
	const out: T[] = [];
	for (const line of text.split("\n")) {
		if (!line.trim()) continue;
		const value = parseJson<T>(line);
		if (value !== undefined) out.push(value);
	}
	return out;
}
