import {
	fauxAssistantMessage,
	fauxProvider,
	fauxText,
	fauxToolCall,
	type FauxResponseStep,
	getCurrentSystemPrompt,
	getCurrentTools,
} from "@earendil-works/pi-ai";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

function lastRun(messages: { role: string; details?: unknown }[]): string | undefined {
	for (const m of [...messages].reverse()) {
		const details = m.role === "toolResult" ? (m.details as { outcome?: { run_id?: string } } | undefined) : undefined;
		if (details?.outcome?.run_id) return details.outcome.run_id;
	}
	return undefined;
}

interface Call {
	name: string;
	arguments: Record<string, unknown>;
}

// Test-only: a model that bills nothing and emits the tool calls in SPAR_PI_SCRIPT, one
// assistant turn per step, then echoes every tool result it saw so print mode shows them.
// An argument "$LAST_RUN" becomes the run id of the latest launch result.
export default function (pi: ExtensionAPI) {
	const steps = JSON.parse(process.env.SPAR_PI_SCRIPT ?? "[]") as Call[][];
	const faux = fauxProvider({ provider: "scripted", api: "scripted", models: [{ id: "scripted", name: "Scripted" }] });
	const responses: FauxResponseStep[] = steps.map((calls, step) => (context) => {
		const run = lastRun(context.messages);
		const bind = (args: Record<string, unknown>) =>
			Object.fromEntries(Object.entries(args).map(([k, v]) => [k, v === "$LAST_RUN" ? run : v]));
		return fauxAssistantMessage(
			calls.map((c, i) => fauxToolCall(c.name, bind(c.arguments) as never, { id: `s${step}c${i}` })),
			{ stopReason: "toolUse" },
		);
	});
	responses.push((context) => {
		const results = context.messages.flatMap((m) =>
			m.role === "toolResult"
				? [`## ${m.toolName}${m.isError ? " (error)" : ""}\n${m.content.map((c) => (c.type === "text" ? c.text : "")).join("")}`]
				: [],
		);
		const tools = getCurrentTools(context.messages)
			.map((t) => t.name)
			.filter((n) => n.startsWith("spar_"));
		const skill = /<name>spar<\/name>/.test(getCurrentSystemPrompt(context.messages));
		const header = `spar tools declared: ${tools.join(",")}\nspar skill advertised: ${skill}`;
		return fauxAssistantMessage(fauxText([header, ...results].join("\n\n")));
	});
	faux.setResponses(responses);
	pi.registerProvider(faux.provider);
}
