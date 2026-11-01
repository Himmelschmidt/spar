// Word-wraps one logical line to `width` columns, keeping its leading spaces. Continuation
// lines are indented `indent` further; a word longer than the room left is cut, so no
// output line is ever wider than `width`.
export function wrap(text: string, width: number, indent = 2): string[] {
	const room = Math.max(1, width);
	const lead = /^ */.exec(text)![0];
	const first = lead.slice(0, Math.floor(room / 2));
	const pad = (lead + " ".repeat(indent)).slice(0, Math.floor(room / 2));
	const lines: string[] = [];
	let line = first;
	let empty = true;
	const push = () => {
		lines.push(line.trimEnd());
		line = pad;
		empty = true;
	};
	for (const word of text.split(/\s+/).filter(Boolean)) {
		const sep = empty ? "" : " ";
		if (line.length + sep.length + word.length <= room) {
			line += sep + word;
			empty = false;
			continue;
		}
		if (!empty) push();
		let rest = word;
		while (line.length + rest.length > room) {
			const take = room - line.length;
			line += rest.slice(0, take);
			rest = rest.slice(take);
			push();
		}
		if (rest) {
			line += rest;
			empty = false;
		}
	}
	if (!empty || lines.length === 0) lines.push(line.trimEnd());
	return lines;
}
