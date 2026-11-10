import assert from "node:assert/strict";
import { test } from "node:test";
import { actionsFor, buildArgv, isStallPark } from "../src/gates.ts";

const at = (phase: string, over: { holds_seat?: boolean; resumable?: boolean } = {}) =>
	actionsFor({ phase, holds_seat: false, resumable: false, ...over });

test("each gate offers its own verbs", () => {
	assert.deepEqual(at("awaiting_plan_approval"), ["approve", "reject", "rule"]);
	assert.deepEqual(at("awaiting_ship_confirm"), ["ship", "rule"]);
	assert.deepEqual(at("awaiting_winner_confirm"), ["confirm", "reconcile", "rule"]);
	assert.deepEqual(at("awaiting_reconcile"), ["override", "confirm", "reconcile", "rule"]);
	assert.deepEqual(at("awaiting_round_extension", { resumable: true }), ["override", "extend", "rule"]);
});

test("parked runs offer override, resume and reassign where spar accepts them", () => {
	assert.deepEqual(at("failed", { resumable: true }), ["override", "resume", "rule", "reassign"]);
	assert.deepEqual(at("stopped", { resumable: true }), ["resume", "rule", "reassign"]);
	assert.deepEqual(at("plan_approved", { resumable: true }), ["resume", "rule", "reassign"]);
	assert.deepEqual(at("done"), ["rule", "cleanup"]);
});

test("a run holding a seat only offers stop and rule", () => {
	assert.deepEqual(at("dispatch", { holds_seat: true }), ["stop", "rule"]);
	assert.deepEqual(at("awaiting_reconcile", { holds_seat: true }), ["stop", "rule"]);
	assert.deepEqual(at("shipping", { holds_seat: true }), ["rule"]);
});

test("a run below its ceiling is a stall park, which re-enters bare", () => {
	assert.equal(isStallPark({ round: 3, max_rounds: 8 }), true);
	assert.equal(isStallPark({ round: 5, max_rounds: 0 }), true);
	assert.equal(isStallPark({ round: 8, max_rounds: 8 }), false);
	assert.equal(isStallPark({ round: 9, max_rounds: 8 }), false);
	assert.deepEqual(buildArgv("extend", "r1", { rounds: 2 }, { round: 3, max_rounds: 8 }), [
		"implement",
		"--run",
		"r1",
		"--detach",
		"--json",
	]);
	assert.deepEqual(buildArgv("extend", "r1", { rounds: 2 }, { round: 8, max_rounds: 8 }), [
		"implement",
		"--run",
		"r1",
		"--max-rounds",
		"+2",
		"--detach",
		"--json",
	]);
});

test("ship carries --confirm, which the popup's typed yes stands for", () => {
	assert.deepEqual(buildArgv("ship", "r1"), ["ship", "r1", "--confirm", "--json"]);
	assert.deepEqual(buildArgv("override", "r1", { reason: "why" }), ["ship", "r1", "--override-gate", "--reason=why", "--json"]);
	assert.deepEqual(buildArgv("extend", "r1", { rounds: 2 }), ["implement", "--run", "r1", "--max-rounds", "+2", "--detach", "--json"]);
	assert.deepEqual(buildArgv("rule", "r1", { text: "t", waive: ["AC-2"] }), ["rule", "add", "--run", "r1", "--waive", "AC-2", "--json", "--", "t"]);
});
