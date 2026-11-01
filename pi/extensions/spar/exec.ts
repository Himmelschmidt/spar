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
		const kill = () => {
			if (child.exitCode === null && child.signalCode === null) child.kill("SIGTERM");
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
			opts.signal?.removeEventListener("abort", onAbort);
			resolve({ code, stdout, stderr, missing, aborted, timedOut, truncated });
		});
	});
}

export function firstLine(text: string): string {
	return text.trim().split("\n")[0] ?? "";
}

export function parseJson<T>(text: string): T | undefined {
	try {
		return JSON.parse(text) as T;
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
