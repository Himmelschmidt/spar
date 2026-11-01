import type { ExtensionContext, Theme } from "@earendil-works/pi-coding-agent";
import { DynamicBorder, getSelectListTheme } from "@earendil-works/pi-coding-agent";
import { Container, Key, matchesKey, SelectList, Text, truncateToWidth } from "@earendil-works/pi-tui";
import type { ListRow, Paint } from "./format.ts";

export function themePaint(theme: Theme): Paint {
	return (tone, text) => theme.fg(tone, text);
}

const HEIGHT = 0.85;
const OVERLAY = { overlay: true, overlayOptions: { width: "90%", maxHeight: "85%", anchor: "center" } } as const;

function overlayRows(terminalRows: number): number {
	return Math.floor(terminalRows * HEIGHT);
}

export function pickRun(
	ctx: ExtensionContext,
	rows: ListRow[],
	waiting: number,
	stale: number,
): Promise<string | undefined> {
	return ctx.ui.custom<string | undefined>((tui, theme, _keybindings, done) => {
		const container = new Container();
		const border = new DynamicBorder((s) => theme.fg("accent", s));
		const title = `spar runs: ${rows.length}${waiting ? `, ${waiting} need you` : ""}${stale ? `, ${stale} stale` : ""}`;
		const labelWidth = Math.max(...rows.map((r) => r.label.length)) + 2;
		const list = new SelectList(
			rows.map((r) => ({ value: r.id, label: r.label, description: r.description })),
			// Borders, title, hint and the list's own scroll line.
			Math.max(3, Math.min(rows.length, overlayRows(tui.terminal.rows) - 5)),
			getSelectListTheme(),
			{ minPrimaryColumnWidth: labelWidth, maxPrimaryColumnWidth: labelWidth },
		);
		list.onSelect = (item) => done(item.value);
		list.onCancel = () => done(undefined);
		container.addChild(border);
		container.addChild(new Text(theme.fg("accent", theme.bold(title))));
		container.addChild(list);
		container.addChild(new Text(theme.fg("dim", "up/down move, enter opens the run, esc closes")));
		container.addChild(new DynamicBorder((s) => theme.fg("accent", s)));
		return {
			render: (width: number) => container.render(width),
			invalidate: () => container.invalidate(),
			handleInput: (data: string) => {
				list.handleInput(data);
				tui.requestRender();
			},
		};
	}, OVERLAY);
}

export type DetailResult = "back" | "close";

export function showDetail(ctx: ExtensionContext, build: (paint: Paint) => string[]): Promise<DetailResult> {
	return ctx.ui.custom<DetailResult>((tui, theme, keybindings, done) => {
		const lines = build(themePaint(theme));
		let top = 0;
		const viewport = () => Math.max(3, overlayRows(tui.terminal.rows) - 3);
		const maxTop = () => Math.max(0, lines.length - viewport());
		return {
			render(width: number): string[] {
				const height = viewport();
				const rule = theme.fg("accent", "-".repeat(width));
				const shown = lines.slice(top, top + height).map((l) => truncateToWidth(l, width));
				const more = lines.length > height ? ` ${top + 1}-${Math.min(lines.length, top + height)} of ${lines.length},` : "";
				return [
					rule,
					...shown,
					truncateToWidth(theme.fg("dim", `${more} up/down scroll, pgup/pgdn page, b back, esc closes`), width),
					rule,
				];
			},
			invalidate() {},
			handleInput(data: string): void {
				const page = viewport();
				if (keybindings.matches(data, "tui.select.cancel") || matchesKey(data, Key.escape) || matchesKey(data, "q")) {
					return done("close");
				}
				if (matchesKey(data, "b")) return done("back");
				if (keybindings.matches(data, "tui.select.up")) top = Math.max(0, top - 1);
				else if (keybindings.matches(data, "tui.select.down")) top = Math.min(maxTop(), top + 1);
				else if (keybindings.matches(data, "tui.select.pageUp")) top = Math.max(0, top - page);
				else if (keybindings.matches(data, "tui.select.pageDown")) top = Math.min(maxTop(), top + page);
				else return;
				tui.requestRender();
			},
		};
	}, OVERLAY);
}
