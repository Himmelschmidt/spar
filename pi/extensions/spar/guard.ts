type Token = { word: string; quoted: boolean } | { op: string };

const OPS = ["&&", "||", "$(", "<(", ">(", ";;", ";", "|&", "|", "&", "\n", "(", ")", "`"];
const REDIRECT = /^(&>>|&>|>>|>&|>\||<<<|<<|<&|<>|>|<)/;

function lex(command: string): Token[] {
	const out: Token[] = [];
	let word = "";
	let quoted = false;
	let inWord = false;
	const flush = () => {
		if (inWord) out.push({ word, quoted });
		word = "";
		quoted = false;
		inWord = false;
	};
	for (let i = 0; i < command.length; i++) {
		const c = command[i]!;
		if (c === "\\" && i + 1 < command.length) {
			if (command[i + 1] !== "\n") {
				word += command[i + 1];
				inWord = true;
			}
			i++;
			continue;
		}
		if (c === "'" || c === '"') {
			const end = c === "'" ? command.indexOf("'", i + 1) : closingDouble(command, i + 1);
			const stop = end === -1 ? command.length : end;
			const body = command.slice(i + 1, stop);
			word += c === '"' ? body.replace(/\\(["\\$`])/g, "$1") : body;
			quoted = true;
			inWord = true;
			i = stop;
			continue;
		}
		if (c === "#" && !inWord) {
			const nl = command.indexOf("\n", i);
			if (nl === -1) break;
			i = nl - 1;
			continue;
		}
		if (c === " " || c === "\t") {
			flush();
			continue;
		}
		const op = OPS.find((o) => command.startsWith(o, i));
		if (op === "<(" || op === ">(" || !(c === ">" || c === "<" || command.startsWith("&>", i))) {
			if (op) {
				flush();
				out.push({ op });
				i += op.length - 1;
				continue;
			}
		} else {
			if (inWord && !/^[0-9]+$/.test(word)) flush();
			word = "";
			quoted = false;
			inWord = false;
			out.push({ op: "redirect" });
			i += REDIRECT.exec(command.slice(i))![0].length - 1;
			continue;
		}
		word += c;
		inWord = true;
	}
	flush();
	return out;
}

function closingDouble(command: string, from: number): number {
	for (let i = from; i < command.length; i++) {
		if (command[i] === "\\") i++;
		else if (command[i] === '"') return i;
	}
	return -1;
}

const WRAPPERS = new Set(["env", "sudo", "exec", "command", "builtin", "nohup", "time", "nice", "xargs", "stdbuf", "setsid", "doas"]);
const WRAPPER_VALUES: Record<string, Set<string>> = {
	env: new Set(["-u", "-C", "-S", "--unset", "--chdir"]),
	sudo: new Set(["-u", "-g", "-C", "-D", "-h", "-p", "-r", "-t", "-U"]),
	doas: new Set(["-u", "-C"]),
	timeout: new Set(["-s", "-k", "--signal", "--kill-after"]),
	nice: new Set(["-n"]),
	xargs: new Set(["-I", "-n", "-P", "-d", "-E", "-L", "-s", "-a"]),
	stdbuf: new Set(["-i", "-o", "-e"]),
};
const KEYWORDS = new Set(["then", "do", "else", "elif", "if", "while", "until", "!", "{", "}"]);

function isSpar(word: string): boolean {
	return word === "spar" || word.endsWith("/spar");
}

// Every `spar ...` the command line would run, as argv after `spar`. Heuristic: it reads
// the command positions a shell would (after ;, &&, ||, |, &, (, $(, backticks, env
// assignments and common wrappers) and looks inside quoted strings that mention spar
// (bash -c "..."). It cannot see through variables, aliases, functions or scripts.
export function sparInvocations(command: string, depth = 0): string[][] {
	const tokens = lex(command);
	const found: string[][] = [];
	let atCommand = true;
	let skipNext = false;
	let wrapper = "";
	for (let i = 0; i < tokens.length; i++) {
		const t = tokens[i]!;
		if ("op" in t) {
			if (t.op === "redirect") skipNext = true;
			else {
				atCommand = true;
				wrapper = "";
			}
			continue;
		}
		if (depth < 3 && t.quoted && /spar/.test(t.word)) found.push(...sparInvocations(t.word, depth + 1));
		if (skipNext) {
			skipNext = false;
			continue;
		}
		if (!atCommand) continue;
		if (/^[A-Za-z_][A-Za-z0-9_]*=/.test(t.word) || KEYWORDS.has(t.word)) continue;
		if (wrapper && t.word.startsWith("-")) {
			if (WRAPPER_VALUES[wrapper]?.has(t.word)) skipNext = true;
			continue;
		}
		if (wrapper === "timeout" && /^[0-9.]+[smhd]?$/.test(t.word)) continue;
		if (WRAPPERS.has(t.word) || t.word === "timeout") {
			wrapper = t.word;
			continue;
		}
		atCommand = false;
		wrapper = "";
		if (!isSpar(t.word)) continue;
		const args: string[] = [];
		for (let j = i + 1; j < tokens.length; j++) {
			const a = tokens[j]!;
			if ("op" in a) {
				if (a.op === "redirect") {
					j++;
					continue;
				}
				break;
			}
			args.push(a.word);
		}
		found.push(args);
	}
	return found;
}

const READ_ONLY = new Set(["status", "brief", "events", "logs", "stats", "wait", "doctor", "skills", "help", "attach", "dashboard"]);
const READ_ONLY_PAIRS: Record<string, Set<string>> = {
	provider: new Set(["list"]),
	model: new Set(["list", "pick", "cache"]),
	bus: new Set(["log", "presence", "inbox"]),
	rule: new Set(["list"]),
	daemon: new Set(["status"]),
};
const LAUNCH = new Set(["plan", "implement", "run"]);
const GLOBAL_WITH_VALUE = new Set(["--cwd", "--task"]);

export type SparKind = "read" | "launch" | "operator";

export function classify(args: string[]): SparKind {
	const words: string[] = [];
	for (let i = 0; i < args.length; i++) {
		const a = args[i]!;
		if (words.length === 0 && GLOBAL_WITH_VALUE.has(a)) {
			i++;
			continue;
		}
		if (a.startsWith("-")) continue;
		words.push(a);
		if (words.length === 2) break;
	}
	const [sub, action] = words;
	if (sub === undefined) {
		return args.some((a) => a === "-h" || a === "--help" || a === "-V" || a === "--version") || args.length === 0 ? "read" : "operator";
	}
	if (args.includes("--help") || args.includes("-h")) return "read";
	if (READ_ONLY.has(sub)) return "read";
	const pair = READ_ONLY_PAIRS[sub];
	if (pair && action !== undefined && pair.has(action)) return sub === "bus" && args.includes("--claim") ? "operator" : "read";
	if (LAUNCH.has(sub)) {
		if (args.includes("--validate-only")) return "read";
		const gated = args.some((a) => /^--(max-rounds|reassign|reload-config|accept-contract|override-gate)(=|$)/.test(a));
		return gated ? "operator" : "launch";
	}
	return "operator";
}

export interface BashVerdict {
	kind: "none" | "launch" | "operator";
	commands: string[][];
}

export function judgeBash(command: string): BashVerdict {
	const commands = sparInvocations(command).filter((args) => classify(args) !== "read");
	if (commands.length === 0) return { kind: "none", commands };
	return { kind: commands.some((c) => classify(c) === "operator") ? "operator" : "launch", commands };
}
