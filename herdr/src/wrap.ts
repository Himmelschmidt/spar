const ANSI = /\x1b\[[0-?]*[ -/]*[@-~]/g;

// Terminal columns one code point takes: 0 for combining marks and zero-width joiners, 2
// for East Asian wide and fullwidth forms and most emoji, else 1.
export function charWidth(cp: number): number {
	if ((cp >= 0x300 && cp <= 0x36f) || cp === 0x200b || cp === 0x200d || (cp >= 0xfe00 && cp <= 0xfe0f)) return 0;
	if (
		(cp >= 0x1100 && cp <= 0x115f) ||
		(cp >= 0x2e80 && cp <= 0x303e) ||
		(cp >= 0x3041 && cp <= 0x33ff) ||
		(cp >= 0x3400 && cp <= 0x4dbf) ||
		(cp >= 0x4e00 && cp <= 0x9fff) ||
		(cp >= 0xa000 && cp <= 0xa4cf) ||
		(cp >= 0xac00 && cp <= 0xd7a3) ||
		(cp >= 0xf900 && cp <= 0xfaff) ||
		(cp >= 0xfe30 && cp <= 0xfe4f) ||
		(cp >= 0xff00 && cp <= 0xff60) ||
		(cp >= 0xffe0 && cp <= 0xffe6) ||
		(cp >= 0x1f300 && cp <= 0x1f64f) ||
		(cp >= 0x1f900 && cp <= 0x1f9ff) ||
		(cp >= 0x20000 && cp <= 0x3fffd)
	) {
		return 2;
	}
	return 1;
}

export function displayWidth(text: string): number {
	let width = 0;
	for (const ch of text.replace(ANSI, "")) width += charWidth(ch.codePointAt(0)!);
	return width;
}

// Word-wraps one logical line to `width` terminal columns, keeping its leading spaces.
// Continuation lines are indented `indent` further; a word wider than the room left is
// cut, so no output line is ever wider than `width` (a lone wide character in a
// one-column room is the only exception).
export function wrap(text: string, width: number, indent = 2): string[] {
	const room = Math.max(1, width);
	const lead = /^ */.exec(text)![0];
	const first = lead.slice(0, Math.floor(room / 2));
	const pad = (lead + " ".repeat(indent)).slice(0, Math.floor(room / 2));
	const lines: string[] = [];
	let line = first;
	let used = first.length;
	let empty = true;
	const push = () => {
		lines.push(line.trimEnd());
		line = pad;
		used = pad.length;
		empty = true;
	};
	for (const word of text.split(/\s+/).filter(Boolean)) {
		const sep = empty ? 0 : 1;
		const w = displayWidth(word);
		if (used + sep + w <= room) {
			line += (sep ? " " : "") + word;
			used += sep + w;
			empty = false;
			continue;
		}
		if (!empty) push();
		for (const ch of word) {
			const cw = charWidth(ch.codePointAt(0)!);
			if (used + cw > room && !empty) push();
			line += ch;
			used += cw;
			empty = false;
		}
	}
	if (!empty || lines.length === 0) lines.push(line.trimEnd());
	return lines;
}
